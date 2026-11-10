//! The signed daemon manifest: which binary to download for each target, and its hash.
//!
//! The release workflow writes `apitorrentd-manifest.json` and signs it into
//! `apitorrentd-manifest.json.sig`. An install trusts nothing outside the signed bytes:
//! the version, target, URL, size and SHA-256 all come from here.

use serde::{Deserialize, Serialize};

/// The manifest's asset name in a release.
pub const MANIFEST_NAME: &str = "apitorrentd-manifest.json";
/// Its signature's asset name.
pub const SIGNATURE_NAME: &str = "apitorrentd-manifest.json.sig";
/// `product` in every daemon manifest, so no other signed JSON passes for one.
pub const PRODUCT: &str = "apitorrentd";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub product: String,
    pub version: String,
    /// When the release was built, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pub_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    pub assets: Vec<ManifestAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestAsset {
    /// A Rust target triple, such as `x86_64-unknown-linux-gnu`.
    pub target: String,
    /// Where the bare executable is (not an archive).
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("the manifest isn't valid JSON: {0}")]
    Json(String),
    #[error("the manifest is for {0:?}, not apitorrentd")]
    WrongProduct(String),
    #[error("the manifest's version {0:?} isn't a semantic version")]
    BadVersion(String),
    #[error("the manifest has no build for {0}")]
    NoTarget(String),
    #[error("the manifest's entry for {0} is malformed: {1}")]
    BadAsset(String, String),
}

impl Manifest {
    /// Parse already-verified bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let m: Self =
            serde_json::from_slice(bytes).map_err(|e| ManifestError::Json(e.to_string()))?;
        if m.product != PRODUCT {
            return Err(ManifestError::WrongProduct(m.product));
        }
        m.semver()?;
        Ok(m)
    }

    pub fn semver(&self) -> Result<semver::Version, ManifestError> {
        semver::Version::parse(&self.version)
            .map_err(|_| ManifestError::BadVersion(self.version.clone()))
    }

    /// The build for `target`, checked for a well-formed hash and an absolute URL.
    pub fn asset(&self, target: &str) -> Result<&ManifestAsset, ManifestError> {
        let a = self
            .assets
            .iter()
            .find(|a| a.target == target)
            .ok_or_else(|| ManifestError::NoTarget(target.to_owned()))?;
        let bad = |why: &str| ManifestError::BadAsset(target.to_owned(), why.to_owned());
        if a.sha256.len() != 64
            || !a
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(bad("sha256 must be 64 lowercase hex digits"));
        }
        if a.size == 0 {
            return Err(bad("size is zero"));
        }
        url::Url::parse(&a.url).map_err(|e| bad(&format!("url: {e}")))?;
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            product: PRODUCT.to_owned(),
            version: "1.2.3".to_owned(),
            pub_date: None,
            notes_url: None,
            assets: vec![ManifestAsset {
                target: "x86_64-unknown-linux-gnu".to_owned(),
                url: "https://example.com/apitorrentd".to_owned(),
                sha256: "ab".repeat(32),
                size: 10,
            }],
        }
    }

    #[test]
    fn round_trip_and_checks() {
        let m = sample();
        let bytes = serde_json::to_vec(&m).unwrap();
        assert_eq!(Manifest::parse(&bytes).unwrap(), m);
        assert!(m.asset("x86_64-unknown-linux-gnu").is_ok());
        assert_eq!(
            m.asset("riscv64gc-unknown-linux-gnu"),
            Err(ManifestError::NoTarget(
                "riscv64gc-unknown-linux-gnu".into()
            ))
        );

        let mut other = sample();
        other.product = "something-else".into();
        assert!(matches!(
            Manifest::parse(&serde_json::to_vec(&other).unwrap()),
            Err(ManifestError::WrongProduct(_))
        ));
        let mut v = sample();
        v.version = "latest".into();
        assert!(matches!(
            Manifest::parse(&serde_json::to_vec(&v).unwrap()),
            Err(ManifestError::BadVersion(_))
        ));
        let mut h = sample();
        h.assets[0].sha256 = "AB".repeat(32);
        assert!(matches!(
            h.asset("x86_64-unknown-linux-gnu"),
            Err(ManifestError::BadAsset(..))
        ));
    }
}
