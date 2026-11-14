//! `apitorrentd register-device`: a one-use token, valid ten minutes, that registers the
//! desktop app as a user's device, without a browser. Run it on the server, as the user
//! the daemon runs as, with the daemon's data directory; the daemon may be running.
//!
//! Before anyone has set up the server, the daemon's own setup token (printed at startup)
//! does this instead, and makes the first admin.

use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use apitorrent_auth::{REGISTRATION_TTL, create_registration_token};

#[derive(Debug, clap::Args)]
pub struct RegisterArgs {
    /// The user whose device it registers. Default: the only server admin.
    #[arg(long)]
    user: Option<String>,
}

pub fn run(args: &RegisterArgs, data_dir: &Path) -> ExitCode {
    let db = data_dir.join("auth.db");
    if !db.is_file() {
        eprintln!(
            "no apitorrent database at {}: pass the daemon's --data-dir (a systemd install keeps \
             it in /var/lib/apitorrent), and run this as the user the daemon runs as",
            db.display()
        );
        return ExitCode::FAILURE;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let issued = match create_registration_token(&db, args.user.as_deref(), now) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let minutes = REGISTRATION_TTL.as_secs() / 60;
    println!(
        "A registration token for {}'s next device. It works once, for {minutes} minutes:\n\n   \
         {}\n\nIn the desktop app, add this server and choose \"Register with a token\".",
        issued.username, issued.token
    );
    if let Some(code) = self_signed_code(data_dir) {
        println!("The app shows the server's key code when it connects; it must be:\n\n   {code}");
    }
    ExitCode::SUCCESS
}

/// The pairing code of the daemon's self-signed certificate (`--tls-self-signed`), if it
/// has made one.
fn self_signed_code(data_dir: &Path) -> Option<String> {
    let pem = std::fs::read(data_dir.join("tls/self-signed/cert.pem")).ok()?;
    let der = apitorrent_tls::read_pem_certificates(&pem)
        .ok()?
        .into_iter()
        .next()?;
    Some(
        apitorrent_tls::CertInfo::from_der(&der)
            .ok()?
            .pairing_code(),
    )
}
