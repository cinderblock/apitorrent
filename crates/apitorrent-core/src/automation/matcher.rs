//! Compiled rule conditions: text matches (contains, equals, glob, regex), metadata
//! conditions and file globs.
//!
//! Globs are translated to anchored regular expressions: `*` matches within one path
//! segment, `**` across segments, `?` one character, `[…]` a set (`[!…]` negated).
//! Everything else is literal. Matching ignores case. As in `.gitignore`, a file glob
//! without a `/` matches the file's name in any folder (`*.iso`), and one with a `/`
//! matches the whole path inside the torrent (`ubuntu-*/casper/*`).

use regex::{Regex, RegexBuilder};

use apitorrent_types::automation::{
    CandidateFilter, FileSelection, MatchMode, MetadataMatch, ReleaseCandidate, TextMatch,
};

/// The largest compiled regular expression a rule may hold, so a hostile pattern can't
/// take the daemon's memory.
const REGEX_SIZE_LIMIT: usize = 1 << 20;
const MAX_PATTERN: usize = 1000;

fn build(pattern: &str) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| format!("`{pattern}`: {e}"))
}

/// A glob as an anchored regular expression. `path` globs keep `*` within a segment.
pub fn glob_regex(glob: &str, path: bool) -> Result<Regex, String> {
    if glob.len() > MAX_PATTERN {
        return Err("the pattern is too long".to_owned());
    }
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if path && chars.peek() == Some(&'/') {
                    // `**/` matches zero or more whole folders.
                    chars.next();
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str(if path { "[^/]*" } else { ".*" }),
            '?' => re.push_str(if path { "[^/]" } else { "." }),
            '[' => {
                let mut class = String::new();
                let mut closed = false;
                if matches!(chars.peek(), Some('!' | '^')) {
                    chars.next();
                    class.push('^');
                }
                // A `]` straight after the opening is a literal.
                if chars.peek() == Some(&']') {
                    chars.next();
                    class.push_str("\\]");
                }
                for c in chars.by_ref() {
                    if c == ']' {
                        closed = true;
                        break;
                    }
                    if matches!(c, '\\' | '[' | '&' | '~') {
                        class.push('\\');
                    }
                    class.push(c);
                }
                if !closed {
                    return Err(format!("`{glob}`: a `[` has no closing `]`"));
                }
                re.push('[');
                re.push_str(&class);
                re.push(']');
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    build(&re).map_err(|e| format!("`{glob}`: {e}"))
}

/// A compiled [`TextMatch`].
#[derive(Debug, Clone)]
pub enum Text {
    Contains(String),
    Equals(String),
    Pattern(Regex),
}

impl Text {
    pub fn compile(m: &TextMatch) -> Result<Self, String> {
        if m.pattern.len() > MAX_PATTERN {
            return Err("a pattern is too long".to_owned());
        }
        Ok(match m.mode {
            MatchMode::Contains => Text::Contains(m.pattern.to_lowercase()),
            MatchMode::Equals => Text::Equals(m.pattern.to_lowercase()),
            MatchMode::Glob => Text::Pattern(glob_regex(&m.pattern, false)?),
            MatchMode::Regex => Text::Pattern(build(&m.pattern)?),
        })
    }

    pub fn is_match(&self, text: &str) -> bool {
        match self {
            Text::Contains(p) => text.to_lowercase().contains(p.as_str()),
            Text::Equals(p) => text.to_lowercase() == *p,
            Text::Pattern(re) => re.is_match(text),
        }
    }
}

#[derive(Debug, Clone)]
struct Meta {
    field: String,
    text: Text,
}

impl Meta {
    fn is_match(&self, metadata: &serde_json::Map<String, serde_json::Value>) -> bool {
        fn one(text: &Text, v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::String(s) => text.is_match(s),
                serde_json::Value::Array(items) => items.iter().any(|i| one(text, i)),
                serde_json::Value::Null | serde_json::Value::Object(_) => false,
                other => text.is_match(&other.to_string()),
            }
        }
        metadata
            .get(&self.field)
            .is_some_and(|v| one(&self.text, v))
    }
}

/// A compiled [`CandidateFilter`].
#[derive(Debug, Clone)]
pub struct Filter {
    sources: Vec<String>,
    title: Option<Text>,
    exclude_title: Option<Text>,
    metadata: Vec<Meta>,
}

impl Filter {
    pub fn compile(f: &CandidateFilter) -> Result<Self, String> {
        let meta = |m: &MetadataMatch| -> Result<Meta, String> {
            if m.field.trim().is_empty() {
                return Err("a metadata condition has no field".to_owned());
            }
            Ok(Meta {
                field: m.field.clone(),
                text: Text::compile(&m.matches)?,
            })
        };
        Ok(Self {
            sources: f.sources.iter().map(|s| s.trim().to_owned()).collect(),
            title: f.title.as_ref().map(Text::compile).transpose()?,
            exclude_title: f.exclude_title.as_ref().map(Text::compile).transpose()?,
            metadata: f.metadata.iter().map(meta).collect::<Result<_, _>>()?,
        })
    }

    pub fn takes_source(&self, source: &str) -> bool {
        self.sources.is_empty() || self.sources.iter().any(|s| s == source)
    }

    pub fn is_match(&self, source: &str, c: &ReleaseCandidate) -> bool {
        self.takes_source(source)
            && self.title.as_ref().is_none_or(|t| t.is_match(&c.title))
            && !self
                .exclude_title
                .as_ref()
                .is_some_and(|t| t.is_match(&c.title))
            && self.metadata.iter().all(|m| m.is_match(&c.metadata))
    }
}

/// A file glob: over the file name alone when it has no `/`, else over the whole path.
#[derive(Debug, Clone)]
pub struct FileGlob {
    re: Regex,
    name_only: bool,
}

impl FileGlob {
    pub fn compile(glob: &str) -> Result<Self, String> {
        let glob = glob.trim();
        Ok(Self {
            re: glob_regex(glob, true)?,
            name_only: !glob.contains('/'),
        })
    }

    /// Whether `path` (inside the torrent, `/`-separated) matches.
    pub fn is_match(&self, path: &str) -> bool {
        if self.name_only {
            self.re.is_match(path.rsplit('/').next().unwrap_or(path))
        } else {
            self.re.is_match(path)
        }
    }
}

/// A compiled [`FileSelection`].
#[derive(Debug, Clone)]
pub struct Files {
    include: Vec<FileGlob>,
    exclude: Vec<FileGlob>,
}

impl Files {
    pub fn compile(f: &FileSelection) -> Result<Self, String> {
        let all = |gs: &[String]| -> Result<Vec<FileGlob>, String> {
            gs.iter()
                .filter(|g| !g.trim().is_empty())
                .map(|g| FileGlob::compile(g))
                .collect()
        };
        Ok(Self {
            include: all(&f.include)?,
            exclude: all(&f.exclude)?,
        })
    }

    /// Whether every file is wanted, so priorities needn't be set.
    pub fn selects_all(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// Whether the file at `path` (inside the torrent, `/`-separated) is wanted.
    pub fn wants(&self, path: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|r| r.is_match(path)))
            && !self.exclude.iter().any(|r| r.is_match(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::automation::CandidateLinks;

    fn g(glob: &str, path: &str) -> bool {
        glob_regex(glob, true).unwrap().is_match(path)
    }

    #[test]
    fn path_globs() {
        assert!(g("*.iso", "ubuntu.iso"));
        assert!(!g("*.iso", "dir/ubuntu.iso"), "`*` stays in one folder");
        assert!(g("**/*.iso", "dir/ubuntu.iso"));
        assert!(g("**/*.iso", "ubuntu.iso"), "`**/` may match no folder");
        assert!(g("**", "a/b/c"));
        assert!(g("*.ISO", "ubuntu.iso"), "case is ignored");
        assert!(g("disc?.bin", "disc1.bin"));
        assert!(!g("disc?.bin", "disc12.bin"));
        assert!(g("[ab]*.txt", "a1.txt"));
        assert!(g("[!ab]*.txt", "c1.txt"));
        assert!(!g("[!ab]*.txt", "a1.txt"));
        assert!(
            g("a+b(1).txt", "a+b(1).txt"),
            "regex characters are literal"
        );
        assert!(!g("a+b(1).txt", "aab1.txt"));
        assert!(glob_regex("[abc", true).is_err());
    }

    #[test]
    fn text_matches() {
        let t = |mode, p: &str| {
            Text::compile(&TextMatch {
                mode,
                pattern: p.into(),
            })
            .unwrap()
        };
        assert!(t(MatchMode::Contains, "Desktop").is_match("ubuntu-26.04-desktop-amd64"));
        assert!(t(MatchMode::Equals, "LTS").is_match("lts"));
        assert!(!t(MatchMode::Equals, "LTS").is_match("lts2"));
        assert!(t(MatchMode::Glob, "ubuntu-*-desktop-*").is_match("Ubuntu-26.04-desktop-amd64"));
        assert!(t(MatchMode::Regex, r"\d+\.04").is_match("ubuntu 26.04"));
        assert!(
            Text::compile(&TextMatch {
                mode: MatchMode::Regex,
                pattern: "(".into()
            })
            .is_err()
        );
    }

    #[test]
    fn filters_and_files() {
        let f = Filter::compile(&CandidateFilter {
            sources: vec!["plugin:ubuntu-releases".into()],
            title: Some(TextMatch {
                mode: MatchMode::Contains,
                pattern: "ubuntu".into(),
            }),
            exclude_title: Some(TextMatch {
                mode: MatchMode::Contains,
                pattern: "beta".into(),
            }),
            metadata: vec![MetadataMatch {
                field: "lts".into(),
                matches: TextMatch {
                    mode: MatchMode::Equals,
                    pattern: "true".into(),
                },
            }],
        })
        .unwrap();
        let mut c = ReleaseCandidate {
            id: "1".into(),
            title: "Ubuntu 26.04".into(),
            version: None,
            published: None,
            links: CandidateLinks::default(),
            metadata: serde_json::json!({ "lts": true })
                .as_object()
                .unwrap()
                .clone(),
        };
        assert!(f.is_match("plugin:ubuntu-releases", &c));
        assert!(!f.is_match("plugin:rss", &c));
        c.title = "Ubuntu 26.04 beta".into();
        assert!(!f.is_match("plugin:ubuntu-releases", &c));
        c.title = "Ubuntu 26.04".into();
        c.metadata = serde_json::json!({ "lts": false })
            .as_object()
            .unwrap()
            .clone();
        assert!(!f.is_match("plugin:ubuntu-releases", &c));
        c.metadata = serde_json::json!({ "lts": [false, true] })
            .as_object()
            .unwrap()
            .clone();
        assert!(f.is_match("plugin:ubuntu-releases", &c), "any list element");

        let files = Files::compile(&FileSelection {
            include: vec!["**/*.iso".into()],
            exclude: vec!["**/*source*".into()],
        })
        .unwrap();
        assert!(files.wants("ubuntu.iso"));
        assert!(files.wants("d/ubuntu.iso"));
        assert!(!files.wants("d/ubuntu-source.iso"));
        assert!(!files.wants("SHA256SUMS"));
        // Without a `/`, a glob names files in any folder; with one, the whole path.
        let by_name = FileGlob::compile("*.iso").unwrap();
        assert!(by_name.is_match("ubuntu-26.04/ubuntu.iso"));
        assert!(!by_name.is_match("ubuntu-26.04/ubuntu.iso.zsync"));
        let by_path = FileGlob::compile("*/casper/*").unwrap();
        assert!(by_path.is_match("ubuntu/casper/vmlinuz"));
        assert!(!by_path.is_match("ubuntu/other/casper/vmlinuz"));
        assert!(
            Files::compile(&FileSelection::default())
                .unwrap()
                .selects_all()
        );
    }
}
