//! Serving the web UI embedded in the binary (`apitorrent-webui`).
//!
//! - Content negotiation: the build's brotli or gzip copy when the client accepts it, with
//!   `Vary: Accept-Encoding` and a distinct entity tag per encoding.
//! - Conditional requests: `If-None-Match` answers `304 Not Modified`.
//! - Caching: Vite's content-hashed `/assets/*` files are immutable for a year. Everything
//!   else, `index.html` above all, is revalidated on every load, so a new release shows up
//!   at once.
//! - Client-side routes: an unknown path outside `/assets/` gets `index.html`. A missing
//!   asset is a real 404, so a stale page doesn't run HTML as a script.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use apitorrent_webui::{Asset, Bundle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Identity,
    Gzip,
    Brotli,
}

pub(crate) fn serve(
    bundle: &'static Bundle,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, HeaderValue::from_static("GET, HEAD"))],
        )
            .into_response();
    }
    let path = uri.path();
    let asset = match bundle.get(path) {
        Some(a) => a,
        None if path == "/" || !path.starts_with("/assets/") => bundle.index(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    respond(asset, method, headers)
}

fn respond(asset: &'static Asset, method: &Method, headers: &HeaderMap) -> Response {
    let (encoding, body) = choose(asset, headers);
    let etag = match encoding {
        Encoding::Identity => format!("\"{}\"", asset.etag),
        Encoding::Gzip => format!("\"{}-gz\"", asset.etag),
        Encoding::Brotli => format!("\"{}-br\"", asset.etag),
    };
    let cache = if asset.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    let mut resp = Response::builder()
        .header(header::CACHE_CONTROL, cache)
        .header(header::ETAG, &etag);
    if asset.br.is_some() || asset.gzip.is_some() {
        resp = resp.header(header::VARY, "Accept-Encoding");
    }
    if not_modified(headers, &etag) {
        return resp
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .expect("valid response");
    }
    resp = resp
        .header(header::CONTENT_TYPE, asset.content_type)
        .header(header::CONTENT_LENGTH, body.len());
    match encoding {
        Encoding::Identity => {}
        Encoding::Gzip => resp = resp.header(header::CONTENT_ENCODING, "gzip"),
        Encoding::Brotli => resp = resp.header(header::CONTENT_ENCODING, "br"),
    }
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(body)
    };
    resp.body(body).expect("valid response")
}

/// The best copy the client accepts: brotli, then gzip, then the raw bytes.
fn choose(asset: &'static Asset, headers: &HeaderMap) -> (Encoding, &'static [u8]) {
    let accepted = accepted_encodings(headers);
    if let Some(br) = asset.br
        && accepted.contains(&Encoding::Brotli)
    {
        return (Encoding::Brotli, br);
    }
    if let Some(gz) = asset.gzip
        && accepted.contains(&Encoding::Gzip)
    {
        return (Encoding::Gzip, gz);
    }
    (Encoding::Identity, asset.raw)
}

/// The encodings named in `Accept-Encoding` with a nonzero quality. Preference among them is
/// ours (smallest first): browsers weight them equally in practice.
fn accepted_encodings(headers: &HeaderMap) -> Vec<Encoding> {
    let mut out = Vec::new();
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else { continue };
        for item in value.split(',') {
            let mut parts = item.split(';');
            let name = parts.next().unwrap_or("").trim();
            let refused = parts.any(|p| {
                p.trim()
                    .strip_prefix("q=")
                    .and_then(|q| q.trim().parse::<f32>().ok())
                    .is_some_and(|q| q <= 0.0)
            });
            if refused {
                continue;
            }
            match name.to_ascii_lowercase().as_str() {
                "br" => out.push(Encoding::Brotli),
                "gzip" | "x-gzip" => out.push(Encoding::Gzip),
                _ => {}
            }
        }
    }
    out
}

/// `If-None-Match` matches with the weak comparison (RFC 9110 13.1.2), as required for it.
fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    headers.get_all(header::IF_NONE_MATCH).iter().any(|v| {
        v.to_str().is_ok_and(|v| {
            v.split(',').any(|tag| {
                let tag = tag.trim();
                tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn parses_accept_encoding() {
        let h = headers(&[(header::ACCEPT_ENCODING, "gzip, deflate, br;q=0")]);
        assert_eq!(accepted_encodings(&h), vec![Encoding::Gzip]);
        let h = headers(&[(header::ACCEPT_ENCODING, "br;q=1.0, GZIP;q=0.5")]);
        assert_eq!(
            accepted_encodings(&h),
            vec![Encoding::Brotli, Encoding::Gzip]
        );
        assert!(accepted_encodings(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn if_none_match() {
        let tag = "\"abc\"";
        assert!(not_modified(
            &headers(&[(header::IF_NONE_MATCH, "\"x\", W/\"abc\"")]),
            tag
        ));
        assert!(not_modified(&headers(&[(header::IF_NONE_MATCH, "*")]), tag));
        assert!(!not_modified(
            &headers(&[(header::IF_NONE_MATCH, "\"abc-gz\"")]),
            tag
        ));
        assert!(!not_modified(&HeaderMap::new(), tag));
    }
}
