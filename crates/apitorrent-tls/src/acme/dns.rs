//! The DNS-01 hook: how a `dns-provider` plugin publishes ACME's TXT records.
//!
//! The ACME state machine stays in the core; only "publish this TXT record" and "remove
//! it" are pluggable, so the TLS listener always owns its certificates and keys. No
//! provider is built in.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

/// A boxed future, so [`DnsProvider`] can be used as a trait object.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One DNS-01 TXT record (RFC 8555 §8.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtRecord {
    /// The domain being validated, without any wildcard prefix (`example.com` for
    /// `*.example.com`).
    pub domain: String,
    /// The full record name: `_acme-challenge.<domain>`, without a trailing dot.
    pub name: String,
    /// The record's value: the base64url SHA-256 of the key authorization.
    pub value: String,
}

/// A DNS provider failure, as text for the log and the API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DnsError(pub String);

/// Publishes and removes the TXT records of DNS-01 challenges.
///
/// Implementations must allow several records with the same name at once (a wildcard and
/// its base domain share `_acme-challenge.<domain>`), and must only remove the record with
/// the given value.
pub trait DnsProvider: Send + Sync + fmt::Debug {
    /// Create the record, resolving once it is published *and* visible on the zone's
    /// authoritative name servers. The ACME server is told to validate as soon as this
    /// returns, and a validator that can't see the record fails the order.
    fn publish_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), DnsError>>;

    /// Delete the record. Called after validation, whether it succeeded or failed.
    fn remove_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), DnsError>>;
}

impl TxtRecord {
    pub(crate) fn for_domain(domain: &str, value: String) -> Self {
        let domain = domain
            .trim_start_matches("*.")
            .trim_end_matches('.')
            .to_ascii_lowercase();
        Self {
            name: format!("_acme-challenge.{domain}"),
            domain,
            value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_names() {
        let r = TxtRecord::for_domain("*.Example.com.", "v".into());
        assert_eq!(r.domain, "example.com");
        assert_eq!(r.name, "_acme-challenge.example.com");
    }
}
