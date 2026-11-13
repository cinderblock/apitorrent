//! Deluge's `auth` file: who may log in to the daemon.
//!
//! One account per line, `username:password:level` (`deluge/core/authmanager.py`); `#`
//! starts a comment. The level is a number (`10` admin, `5` normal, `1` read-only) or, in old
//! files, missing. Deluge splits on every `:`, so a password can't contain one, and strips
//! spaces around the name and the password. Deluge makes a `localclient` account with a random
//! password on first start, for clients on the same machine; that's the one we use unless told
//! otherwise.
//!
//! The password is held only for the login call: it has no `Debug` or `Display`, and nothing
//! here logs it.

use std::fmt;
use std::path::{Path, PathBuf};

/// The account Deluge makes for clients on the same machine.
pub const LOCALCLIENT: &str = "localclient";

/// A username and password from the `auth` file.
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthFileError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} has no account named {username:?}")]
    NoAccount { path: PathBuf, username: String },
}

impl Credentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }

    /// `username`'s entry in `<config_dir>/auth`.
    pub fn from_config_dir(config_dir: &Path, username: &str) -> Result<Self, AuthFileError> {
        let path = config_dir.join("auth");
        let text = std::fs::read_to_string(&path).map_err(|source| AuthFileError::Io {
            path: path.clone(),
            source,
        })?;
        Self::from_auth_file(&text, username).ok_or(AuthFileError::NoAccount {
            path,
            username: username.to_owned(),
        })
    }

    /// `username`'s entry in the text of an `auth` file. The last entry wins, as in Deluge.
    pub fn from_auth_file(text: &str, username: &str) -> Option<Self> {
        let mut found = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split(':').collect();
            if !(2..=3).contains(&fields.len()) {
                continue;
            }
            if fields[0].trim() == username {
                found = Some(Self::new(username, fields[1].trim()));
            }
        }
        found
    }

    pub(crate) fn password(&self) -> &str {
        &self.password
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_entries_like_deluge() {
        let text = "\
# a comment
localclient:0123abcd:10

 bob : hunter2 :5
old:nolevel
bad:too:many:fields
localclient2:x:10
";
        let c = Credentials::from_auth_file(text, "localclient").unwrap();
        assert_eq!(c.username, "localclient");
        assert_eq!(c.password(), "0123abcd");
        assert_eq!(
            Credentials::from_auth_file(text, "bob").unwrap().password(),
            "hunter2"
        );
        assert_eq!(
            Credentials::from_auth_file(text, "old").unwrap().password(),
            "nolevel"
        );
        assert!(Credentials::from_auth_file(text, "bad").is_none());
        assert!(Credentials::from_auth_file(text, "nobody").is_none());
        // CRLF files (Windows) work.
        let crlf = "localclient:pw:10\r\n";
        assert_eq!(
            Credentials::from_auth_file(crlf, "localclient")
                .unwrap()
                .password(),
            "pw"
        );
    }

    #[test]
    fn never_shows_the_password() {
        let c = Credentials::new("localclient", "s3cret");
        assert!(!format!("{c:?}").contains("s3cret"));
    }

    #[test]
    fn reads_a_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("auth"), "localclient:pw:10\n").unwrap();
        let c = Credentials::from_config_dir(dir.path(), LOCALCLIENT).unwrap();
        assert_eq!(c.password(), "pw");
        assert!(matches!(
            Credentials::from_config_dir(dir.path(), "bob"),
            Err(AuthFileError::NoAccount { .. })
        ));
    }
}
