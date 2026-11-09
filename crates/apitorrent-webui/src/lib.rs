//! The apitorrent web UI, embedded in the binary.
//!
//! With the `embed` feature, the build packs `web/dist` (or `APITORRENT_WEB_DIST`) into the
//! crate, with brotli and gzip copies of the compressible files. Without it, [`bundle`]
//! returns `None`, and the daemon serves the UI from `--web-root` or not at all. Release
//! builds enable it. Development builds usually point `--web-root` at a fresh
//! `bun run build`, or use the Vite dev server.

/// One file of the built UI.
#[derive(Debug)]
pub struct Asset {
    /// The URL path, starting with `/`.
    pub path: &'static str,
    pub content_type: &'static str,
    /// A strong validator for the raw bytes (hex, unquoted). Compressed copies add a suffix.
    pub etag: &'static str,
    /// Vite names these files by content hash, so a name never gets new content.
    pub immutable: bool,
    pub raw: &'static [u8],
    pub br: Option<&'static [u8]>,
    pub gzip: Option<&'static [u8]>,
}

/// The embedded UI: its files, sorted by path.
#[derive(Debug)]
pub struct Bundle {
    files: &'static [Asset],
}

impl Bundle {
    /// A bundle from files sorted by path. The build makes the real one; this is for tests
    /// and for embedders with their own assets.
    ///
    /// # Panics
    ///
    /// If the files aren't sorted by path, or there is no `/index.html`.
    pub fn new(files: &'static [Asset]) -> Self {
        assert!(
            files.windows(2).all(|w| w[0].path < w[1].path),
            "bundle files must be sorted by path, without duplicates"
        );
        let bundle = Self { files };
        assert!(
            bundle.get("/index.html").is_some(),
            "a bundle needs /index.html"
        );
        bundle
    }

    /// The file at a URL path, such as `/assets/index-abc.js`.
    pub fn get(&self, path: &str) -> Option<&'static Asset> {
        self.files
            .binary_search_by(|a| a.path.cmp(path))
            .ok()
            .map(|i| &self.files[i])
    }

    /// The single-page app's entry point, served for client-side routes.
    pub fn index(&self) -> &'static Asset {
        self.get("/index.html")
            .expect("the build checks for index.html")
    }

    pub fn files(&self) -> &'static [Asset] {
        self.files
    }
}

include!(concat!(env!("OUT_DIR"), "/bundle.rs"));

static BUNDLE: Option<Bundle> = match FILES {
    Some(files) => Some(Bundle { files }),
    None => None,
};

/// The embedded UI, if this build has one.
pub fn bundle() -> Option<&'static Bundle> {
    BUNDLE.as_ref()
}
