//! Where releases come from: GitHub's latest *published* release, over strictly verified
//! HTTPS.
//!
//! `GET /repos/{owner}/{repo}/releases/latest` never returns a draft or a prerelease, so a
//! release the workflow drafted stays invisible until a person publishes it.

use std::time::Duration;

use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt as _;
use serde::Deserialize;
use url::Url;

use crate::UpdateError;

/// The repository releases come from.
pub const REPOSITORY: &str = "cinderblock/apitorrent";

/// Largest response read for release metadata, the manifest or a signature.
const MAX_METADATA: usize = 4 * 1024 * 1024;

/// Where to look for releases.
#[derive(Debug, Clone)]
pub struct Source {
    api_base: Url,
    repository: String,
    /// Tests only: plain HTTP to a loopback server.
    loopback_http: bool,
}

impl Source {
    /// GitHub, for this repository.
    pub fn github() -> Self {
        Self {
            api_base: Url::parse("https://api.github.com/").expect("valid"),
            repository: REPOSITORY.to_owned(),
            loopback_http: false,
        }
    }

    /// A GitHub-shaped server on loopback over plain HTTP, for tests. Signatures are
    /// checked exactly as in production; only the transport differs.
    #[doc(hidden)]
    pub fn loopback_for_tests(api_base: Url) -> Self {
        assert!(
            api_base.scheme() == "http"
                && matches!(
                    api_base.host(),
                    Some(url::Host::Ipv4(ip)) if ip.is_loopback()
                ),
            "test sources are plain HTTP on 127.0.0.1"
        );
        Self {
            api_base,
            repository: REPOSITORY.to_owned(),
            loopback_http: true,
        }
    }

    /// Whether a download URL (from a release or the signed manifest) may be fetched.
    pub(crate) fn allows(&self, url: &Url) -> bool {
        match url.scheme() {
            "https" => true,
            "http" => {
                self.loopback_http
                    && matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
            }
            _ => false,
        }
    }

    pub(crate) fn client(&self) -> Result<reqwest::Client, UpdateError> {
        let tls = ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| UpdateError::Network(e.to_string()))?
        .with_platform_verifier()
        .map_err(|e| UpdateError::Network(e.to_string()))?
        .with_no_client_auth();
        reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            // GitHub's download links redirect to its storage host; never to plain HTTP.
            .https_only(!self.loopback_http)
            .user_agent(concat!("apitorrentd/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| UpdateError::Network(e.to_string()))
    }

    fn latest_url(&self) -> Url {
        self.api_base
            .join(&format!("repos/{}/releases/latest", self.repository))
            .expect("valid path")
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    body: Option<String>,
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// The latest published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// From the tag (`v1.2.3` is `1.2.3`). Only for display and for "is there something
    /// newer": an install goes by the signed manifest's version.
    pub version: semver::Version,
    pub url: String,
    pub notes: Option<String>,
    pub published_ms: Option<i64>,
    /// Asset name to download URL.
    pub assets: Vec<(String, String)>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, u)| u.as_str())
    }
}

/// Read a whole response, refusing more than `max` bytes.
pub(crate) async fn read_limited(
    mut resp: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, UpdateError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?
    {
        if out.len() + chunk.len() > max {
            return Err(UpdateError::Network(format!(
                "response larger than {max} bytes"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// GET `url` (a release asset), at most [`MAX_METADATA`] bytes.
pub(crate) async fn fetch_small(
    source: &Source,
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<u8>, UpdateError> {
    let url = Url::parse(url).map_err(|e| UpdateError::Network(format!("{url}: {e}")))?;
    if !source.allows(&url) {
        return Err(UpdateError::Network(format!("refusing to fetch {url}")));
    }
    let resp = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| UpdateError::Network(format!("{url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(UpdateError::Network(format!(
            "{url}: HTTP {}",
            resp.status()
        )));
    }
    read_limited(resp, MAX_METADATA).await
}

/// The latest published release, or `None` when there is none yet.
pub async fn latest(
    source: &Source,
    client: &reqwest::Client,
) -> Result<Option<Release>, UpdateError> {
    let resp = client
        .get(source.latest_url())
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(UpdateError::Network(format!(
            "GitHub answered HTTP {}",
            resp.status()
        )));
    }
    let body = read_limited(resp, MAX_METADATA).await?;
    let r: ApiRelease = serde_json::from_slice(&body)
        .map_err(|e| UpdateError::Network(format!("unexpected release JSON: {e}")))?;
    let tag = r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name);
    let version = semver::Version::parse(tag).map_err(|_| {
        UpdateError::Network(format!("release tag {:?} isn't a version", r.tag_name))
    })?;
    Ok(Some(Release {
        version,
        url: r.html_url,
        notes: r.body.map(|b| shorten(&b, 16 * 1024)),
        published_ms: r.published_at.as_deref().and_then(parse_rfc3339_ms),
        assets: r
            .assets
            .into_iter()
            .map(|a| (a.name, a.browser_download_url))
            .collect(),
    }))
}

fn shorten(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// `2026-11-09T20:54:00Z` (GitHub's format: UTC, whole seconds) as Unix milliseconds.
#[allow(clippy::many_single_char_names)] // the textbook names of the date algorithm
pub(crate) fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(str::parse::<i64>);
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':').map(str::parse::<i64>);
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400) + hh * 3600 + mm * 60 + ss) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-11-09T20:54:00Z"),
            Some(1_794_257_640_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2000-02-29T12:00:00Z"),
            Some(951_825_600_000)
        );
        assert_eq!(parse_rfc3339_ms("2026-11-09T20:54:00"), None);
        assert_eq!(parse_rfc3339_ms("garbage"), None);
    }

    #[test]
    fn only_https_or_test_loopback() {
        let gh = Source::github();
        assert!(gh.allows(&Url::parse("https://github.com/x").unwrap()));
        assert!(!gh.allows(&Url::parse("http://127.0.0.1/x").unwrap()));
        assert!(!gh.allows(&Url::parse("file:///etc/passwd").unwrap()));
        let t = Source::loopback_for_tests(Url::parse("http://127.0.0.1:1/").unwrap());
        assert!(t.allows(&Url::parse("http://127.0.0.1:2/x").unwrap()));
        assert!(!t.allows(&Url::parse("http://example.com/x").unwrap()));
    }

    #[test]
    fn shortens_on_a_char_boundary() {
        assert_eq!(shorten("abc", 5), "abc");
        assert_eq!(shorten("ééé", 3), "é…");
    }
}
