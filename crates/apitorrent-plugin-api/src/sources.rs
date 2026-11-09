//! Release sources: how plugins hand candidates to the core's rules, and how the core gets
//! a candidate's `.torrent` back through the plugin that named it.

use crate::{AppError, BoxFuture};

/// Fetches a URL with one plugin's own network grants, through the plugin host's `fetch`
/// (the same grant, address and TLS checks a plugin's own `fetch` gets).
///
/// The core uses it for a `source` plugin's `.torrent` links once a rule takes a candidate.
/// It never fetches a plugin-supplied URL with the daemon's own network access: that would
/// let a plugin reach hosts an admin never granted it.
pub trait SourceFetcher: Send + Sync + 'static {
    /// `GET url` as plugin `plugin_id`, returning at most `max_bytes` of body. Fails when
    /// the plugin isn't running, the URL isn't within its grants, or the answer isn't a
    /// success.
    fn fetch_as(
        &self,
        plugin_id: &str,
        url: &str,
        max_bytes: usize,
    ) -> BoxFuture<Result<Vec<u8>, AppError>>;
}
