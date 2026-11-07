//! Make a `.torrent` from a file or directory.
//!
//! ```text
//! cargo run -p apitorrent-metainfo --example mktorrent -- <path> <out.torrent> [v1|v2|hybrid] [piece-KiB]
//! ```
//!
//! Files are read into memory, so this is for test data, not large content.

use std::path::Path;

use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};

fn collect(root: &Path, dir: &Path, out: &mut Vec<(Vec<String>, Vec<u8>)>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for e in entries {
        let path = e.path();
        if e.file_type()?.is_dir() {
            collect(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push((rel, std::fs::read(&path)?));
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output, rest @ ..] = args.as_slice() else {
        eprintln!("usage: mktorrent <path> <out.torrent> [v1|v2|hybrid] [piece-KiB]");
        std::process::exit(2);
    };
    let version = match rest.first().map(String::as_str) {
        None | Some("v1") => Version::V1,
        Some("v2") => Version::V2,
        Some("hybrid") => Version::Hybrid,
        Some(other) => return Err(format!("unknown version {other}").into()),
    };
    let input = Path::new(input);
    let name = input
        .file_name()
        .ok_or("the input needs a name")?
        .to_string_lossy()
        .into_owned();
    let mut b = TorrentBuilder::new(name, version);
    if let Some(kib) = rest.get(1) {
        b = b.piece_length(kib.parse::<u64>()? * 1024);
    }
    if input.is_dir() {
        let mut files = Vec::new();
        collect(input, input, &mut files)?;
        for (path, data) in files {
            let p: Vec<&str> = path.iter().map(String::as_str).collect();
            b = b.file(FileSource::from_bytes(&p, data));
        }
    } else {
        b = b.file(FileSource::from_bytes(&[], std::fs::read(input)?));
    }
    std::fs::write(output, b.build()?)?;
    Ok(())
}
