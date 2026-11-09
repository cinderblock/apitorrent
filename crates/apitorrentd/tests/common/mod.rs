//! Shared test support: building first-party plugins with Bun.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use apitorrent_plugin_api::PluginBundle;

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn bun(args: &[&str], dir: &Path) {
    let out = Command::new("bun")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("this test needs bun on the PATH (https://bun.sh): {e}"));
    assert!(
        out.status.success(),
        "bun {args:?} failed:
{}
{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build `plugins/<name>` into `out` and load the bundle.
pub fn build_plugin(name: &str, out: &Path) -> PluginBundle {
    // Tests run in parallel; two `bun install`s at once trip over each other's links.
    static INSTALL: Mutex<()> = Mutex::new(());
    let root = repo();
    {
        let _one_at_a_time = INSTALL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !root.join("node_modules/@apitorrent/plugin-sdk").exists() {
            bun(&["install", "--frozen-lockfile"], &root);
        }
    }
    let build = root.join("packages/plugin-sdk/src/build.ts");
    let plugin = root.join("plugins").join(name);
    bun(
        &[
            build.to_str().unwrap(),
            plugin.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
        &root,
    );
    let file = std::fs::read_dir(out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".plugin.json"))
        .expect("a bundle");
    serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
}
