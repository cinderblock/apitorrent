//! A whole `.torrent` file: the info dictionary plus the fields around it.

use std::collections::HashMap;
use std::sync::Arc;

use apitorrent_bencode::{DecodeOptions, Dict, Value};

use crate::error::MetainfoError;
use crate::hash::{InfoHashes, Sha256Hash};
use crate::info::{Info, Limits};

/// A parsed, validated `.torrent` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Torrent {
    pub info: Info,
    /// Tracker tiers (BEP 12). A plain `announce` becomes a single tier.
    pub trackers: Vec<Vec<String>>,
    /// BEP 19 web seeds (`url-list`).
    pub web_seeds: Vec<String>,
    /// BEP 17 HTTP seeds (`httpseeds`).
    pub http_seeds: Vec<String>,
    /// BEP 5 DHT bootstrap nodes.
    pub nodes: Vec<(String, u16)>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    /// Seconds since the Unix epoch.
    pub creation_date: Option<i64>,
    /// BEP 52 piece layers, keyed by `pieces root`. Each one has been checked against its
    /// root. Files whose layer is missing get it from peers, as for a magnet link.
    pub piece_layers: HashMap<Sha256Hash, Arc<[Sha256Hash]>>,
}

impl Torrent {
    /// Parse a `.torrent` file with the default limits.
    pub fn from_bytes(data: &[u8]) -> Result<Self, MetainfoError> {
        Self::from_bytes_with(data, &Limits::default())
    }

    pub fn from_bytes_with(data: &[u8], limits: &Limits) -> Result<Self, MetainfoError> {
        // Trailing bytes after the root dictionary (often a newline, sometimes junk from a
        // bad download tool) are ignored, as every major client does.
        let (root, _) = apitorrent_bencode::decode_prefix(data, &DecodeOptions::lenient())?;
        let root = root.as_dict().ok_or(MetainfoError::NotADictionary)?;
        let info = root.get_dict(b"info").ok_or(MetainfoError::MissingInfo)?;
        let info = Info::from_dict(info, limits)?;

        let piece_layers = if info.has_v2() {
            parse_piece_layers(root, &info)?
        } else {
            HashMap::new()
        };
        let multi_file = !info.is_single_file();

        let mut similar = info.similar.clone();
        let mut collections = info.collections.clone();
        merge_bep38(root, &mut similar, &mut collections);
        let mut info = info;
        info.similar = similar;
        info.collections = collections;

        Ok(Self {
            trackers: parse_trackers(root),
            web_seeds: parse_web_seeds(root, multi_file),
            http_seeds: string_list(root.get(b"httpseeds")),
            nodes: parse_nodes(root),
            comment: text(root, b"comment"),
            created_by: text(root, b"created by"),
            creation_date: root.get_int(b"creation date").filter(|&d| d > 0),
            piece_layers,
            info,
        })
    }

    pub fn info_hashes(&self) -> InfoHashes {
        self.info.info_hashes()
    }

    /// Whether every v2 file that needs a piece layer has one. When false, the missing
    /// layers must come from peers before those files can be verified.
    pub fn has_all_piece_layers(&self) -> bool {
        let piece_length = self.info.piece_length;
        self.info
            .files
            .iter()
            .filter(|f| f.length > piece_length)
            .filter_map(|f| f.pieces_root)
            .all(|root| self.piece_layers.contains_key(&root))
    }
}

fn text(dict: &Dict<'_>, key: &[u8]) -> Option<String> {
    let mut utf8_key = key.to_vec();
    utf8_key.extend_from_slice(b".utf-8");
    dict.get_bytes(&utf8_key)
        .or_else(|| dict.get_bytes(key))
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

fn clean_url(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    Some(s.replace(' ', "%20"))
}

fn string_list(value: Option<&Value<'_>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |raw: &[u8]| {
        if let Some(url) = clean_url(raw)
            && !out.contains(&url)
        {
            out.push(url);
        }
    };
    match value {
        Some(Value::Bytes(b)) => push(b),
        Some(Value::List(l)) => {
            for item in l {
                if let Some(b) = item.as_bytes() {
                    push(b);
                }
            }
        }
        _ => {}
    }
    out
}

fn parse_trackers(root: &Dict<'_>) -> Vec<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    let mut tiers: Vec<Vec<String>> = Vec::new();
    if let Some(list) = root.get_list(b"announce-list") {
        for tier in list {
            let urls: Vec<String> = string_list(Some(tier))
                .into_iter()
                .filter(|u| seen.insert(u.clone()))
                .collect();
            if !urls.is_empty() {
                tiers.push(urls);
            }
        }
    }
    // BEP 12: a client that understands `announce-list` ignores `announce`. Some creators
    // leave the main tracker out of the list, though, so keep it as a last tier if it is
    // new.
    if let Some(url) = root.get_bytes(b"announce").and_then(clean_url)
        && seen.insert(url.clone())
    {
        tiers.push(vec![url]);
    }
    tiers
}

fn parse_web_seeds(root: &Dict<'_>, multi_file: bool) -> Vec<String> {
    let mut seeds = string_list(root.get(b"url-list"));
    if multi_file {
        // BEP 19: for a multi-file torrent the URL names a directory, and file paths are
        // appended to it.
        for s in &mut seeds {
            if !s.ends_with('/') {
                s.push('/');
            }
        }
    }
    seeds
}

fn parse_nodes(root: &Dict<'_>) -> Vec<(String, u16)> {
    let Some(list) = root.get_list(b"nodes") else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|node| {
            let pair = node.as_list()?;
            let host = pair.get(0)?.as_str()?.to_owned();
            let port = u16::try_from(pair.get(1)?.as_int()?).ok()?;
            Some((host, port))
        })
        .collect()
}

fn merge_bep38(root: &Dict<'_>, similar: &mut Vec<crate::Sha1Hash>, collections: &mut Vec<String>) {
    if let Some(l) = root.get_list(b"similar") {
        for h in l
            .iter()
            .filter_map(|v| v.as_bytes().and_then(crate::Sha1Hash::from_slice))
        {
            if !similar.contains(&h) {
                similar.push(h);
            }
        }
    }
    if let Some(l) = root.get_list(b"collections") {
        for c in l.iter().filter_map(Value::as_str) {
            if !collections.iter().any(|x| x == c) {
                collections.push(c.to_owned());
            }
        }
    }
    collections.sort();
}

fn parse_piece_layers(
    root: &Dict<'_>,
    info: &Info,
) -> Result<HashMap<Sha256Hash, Arc<[Sha256Hash]>>, MetainfoError> {
    let mut out = HashMap::new();
    let Some(layers) = root.get(b"piece layers") else {
        return Ok(out);
    };
    let layers = layers
        .as_dict()
        .ok_or_else(|| MetainfoError::InvalidPieceLayer("not a dictionary".into()))?;
    for (key, value) in layers.iter() {
        let root_hash = Sha256Hash::from_slice(key)
            .ok_or_else(|| MetainfoError::InvalidPieceLayer("key is not a SHA-256 hash".into()))?;
        let index = info
            .files
            .iter()
            .position(|f| f.pieces_root == Some(root_hash))
            .ok_or_else(|| {
                MetainfoError::InvalidPieceLayer(format!("no file has root {root_hash}"))
            })?;
        let bytes = value
            .as_bytes()
            .ok_or_else(|| MetainfoError::InvalidPieceLayer("layer is not a byte string".into()))?;
        if bytes.len() % 32 != 0 {
            return Err(MetainfoError::InvalidPieceLayer(
                "layer is not a whole number of hashes".into(),
            ));
        }
        let layer: Vec<Sha256Hash> = bytes
            .as_chunks::<32>()
            .0
            .iter()
            .map(|c| Sha256Hash::from_slice(c).expect("chunk is 32 bytes"))
            .collect();
        if info.files[index].length <= info.piece_length {
            // BEP 52 has no layers for single-piece files; the root already covers them.
            if layer.len() != 1 || layer[0] != root_hash {
                return Err(MetainfoError::InvalidPieceLayer(format!(
                    "unexpected layer for single-piece file `{}`",
                    info.files[index].display_path()
                )));
            }
            continue;
        }
        info.validate_piece_layer(index, &layer)?;
        out.insert(root_hash, Arc::from(layer));
    }
    Ok(out)
}
