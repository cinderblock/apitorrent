//! Starting the new binary after an install, once the daemon has shut down cleanly.
//!
//! - **Under systemd** the daemon just exits with [`RESTART_EXIT_CODE`]; the unit's
//!   `Restart=on-failure` (or `always`) starts the new binary in a fresh service
//!   instance. systemd sets `INVOCATION_ID` for every service it runs.
//! - **Otherwise** it re-executes itself with the same arguments: on Unix by `exec` (same
//!   process id, so a supervisor watching it doesn't notice), on Windows by starting the
//!   new process and exiting.

use std::ffi::OsString;
use std::path::Path;

/// The exit code that asks a supervisor for a restart (`EX_TEMPFAIL`).
pub const RESTART_EXIT_CODE: u8 = 75;

/// How to restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// Exit with [`RESTART_EXIT_CODE`] and let the supervisor start the new binary.
    Exit,
    /// Start the new binary ourselves.
    Reexec,
}

/// systemd runs us when it set `INVOCATION_ID`.
pub fn how(env: impl Fn(&str) -> Option<OsString>) -> Restart {
    if env("INVOCATION_ID").is_some_and(|v| !v.is_empty()) {
        Restart::Exit
    } else {
        Restart::Reexec
    }
}

/// For this process.
pub fn how_now() -> Restart {
    how(|k| std::env::var_os(k))
}

/// Run `exe` with `args` in place of this process. Returns only on failure (Unix), or
/// once the new process has started (Windows, where the caller then exits).
pub fn reexec(exe: &Path, args: &[OsString]) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        Err(cmd.exec())
    }
    #[cfg(not(unix))]
    {
        cmd.spawn().map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_is_detected_by_its_invocation_id() {
        assert_eq!(how(|_| None), Restart::Reexec);
        assert_eq!(
            how(|k| (k == "INVOCATION_ID").then(|| "abc".into())),
            Restart::Exit
        );
        assert_eq!(how(|_| Some(OsString::new())), Restart::Reexec);
    }
}
