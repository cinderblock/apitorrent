//! Web seeds (BEP 19): plain HTTP servers holding a torrent's files.
//!
//! Each web seed URL gets a worker task. It asks its torrent for a piece, which the picker
//! hands out exclusively (a web seed is like a peer that has everything and takes whole
//! pieces), fetches the piece's byte ranges from the files with `Range` requests, and
//! hands the data back to be verified and written like any other piece. Failures back off;
//! a web seed that keeps failing, or sends data that doesn't verify, is dropped.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use apitorrent_tracker::reqwest;

use crate::layout::Layout;
use crate::limits::{Direction, Throttle};
use crate::picker::PeerKey;

/// The longest a single range response may be, as a sanity limit.
const MAX_RANGE: u64 = 64 * 1024 * 1024;
/// Backoff after a failed request, doubling up to the maximum.
const BACKOFF_START: Duration = Duration::from_secs(15);
const BACKOFF_MAX: Duration = Duration::from_secs(10 * 60);
/// How long to wait before asking again when there's nothing to fetch.
const IDLE_WAIT: Duration = Duration::from_secs(5);

/// From a worker to its torrent.
#[derive(Debug)]
pub(crate) enum WebSeedEvent {
    /// Ready for work; the reply names a piece to fetch, or `None` for nothing now.
    Want {
        key: PeerKey,
        reply: oneshot::Sender<Option<u32>>,
    },
    /// The whole of `piece`.
    Piece {
        key: PeerKey,
        piece: u32,
        data: Bytes,
    },
    /// Fetching `piece` failed; it goes back to the picker.
    Failed {
        key: PeerKey,
        piece: u32,
        error: String,
    },
}

/// Where one file of the torrent lives on a web seed.
fn file_url(base: &str, name: &str, single_file: bool, path: &[String]) -> String {
    let enc = |s: &str| {
        url::form_urlencoded::byte_serialize(s.as_bytes())
            .collect::<String>()
            // form encoding turns spaces into `+`; paths need `%20`.
            .replace('+', "%20")
    };
    if single_file {
        // BEP 19: a URL ending in `/` is a directory holding the file.
        if base.ends_with('/') {
            format!("{base}{}", enc(name))
        } else {
            base.to_owned()
        }
    } else {
        let mut url = base.to_owned();
        if !url.ends_with('/') {
            url.push('/');
        }
        let parts: Vec<String> = path.iter().map(|c| enc(c)).collect();
        url.push_str(&parts.join("/"));
        url
    }
}

/// What a worker needs to know about its torrent.
#[derive(Debug, Clone)]
pub(crate) struct WebSeedTorrent {
    pub layout: Layout,
    /// URL path components of each file, relative to the base URL (multi-file torrents:
    /// the torrent directory and then the file's path).
    pub paths: Vec<Vec<String>>,
    pub name: String,
    pub single_file: bool,
}

pub(crate) fn spawn(
    url: String,
    key: PeerKey,
    torrent: Arc<WebSeedTorrent>,
    client: reqwest::Client,
    throttle: Arc<Throttle>,
    events: mpsc::Sender<WebSeedEvent>,
) {
    tokio::spawn(async move {
        let mut backoff = BACKOFF_START;
        loop {
            let (reply, answer) = oneshot::channel();
            if events
                .send(WebSeedEvent::Want { key, reply })
                .await
                .is_err()
            {
                return; // the torrent is gone
            }
            let Ok(next) = answer.await else { return };
            let Some(piece) = next else {
                tokio::time::sleep(IDLE_WAIT).await;
                continue;
            };
            match fetch_piece(&url, &torrent, piece, &client, &throttle).await {
                Ok(data) => {
                    backoff = BACKOFF_START;
                    if events
                        .send(WebSeedEvent::Piece { key, piece, data })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    tracing::debug!(%url, piece, %error, "web seed request failed");
                    if events
                        .send(WebSeedEvent::Failed { key, piece, error })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    });
}

async fn fetch_piece(
    base: &str,
    t: &WebSeedTorrent,
    piece: u32,
    client: &reqwest::Client,
    throttle: &Throttle,
) -> Result<Bytes, String> {
    let offset = t.layout.piece_offset(piece);
    let size = t.layout.piece_size(piece) as usize;
    let mut buf = vec![0u8; size];
    for seg in t.layout.segments(offset, size) {
        if seg.pad || seg.len == 0 {
            continue; // pad bytes are zeros, already in place
        }
        let url = file_url(base, &t.name, t.single_file, &t.paths[seg.file]);
        let end = seg.file_offset + seg.len as u64 - 1;
        let resp = client
            .get(&url)
            .header(
                reqwest::header::RANGE,
                format!("bytes={}-{end}", seg.file_offset),
            )
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let status = resp.status();
        // 206 is the answer to a range request. A 200 is only usable when the range is
        // the whole file (some servers ignore ranges that cover everything).
        let whole_file =
            seg.file_offset == 0 && t.layout.files()[seg.file].length == seg.len as u64;
        if status != reqwest::StatusCode::PARTIAL_CONTENT && !(status.is_success() && whole_file) {
            return Err(format!("{url}: HTTP {status}"));
        }
        if resp.content_length().is_some_and(|n| n > MAX_RANGE) {
            return Err(format!("{url}: response too large"));
        }
        let dst = &mut buf[seg.buf_offset..seg.buf_offset + seg.len];
        let mut filled = 0usize;
        let mut resp = resp;
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{url}: {e}"))? {
            if filled + chunk.len() > dst.len() {
                return Err(format!("{url}: more data than asked for"));
            }
            dst[filled..filled + chunk.len()].copy_from_slice(&chunk);
            filled += chunk.len();
            throttle.take(Direction::Download, chunk.len() as u64).await;
        }
        if filled != dst.len() {
            return Err(format!(
                "{url}: short response ({filled} of {} bytes)",
                dst.len()
            ));
        }
    }
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            file_url("http://h/dl/", "My File.iso", true, &[]),
            "http://h/dl/My%20File.iso"
        );
        assert_eq!(
            file_url("http://h/x.iso", "My File.iso", true, &[]),
            "http://h/x.iso"
        );
        assert_eq!(
            file_url(
                "http://h/dl",
                "pack",
                false,
                &["pack".into(), "a b".into(), "c#d.txt".into()]
            ),
            "http://h/dl/pack/a%20b/c%23d.txt"
        );
    }
}
