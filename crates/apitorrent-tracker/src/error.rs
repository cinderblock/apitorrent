use std::error::Error as StdError;

/// Why a tracker request failed.
///
/// The variants separate what the tracker said ([`TrackerError::Failure`]) from what went
/// wrong on the way ([`TrackerError::Network`], [`TrackerError::Timeout`]) and from
/// responses we refused to trust ([`TrackerError::Malformed`],
/// [`TrackerError::ResponseTooLarge`]). The engine uses this to decide how soon to retry.
#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    /// The tracker answered, and refused the request: the HTTP `failure reason`, or a BEP 15
    /// error packet.
    #[error("tracker reported failure: {0}")]
    Failure(String),
    /// The response could not be parsed, or broke the protocol.
    #[error("malformed tracker response: {0}")]
    Malformed(String),
    /// The tracker did not answer in time (after every UDP retransmission, or within the
    /// HTTP request timeout).
    #[error("tracker timed out")]
    Timeout,
    /// The connection failed: DNS, TCP, UDP, TLS (including a certificate that failed
    /// verification), or a refused redirect.
    #[error("network error: {0}")]
    Network(#[source] Box<dyn StdError + Send + Sync>),
    /// The tracker answered over HTTP with a non-success status and no bencoded failure.
    #[error("tracker returned HTTP status {0}")]
    HttpStatus(u16),
    /// The response body was larger than the configured limit.
    #[error("tracker response is larger than {limit} bytes")]
    ResponseTooLarge { limit: usize },
    /// The announce URL's scheme is not `http`, `https` or `udp`.
    #[error("unsupported tracker URL scheme {0:?}")]
    UnsupportedScheme(String),
    /// The announce URL could not be parsed, or lacks a host or port.
    #[error("invalid tracker URL: {0}")]
    InvalidUrl(String),
    /// The tracker's URL does not follow the BEP 48 convention, so it has no scrape URL.
    #[error("tracker does not support scrape")]
    ScrapeUnsupported,
    /// The request itself is not valid, such as a scrape with no info-hashes.
    #[error("invalid tracker request: {0}")]
    InvalidRequest(&'static str),
}

impl TrackerError {
    pub(crate) fn malformed(msg: impl Into<String>) -> Self {
        Self::Malformed(msg.into())
    }

    pub(crate) fn network(e: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self::Network(e.into())
    }

    /// Whether this came from the tracker itself rather than from the path to it. A tracker
    /// that answered is reachable, so the engine can keep its normal announce interval.
    pub fn is_tracker_failure(&self) -> bool {
        matches!(self, Self::Failure(_))
    }
}

impl From<reqwest::Error> for TrackerError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Timeout
        } else {
            Self::network(e)
        }
    }
}
