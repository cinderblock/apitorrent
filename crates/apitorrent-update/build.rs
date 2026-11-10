//! Compiles in the update signing public key (`release/update-signing.pub`) when the file
//! exists, and the target triple the update manifest is searched for.
//!
//! Without the key file, `EMBEDDED_KEY` is `None` and installs are switched off: the
//! program can still say a new version exists.

use std::path::Path;

fn main() {
    let release = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../release");
    // The directory, not the file: a missing file would rerun this script on every build.
    println!("cargo:rerun-if-changed={}", release.display());
    let key = std::fs::read_to_string(release.join("update-signing.pub"))
        .ok()
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty());
    let code = match key {
        Some(k) => format!("pub(crate) const EMBEDDED_KEY: Option<&str> = Some({k:?});\n"),
        None => "pub(crate) const EMBEDDED_KEY: Option<&str> = None;\n".to_owned(),
    };
    let out = Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("embedded_key.rs");
    std::fs::write(out, code).unwrap();
    println!(
        "cargo:rustc-env=APITORRENT_UPDATE_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}
