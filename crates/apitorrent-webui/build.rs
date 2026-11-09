//! With the `embed` feature, packs the built web UI into the binary: each file raw, plus
//! brotli and gzip copies of the compressible ones, with content types and entity tags
//! worked out here rather than at run time.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-env-changed=APITORRENT_WEB_DIST");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let generated = out.join("bundle.rs");
    if std::env::var_os("CARGO_FEATURE_EMBED").is_none() {
        std::fs::write(&generated, "static FILES: Option<&[Asset]> = None;\n").unwrap();
        return;
    }

    let dist = std::env::var_os("APITORRENT_WEB_DIST").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist"),
        PathBuf::from,
    );
    // A directory is checked recursively, so a rebuilt UI re-runs this.
    println!("cargo:rerun-if-changed={}", dist.display());
    assert!(
        dist.join("index.html").is_file(),
        "the `embed` feature needs a built web UI at {}: run `bun install && bun run build` \
         in web/, or point APITORRENT_WEB_DIST at one",
        dist.display()
    );

    let mut files = Vec::new();
    collect(&dist, &dist, &mut files);
    files.sort();

    let mut code = String::from("static FILES: Option<&[Asset]> = Some(&[\n");
    for (i, (url, path)) in files.iter().enumerate() {
        let raw = std::fs::read(path).unwrap();
        let etag = hex::encode(&Sha256::digest(&raw)[..12]);
        let content_type = content_type(url);
        let (mut br, mut gzip) = (None, None);
        if compressible(content_type) {
            let b = brotli_bytes(&raw);
            if worth_it(b.len(), raw.len()) {
                let p = out.join(format!("{i}.br"));
                std::fs::write(&p, b).unwrap();
                br = Some(p);
            }
            let g = gzip_bytes(&raw);
            if worth_it(g.len(), raw.len()) {
                let p = out.join(format!("{i}.gz"));
                std::fs::write(&p, g).unwrap();
                gzip = Some(p);
            }
        }
        let include = |p: &Option<PathBuf>| {
            p.as_ref().map_or_else(
                || "None".to_owned(),
                |p| format!("Some(include_bytes!({:?}))", p.display().to_string()),
            )
        };
        writeln!(
            code,
            "    Asset {{ path: {url:?}, content_type: {content_type:?}, etag: {etag:?}, \
             immutable: {immutable}, raw: include_bytes!({raw_path:?}), br: {br}, gzip: {gzip} }},",
            immutable = url.starts_with("/assets/"),
            raw_path = path.display().to_string(),
            br = include(&br),
            gzip = include(&gzip),
        )
        .unwrap();
    }
    code.push_str("]);\n");
    std::fs::write(&generated, code).unwrap();
}

/// Every file under `dir` as (URL path, file path). Source maps stay out: they are large and
/// only help someone debugging, who can use `--web-root` instead.
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(root, &path, files);
            continue;
        }
        if path.extension().is_some_and(|e| e == "map") {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap();
        let mut url = String::new();
        for part in rel.components() {
            url.push('/');
            url.push_str(
                part.as_os_str()
                    .to_str()
                    .expect("web UI file names are UTF-8"),
            );
        }
        files.push((url, path));
    }
}

fn content_type(url: &str) -> &'static str {
    let ext = url.rsplit_once('.').map_or("", |(_, e)| e);
    match ext.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn compressible(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type,
            "application/json"
                | "application/manifest+json"
                | "image/svg+xml"
                | "image/x-icon"
                | "font/ttf"
                | "application/wasm"
        )
}

/// A compressed copy is kept only if it saves at least a tenth.
fn worth_it(compressed: usize, raw: usize) -> bool {
    compressed * 10 <= raw * 9
}

fn brotli_bytes(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 4096, 11, 22);
        w.write_all(raw).unwrap();
    }
    out
}

fn gzip_bytes(raw: &[u8]) -> Vec<u8> {
    let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    w.write_all(raw).unwrap();
    w.finish().unwrap()
}
