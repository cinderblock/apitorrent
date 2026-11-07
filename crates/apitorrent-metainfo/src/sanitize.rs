//! Turning torrent-supplied names into safe path components.
//!
//! The same rules apply on every platform. That is deliberate: a torrent downloaded by a
//! Linux server onto a share must have the layout a Windows or macOS client expects when it
//! opens the same share. The rules are the union of what Windows, macOS and Linux need:
//!
//! - Leading `/` and `\` are removed.
//! - Invalid UTF-8, control characters and `< > : " / \ | ? *` become `_`. A path separator
//!   inside a component can never create a directory or climb out of one.
//! - Trailing spaces and dots are removed (Windows drops them silently).
//! - `.`, `..` and components that end up empty become `_`.
//! - Windows device names (`CON`, `NUL`, `COM1`, …, with or without an extension) get a `_`
//!   prefix.
//! - Components are cut to 255 bytes of UTF-8, keeping a short extension.
//!
//! Sibling names that collide case-insensitively, as they would on Windows and macOS, are
//! disambiguated by [`crate::FileList`] during parsing.

/// Rules for turning torrent-supplied names into path components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Naming {
    /// The same rules on every platform (see the module docs): safe to share over SMB,
    /// move to exFAT, or open from any OS. The default.
    #[default]
    Portable,
    /// Only the host's rules, plus the ones that are always enforced: no separators, no
    /// `.` or `..`, no control characters, 255 bytes at most. On Windows this is the same
    /// as [`Naming::Portable`]. `case_insensitive` says whether the target filesystem
    /// folds case, so siblings differing only in case must be renamed.
    Native { case_insensitive: bool },
    /// The names exactly as the torrent gives them (invalid UTF-8 replaced). Never for
    /// disk paths: for building web seed URLs, which name the server's original files.
    Verbatim,
}

impl Naming {
    /// Make one path component under these rules.
    pub fn component(self, raw: &[u8]) -> String {
        match self {
            Naming::Portable => component(raw),
            Naming::Native { .. } if cfg!(windows) => component(raw),
            Naming::Native { .. } => posix_component(raw),
            Naming::Verbatim => String::from_utf8_lossy(raw).into_owned(),
        }
    }

    /// Whether sibling names that differ only in case collide.
    pub fn folds_case(self) -> bool {
        match self {
            Naming::Portable => true,
            Naming::Native { case_insensitive } => case_insensitive || cfg!(windows),
            Naming::Verbatim => false,
        }
    }
}

/// A component under POSIX rules: only `/`, NUL and control characters are replaced, and
/// `.`, `..` and empty names become `_`. Never returns an empty string, `.` or `..`.
pub fn posix_component(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_start_matches('/');
    let mut out: String = text
        .chars()
        .map(|c| match c {
            '\u{0}'..='\u{1f}' | '\u{7f}' | '/' | '\u{fffd}' => '_',
            c => c,
        })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        "_".clone_into(&mut out);
    }
    if out.len() > MAX_COMPONENT_BYTES {
        out = shorten(&out);
    }
    out
}

/// The longest component, in bytes of UTF-8. Linux allows 255 bytes; Windows allows 255
/// UTF-16 code units, which is never fewer characters.
pub const MAX_COMPONENT_BYTES: usize = 255;

/// The longest extension (including the dot) kept when a long name is shortened.
const MAX_KEPT_EXTENSION: usize = 16;

/// Make one path component safe. Never returns an empty string, `.` or `..`.
pub fn component(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    // A leading separator (`/etc/passwd`, `\\server`) is the creator's absolute path; the
    // name they meant is what follows it.
    let text = text.trim_start_matches(['/', '\\']);
    let mut out: String = text
        .chars()
        .map(|c| match c {
            '\u{0}'..='\u{1f}' | '\u{7f}' => '_',
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            '\u{fffd}' => '_',
            c => c,
        })
        .collect();

    let trimmed_len = out.trim_end_matches([' ', '.']).len();
    out.truncate(trimmed_len);
    if out.is_empty() {
        return "_".to_owned();
    }

    if is_windows_device_name(&out) {
        out.insert(0, '_');
    }

    if out.len() > MAX_COMPONENT_BYTES {
        out = shorten(&out);
    }
    out
}

fn is_windows_device_name(name: &str) -> bool {
    // The device name is matched on the part before the first dot: `CON.txt` is still CON.
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let (Some(prefix), Some(digit)) = (upper.get(..3), upper.get(3..)) else {
                return false;
            };
            (prefix == "COM" || prefix == "LPT")
                && matches!(
                    digit,
                    "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
        }
    }
}

/// Cut a name to [`MAX_COMPONENT_BYTES`], keeping its extension if the extension is short.
fn shorten(name: &str) -> String {
    let ext = name
        .rfind('.')
        .map(|i| &name[i..])
        .filter(|e| e.len() <= MAX_KEPT_EXTENSION && e.len() < name.len());
    let ext = ext.unwrap_or("");
    let budget = MAX_COMPONENT_BYTES - ext.len();
    let stem = &name[..name.len() - ext.len()];
    let mut cut = budget.min(stem.len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = stem[..cut].trim_end_matches([' ', '.']).to_owned();
    if out.is_empty() {
        out.push('_');
    }
    out.push_str(ext);
    out
}

/// A case-insensitive key for collision detection. Windows and macOS (by default) treat
/// names that differ only in case as the same file.
pub(crate) fn fold_case(name: &str) -> String {
    name.to_lowercase()
}

/// Insert `.N` before the extension: `bar.txt` → `bar.1.txt`, `bar` → `bar.1`.
pub(crate) fn with_counter(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{}.{n}{}", &name[..i], &name[i..]),
        _ => format!("{name}.{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert_eq!(component(b"normal.txt"), "normal.txt");
        assert_eq!(component(b"a/b"), "a_b");
        assert_eq!(component(b"abc/../../def"), "abc_.._.._def");
        assert_eq!(component(b".."), "_");
        assert_eq!(component(b"."), "_");
        assert_eq!(component(b""), "_");
        assert_eq!(component(b"/"), "_");
        assert_eq!(component(b"/foobar"), "foobar");
        assert_eq!(component(b"\\server"), "server");
        assert_eq!(component(b"foobar "), "foobar");
        assert_eq!(component(b"foobar..."), "foobar");
        assert_eq!(component(b"what?.txt"), "what_.txt");
        assert_eq!(component(b"tab\there"), "tab_here");
        assert_eq!(component(b"bad\xffutf8"), "bad_utf8");
        assert_eq!(component(b" leading space"), " leading space");
        assert_eq!(component("日本語.mkv".as_bytes()), "日本語.mkv");
    }

    #[test]
    fn device_names() {
        assert_eq!(component(b"CON"), "_CON");
        assert_eq!(component(b"con.txt"), "_con.txt");
        assert_eq!(component(b"COM1"), "_COM1");
        assert_eq!(component(b"lpt9.tar.gz"), "_lpt9.tar.gz");
        assert_eq!(component(b"COM10"), "COM10");
        assert_eq!(component(b"CONSOLE"), "CONSOLE");
        assert_eq!(component(b"NUL "), "_NUL");
        assert_eq!(component("ab日".as_bytes()), "ab日");
    }

    #[test]
    fn long_names_keep_extension() {
        let long = format!("{}.mkv", "x".repeat(400));
        let out = component(long.as_bytes());
        assert_eq!(out.len(), MAX_COMPONENT_BYTES);
        assert!(out.ends_with(".mkv"));

        // Multi-byte characters are never split.
        let long = "é".repeat(200);
        let out = component(long.as_bytes());
        assert!(out.len() <= MAX_COMPONENT_BYTES);
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn counters() {
        assert_eq!(with_counter("bar.txt", 1), "bar.1.txt");
        assert_eq!(with_counter("bar", 2), "bar.2");
        assert_eq!(with_counter(".hidden", 1), ".hidden.1");
    }
}
