//! A client for the apitorrent API, for the desktop thin client and the CLI.
//!
//! It **never accepts a certificate that fails verification**, and has no switch to. A
//! server is trusted through the OS, a private CA imported for the profile, or a key pinned
//! after comparing the daemon's pairing code (see [`trust`]). It authenticates with an API
//! token (`Authorization: Bearer`), which it only ever sends to `/api/` paths.

pub mod remote;
pub mod trust;

pub use remote::{ClientError, MAX_RESPONSE, Probe, RawResponse, Remote, SyncSocket, Token, probe};
pub use trust::{PinMismatch, SpkiHash, Trust, TrustError};
