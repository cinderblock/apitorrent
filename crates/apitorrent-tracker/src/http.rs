//! HTTP tracker requests and responses ([BEP 3], [BEP 23], [BEP 7], [BEP 48]), as plain
//! functions with no I/O.
//!
//! [BEP 3]: https://www.bittorrent.org/beps/bep_0003.html
//! [BEP 23]: https://www.bittorrent.org/beps/bep_0023.html
//! [BEP 7]: https://www.bittorrent.org/beps/bep_0007.html
//! [BEP 48]: https://www.bittorrent.org/beps/bep_0048.html

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use apitorrent_bencode::{DecodeOptions, Dict, Value};
use apitorrent_metainfo::Sha1Hash;
use apitorrent_wire::compact;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};

use crate::error::TrackerError;
use crate::types::{
    AnnounceRequest, AnnounceResponse, MAX_PEERS, MAX_SCRAPE_HASHES, ScrapeResponse, ScrapeStats,
};

/// Everything except the RFC 3986 unreserved characters is escaped, so binary info-hashes
/// and peer ids survive byte for byte.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode_bytes(bytes: &[u8]) -> percent_encoding::PercentEncode<'_> {
    percent_encode(bytes, QUERY_VALUE)
}

/// Split off a `#fragment`, which is never sent to the server.
fn without_fragment(url: &str) -> &str {
    url.split_once('#').map_or(url, |(before, _)| before)
}

/// Start appending parameters to `base`, keeping any query string it already has (private
/// trackers put the passkey there).
fn start_query(base: &str) -> String {
    let base = without_fragment(base);
    let mut out = String::with_capacity(base.len() + 256);
    out.push_str(base);
    match base.find('?') {
        None => out.push('?'),
        Some(_) if base.ends_with('?') || base.ends_with('&') => {}
        Some(_) => out.push('&'),
    }
    out
}

/// The announce URL for `req`: the tracker's URL with the BEP 3 parameters appended.
///
/// We ask for compact peers (`compact=1`) and no peer ids (`no_peer_id=1`). Trackers that
/// ignore this and send the list-of-dictionaries form are still understood.
pub fn announce_url(base: &str, req: &AnnounceRequest) -> String {
    let mut url = start_query(base);
    // Writing to a String cannot fail.
    let _ = write!(
        url,
        "info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&no_peer_id=1&key={:08X}",
        encode_bytes(req.info_hash.as_bytes()),
        encode_bytes(&req.peer_id.0),
        req.port,
        req.uploaded,
        req.downloaded,
        req.left,
        req.key,
    );
    if let Some(event) = req.event.http_name() {
        let _ = write!(url, "&event={event}");
    }
    if let Some(n) = req.num_want {
        let _ = write!(url, "&numwant={n}");
    }
    if let Some(id) = &req.tracker_id {
        let _ = write!(url, "&trackerid={}", encode_bytes(id.as_bytes()));
    }
    url
}

/// The scrape URL that BEP 48 derives from an announce URL: the last path segment must
/// start with `announce`, which is replaced by `scrape`. `None` if the tracker does not
/// follow the convention, which means it does not support scrape.
pub fn scrape_base_url(announce: &str) -> Option<String> {
    let url = without_fragment(announce);
    let authority_start = url.find("://")? + 3;
    let path_start = authority_start + url[authority_start..].find('/')?;
    let path_end = url[path_start..]
        .find('?')
        .map_or(url.len(), |i| path_start + i);
    let segment_start = path_start + url[path_start..path_end].rfind('/')? + 1;
    let segment = &url[segment_start..path_end];
    let rest = segment.strip_prefix("announce")?;
    Some(format!(
        "{}scrape{rest}{}",
        &url[..segment_start],
        &url[path_end..]
    ))
}

/// The full scrape URL for `hashes`.
pub fn scrape_url(announce: &str, hashes: &[Sha1Hash]) -> Result<String, TrackerError> {
    check_scrape_hashes(hashes)?;
    let base = scrape_base_url(announce).ok_or(TrackerError::ScrapeUnsupported)?;
    let mut url = start_query(&base);
    for (i, hash) in hashes.iter().enumerate() {
        if i > 0 {
            url.push('&');
        }
        let _ = write!(url, "info_hash={}", encode_bytes(hash.as_bytes()));
    }
    Ok(url)
}

pub(crate) fn check_scrape_hashes(hashes: &[Sha1Hash]) -> Result<(), TrackerError> {
    if hashes.is_empty() {
        return Err(TrackerError::InvalidRequest(
            "scrape needs at least one info-hash",
        ));
    }
    if hashes.len() > MAX_SCRAPE_HASHES {
        return Err(TrackerError::InvalidRequest(
            "too many info-hashes in one scrape",
        ));
    }
    Ok(())
}

/// Decode a bencoded response body to its top-level dictionary, and turn a
/// `failure reason` into [`TrackerError::Failure`].
fn decode_body(body: &[u8]) -> Result<Value<'_>, TrackerError> {
    // Trackers in the wild don't always sort their keys; duplicates are still rejected.
    let value = apitorrent_bencode::decode_with(body, &DecodeOptions::lenient())
        .map_err(|e| TrackerError::malformed(format!("not bencode: {e}")))?;
    let Some(dict) = value.as_dict() else {
        return Err(TrackerError::malformed(format!(
            "expected a dictionary, got a {}",
            value.kind()
        )));
    };
    if let Some(reason) = dict.get(b"failure reason") {
        let reason = reason
            .as_bytes()
            .map_or_else(|| "(no reason given)".into(), lossy);
        return Err(TrackerError::Failure(reason));
    }
    Ok(value)
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A count from the tracker, clamped into `u32` (negative counts become 0).
fn count(dict: &Dict<'_>, key: &[u8]) -> Option<u32> {
    dict.get_int(key)
        .map(|n| u32::try_from(n.max(0)).unwrap_or(u32::MAX))
}

fn seconds(dict: &Dict<'_>, key: &[u8]) -> Result<Option<Duration>, TrackerError> {
    match dict.get(key) {
        None => Ok(None),
        Some(v) => {
            let n = v.as_int().ok_or_else(|| {
                TrackerError::malformed(format!("{} is not an integer", lossy(key)))
            })?;
            let secs = u64::try_from(n)
                .map_err(|_| TrackerError::malformed(format!("{} is negative", lossy(key))))?;
            Ok(Some(Duration::from_secs(secs)))
        }
    }
}

/// Parse an announce response body.
pub fn parse_announce(body: &[u8]) -> Result<AnnounceResponse, TrackerError> {
    let value = decode_body(body)?;
    let dict = value.as_dict().expect("decode_body checked");

    let interval =
        seconds(dict, b"interval")?.ok_or_else(|| TrackerError::malformed("missing interval"))?;
    let min_interval = seconds(dict, b"min interval")?;

    let mut peers = Vec::new();
    match dict.get(b"peers") {
        None => {}
        Some(Value::Bytes(b)) => {
            let b = &b[..b.len().min(MAX_PEERS * 6)];
            peers.extend(compact::parse_v4(b));
        }
        Some(Value::List(list)) => {
            peers.extend(list.iter().take(MAX_PEERS).filter_map(dict_peer));
        }
        Some(other) => {
            return Err(TrackerError::malformed(format!(
                "peers is a {}",
                other.kind()
            )));
        }
    }
    match dict.get(b"peers6") {
        None => {}
        Some(Value::Bytes(b)) => {
            let room = MAX_PEERS.saturating_sub(peers.len());
            let b = &b[..b.len().min(room * 18)];
            peers.extend(compact::parse_v6(b));
        }
        Some(other) => {
            return Err(TrackerError::malformed(format!(
                "peers6 is a {}",
                other.kind()
            )));
        }
    }
    peers.retain(|p| p.port() != 0);
    peers.truncate(MAX_PEERS);

    Ok(AnnounceResponse {
        interval,
        min_interval,
        peers,
        seeders: count(dict, b"complete"),
        leechers: count(dict, b"incomplete"),
        tracker_id: dict.get_bytes(b"tracker id").map(lossy),
        warning: dict.get_bytes(b"warning message").map(lossy),
    })
}

/// One entry of the non-compact peer list: `{ip, port, peer id}`. Entries we can't use
/// (a hostname instead of an address, a bad port) are skipped rather than failing the
/// whole response.
fn dict_peer(v: &Value<'_>) -> Option<SocketAddr> {
    let d = v.as_dict()?;
    let ip: IpAddr = d.get_str(b"ip")?.trim().parse().ok()?;
    let port = u16::try_from(d.get_int(b"port")?).ok()?;
    Some(SocketAddr::new(ip, port))
}

/// Parse a scrape response body, keeping only entries for `requested` hashes.
pub fn parse_scrape(body: &[u8], requested: &[Sha1Hash]) -> Result<ScrapeResponse, TrackerError> {
    let value = decode_body(body)?;
    let dict = value.as_dict().expect("decode_body checked");
    let files = dict
        .get_dict(b"files")
        .ok_or_else(|| TrackerError::malformed("missing files dictionary"))?;
    let mut out = ScrapeResponse::default();
    for hash in requested {
        let Some(entry) = files.get_dict(hash.as_bytes()) else {
            continue;
        };
        out.files.insert(
            *hash,
            ScrapeStats {
                complete: count(entry, b"complete").unwrap_or(0),
                incomplete: count(entry, b"incomplete").unwrap_or(0),
                downloaded: count(entry, b"downloaded").unwrap_or(0),
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use apitorrent_wire::PeerId;

    use super::*;
    use crate::types::AnnounceEvent;

    fn request() -> AnnounceRequest {
        let mut hash = [0u8; 20];
        for (i, b) in hash.iter_mut().enumerate() {
            *b = u8::try_from(i * 13).unwrap();
        }
        AnnounceRequest {
            info_hash: Sha1Hash(hash),
            peer_id: PeerId(*b"-AT0100-ab~ c.d_e%f\xff"),
            port: 6881,
            uploaded: 1,
            downloaded: 2,
            left: 3,
            event: AnnounceEvent::Started,
            num_want: Some(50),
            key: 0xDEAD_BEEF,
            tracker_id: Some("id &=x".into()),
        }
    }

    #[test]
    fn announce_url_encodes_bytes_and_keeps_query() {
        let url = announce_url("https://t.example/announce?passkey=abc123#frag", &request());
        assert!(url.starts_with("https://t.example/announce?passkey=abc123&info_hash="));
        assert!(!url.contains("frag"));
        assert!(url.contains("info_hash=%00%0D%1A%274AN%5Bhu%82%8F%9C%A9%B6%C3%D0%DD%EA%F7&"));
        assert!(url.contains("&peer_id=-AT0100-ab~%20c.d_e%25f%FF&"));
        assert!(url.contains("&port=6881&uploaded=1&downloaded=2&left=3&compact=1"));
        assert!(url.contains("&key=DEADBEEF"));
        assert!(url.contains("&event=started"));
        assert!(url.contains("&numwant=50"));
        assert!(url.ends_with("&trackerid=id%20%26%3Dx"));
    }

    #[test]
    fn announce_url_without_query() {
        let mut req = request();
        req.event = AnnounceEvent::None;
        req.num_want = None;
        req.tracker_id = None;
        let url = announce_url("http://t.example:8080/announce", &req);
        assert!(url.starts_with("http://t.example:8080/announce?info_hash="));
        assert!(!url.contains("event="));
        assert!(!url.contains("numwant="));
        assert!(!url.contains("trackerid="));
        // A trailing `?` or `&` is not doubled.
        assert!(announce_url("http://t/a?", &req).starts_with("http://t/a?info_hash="));
        assert!(announce_url("http://t/a?k=v&", &req).starts_with("http://t/a?k=v&info_hash="));
    }

    #[test]
    fn scrape_urls_follow_bep48() {
        let cases = [
            ("http://t.example/announce", Some("http://t.example/scrape")),
            (
                "http://t.example/x/announce",
                Some("http://t.example/x/scrape"),
            ),
            (
                "http://t.example/announce.php",
                Some("http://t.example/scrape.php"),
            ),
            (
                "http://t.example/announce?passkey=p",
                Some("http://t.example/scrape?passkey=p"),
            ),
            ("http://t.example/a", None),
            ("http://t.example/announce/x", None),
            ("http://t.example/x%064announce", None),
            ("http://announce.example", None),
            ("http://announce.example/", None),
            ("http://t.example?announce", None),
            ("not a url", None),
        ];
        for (input, want) in cases {
            assert_eq!(scrape_base_url(input).as_deref(), want, "{input}");
        }

        let hashes = [Sha1Hash([0xAA; 20]), Sha1Hash([b'a'; 20])];
        let url = scrape_url("http://t.example/announce?pk=1", &hashes).unwrap();
        assert_eq!(
            url,
            format!(
                "http://t.example/scrape?pk=1&info_hash={}&info_hash={}",
                "%AA".repeat(20),
                "a".repeat(20)
            )
        );
        assert!(matches!(
            scrape_url("http://t.example/a", &hashes),
            Err(TrackerError::ScrapeUnsupported)
        ));
        assert!(matches!(
            scrape_url("http://t.example/announce", &[]),
            Err(TrackerError::InvalidRequest(_))
        ));
        let too_many = vec![Sha1Hash([0; 20]); MAX_SCRAPE_HASHES + 1];
        assert!(matches!(
            scrape_url("http://t.example/announce", &too_many),
            Err(TrackerError::InvalidRequest(_))
        ));
    }

    #[test]
    fn parses_compact_response() {
        let mut body =
            b"d8:completei5e10:incompletei3e8:intervali1800e12:min intervali60e5:peers12:".to_vec();
        body.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1, 5, 6, 7, 8, 0, 0]);
        body.extend_from_slice(b"6:peers618:");
        body.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        body.extend_from_slice(&[0xc8, 0xd5]);
        body.extend_from_slice(b"10:tracker id3:abc15:warning message4:hmm!e");
        let r = parse_announce(&body).unwrap();
        assert_eq!(r.interval, Duration::from_secs(1800));
        assert_eq!(r.min_interval, Some(Duration::from_secs(60)));
        assert_eq!(r.seeders, Some(5));
        assert_eq!(r.leechers, Some(3));
        assert_eq!(r.tracker_id.as_deref(), Some("abc"));
        assert_eq!(r.warning.as_deref(), Some("hmm!"));
        // The port-0 peer is dropped.
        assert_eq!(
            r.peers,
            [
                "1.2.3.4:6881".parse::<SocketAddr>().unwrap(),
                "[2001:db8::1]:51413".parse().unwrap()
            ]
        );
    }

    #[test]
    fn parses_dict_peers() {
        // Usable entries are kept; a bad address, a hostname, a bad port and a non-dictionary
        // entry are skipped.
        let body = b"d8:intervali900e5:peersld2:ip8:10.0.0.17:peer id20:aaaaaaaaaaaaaaaaaaaa4:porti6881eed2:ip7:::1abcd4:porti1eed2:ip11:example.com4:porti80eed2:ip3:::14:porti70000eed2:ip3:::14:porti2eei5eee";
        let r = parse_announce(body).unwrap();
        assert_eq!(
            r.peers,
            [
                "10.0.0.1:6881".parse::<SocketAddr>().unwrap(),
                "[::1]:2".parse().unwrap()
            ]
        );
        assert_eq!(r.seeders, None);
    }

    #[test]
    fn failure_and_malformed() {
        match parse_announce(b"d14:failure reason11:not allowede") {
            Err(TrackerError::Failure(m)) => assert_eq!(m, "not allowed"),
            other => panic!("{other:?}"),
        }
        match parse_scrape(b"d14:failure reason2:noe", &[]) {
            Err(TrackerError::Failure(m)) => assert_eq!(m, "no"),
            other => panic!("{other:?}"),
        }
        for bad in [
            &b""[..],
            b"<html>",
            b"le",
            b"de",
            b"d8:intervali-1ee",
            b"d8:interval3:abce",
            b"d8:intervali1e5:peersi1ee",
            b"d8:intervali1e6:peers6le",
        ] {
            assert!(
                matches!(parse_announce(bad), Err(TrackerError::Malformed(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn caps_peer_count() {
        let n = MAX_PEERS + 10;
        let mut body = format!("d8:intervali1e5:peers{}:", n * 6).into_bytes();
        for _ in 0..n {
            body.extend_from_slice(&[1, 1, 1, 1, 0, 1]);
        }
        body.extend_from_slice(format!("6:peers6{}:", 18 * 3).as_bytes());
        body.extend_from_slice(&[1; 18 * 3]);
        body.push(b'e');
        assert_eq!(parse_announce(&body).unwrap().peers.len(), MAX_PEERS);
    }

    #[test]
    fn parses_scrape() {
        let a = Sha1Hash([b'a'; 20]);
        let b = Sha1Hash([b'b'; 20]);
        let c = Sha1Hash([b'c'; 20]);
        let mut body = b"d5:filesd20:".to_vec();
        body.extend_from_slice(a.as_bytes());
        body.extend_from_slice(b"d8:completei7e10:downloadedi9e10:incompletei-2ee20:");
        body.extend_from_slice(c.as_bytes());
        body.extend_from_slice(b"d8:completei1eeee");
        let r = parse_scrape(&body, &[a, b]).unwrap();
        assert_eq!(r.files.len(), 1);
        assert_eq!(
            r.files[&a],
            ScrapeStats {
                complete: 7,
                incomplete: 0,
                downloaded: 9
            }
        );
        assert!(matches!(
            parse_scrape(b"de", &[a]),
            Err(TrackerError::Malformed(_))
        ));
    }
}
