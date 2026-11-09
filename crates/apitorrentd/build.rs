//! With the `first-party-plugins` feature, embeds the first-party plugin bundles
//! (`dist/first-party-plugins/*.plugin.json`, or `APITORRENT_PLUGIN_DIST`) so the daemon
//! can offer them at startup. Without it, there are none.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=APITORRENT_PLUGIN_DIST");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("bundled.rs");
    if std::env::var_os("CARGO_FEATURE_FIRST_PARTY_PLUGINS").is_none() {
        std::fs::write(&out, "static BUNDLES: &[&str] = &[];\n").unwrap();
        return;
    }
    let dist = std::env::var_os("APITORRENT_PLUGIN_DIST").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist/first-party-plugins"),
        PathBuf::from,
    );
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dist)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with(".plugin.json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    assert!(
        !files.is_empty(),
        "the `first-party-plugins` feature needs the built bundles in {}: run `bun install && \
         bun run build:first-party` at the repository root, or point APITORRENT_PLUGIN_DIST at \
         them",
        dist.display()
    );
    let mut code = String::from("static BUNDLES: &[&str] = &[\n");
    for f in files {
        writeln!(code, "    include_str!({:?}),", f.display().to_string()).unwrap();
    }
    code.push_str("];\n");
    std::fs::write(&out, code).unwrap();
}
