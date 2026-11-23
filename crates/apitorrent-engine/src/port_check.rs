//! Asking a public service to connect back to our peer port, when the user asks.
//!
//! Each service takes only a port and connects to the address the request came from, so
//! the request has to come from the daemon: from a browser elsewhere it would test the
//! browser's network. One request per address family, bound to that family, tests each
//! public address. The answer is evidence for [`crate::reachability`]: a service that got
//! through proves the TCP port open, one that didn't says it's closed, until something
//! newer says otherwise.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// How long a service may take; each tries to connect back before it answers.
const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The least time between two checks with one service, to go easy on it.
pub const COOLDOWN: Duration = Duration::from_secs(30);

/// Why a port check couldn't be made.
#[derive(Debug, thiserror::Error)]
pub enum PortCheckError {
    #[error("force proxy is on, so nothing can connect in")]
    ForceProxy,
    #[error("the peer port isn't listening")]
    NotListening,
    #[error("{} was asked moments ago; try again in {secs} s", checker.host())]
    TooSoon { checker: PortChecker, secs: u64 },
}

/// A service that connects back to the port it's given, at the address asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortChecker {
    /// Transmission's port test: answers `1` (open) or `0`.
    Transmission,
    /// ifconfig.co's: answers JSON with `reachable`.
    Ifconfig,
}

impl PortChecker {
    pub const ALL: [PortChecker; 2] = [PortChecker::Transmission, PortChecker::Ifconfig];

    /// The service's host, as the user knows it.
    pub fn host(self) -> &'static str {
        match self {
            PortChecker::Transmission => "portcheck.transmissionbt.com",
            PortChecker::Ifconfig => "ifconfig.co",
        }
    }

    fn url(self, port: u16) -> String {
        match self {
            PortChecker::Transmission => format!("https://portcheck.transmissionbt.com/{port}"),
            PortChecker::Ifconfig => format!("https://ifconfig.co/port/{port}"),
        }
    }

    fn index(self) -> usize {
        match self {
            PortChecker::Transmission => 0,
            PortChecker::Ifconfig => 1,
        }
    }

    /// Whether the service got through, from its answer.
    fn parse(self, body: &str) -> Result<bool, String> {
        match self {
            PortChecker::Transmission => match body.trim() {
                "1" => Ok(true),
                "0" => Ok(false),
                other => Err(format!("unexpected answer {:?}", truncate(other))),
            },
            PortChecker::Ifconfig => {
                #[derive(serde::Deserialize)]
                struct Answer {
                    reachable: bool,
                }
                serde_json::from_str::<Answer>(body)
                    .map(|a| a.reachable)
                    .map_err(|_| format!("unexpected answer {:?}", truncate(body.trim())))
            }
        }
    }
}

/// The first bit of an answer, for an error message.
fn truncate(s: &str) -> String {
    s.chars().take(80).collect()
}

/// Ask `checker` whether `port` is open at our public address in one family (0 is IPv4).
pub(crate) async fn check(
    checker: PortChecker,
    port: u16,
    family: usize,
    user_agent: &str,
) -> Result<bool, String> {
    let local: IpAddr = if family == 0 {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv6Addr::UNSPECIFIED.into()
    };
    // Straight to the service, never through the proxy: it tests the address it sees.
    let client =
        apitorrent_tracker::strict_http_client_builder(user_agent, TIMEOUT, CONNECT_TIMEOUT)
            .map_err(|e| e.to_string())?
            .local_address(local)
            .build()
            .map_err(|e| e.to_string())?;
    let resp = client
        .get(checker.url(port))
        .send()
        .await
        .map_err(|e| describe(&e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{} answered {status}", checker.host()));
    }
    let body = resp.text().await.map_err(|e| describe(&e))?;
    checker.parse(&body)
}

/// A request error in a few words, with its cause (reqwest's own message is only "error
/// sending request").
fn describe(e: &apitorrent_tracker::reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg = format!("{msg}: {s}");
        source = s.source();
    }
    msg
}

/// When each service was last asked, to keep checks [`COOLDOWN`] apart.
#[derive(Debug, Default)]
pub(crate) struct Cooldown {
    last: std::sync::Mutex<[Option<std::time::Instant>; 2]>,
}

impl Cooldown {
    /// Take a turn with `checker`, or say how long until the next one.
    pub(crate) fn take(
        &self,
        checker: PortChecker,
        now: std::time::Instant,
    ) -> Result<(), Duration> {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = &mut last[checker.index()];
        if let Some(t) = *slot {
            let since = now.saturating_duration_since(t);
            if since < COOLDOWN {
                return Err(COOLDOWN.saturating_sub(since));
            }
        }
        *slot = Some(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        let t = PortChecker::Transmission;
        assert_eq!(t.parse("1"), Ok(true));
        assert_eq!(t.parse("0\n"), Ok(false));
        assert!(t.parse("<html>").is_err());
        let i = PortChecker::Ifconfig;
        assert_eq!(
            i.parse(r#"{"ip":"203.0.113.1","port":51413,"reachable":true}"#),
            Ok(true)
        );
        assert_eq!(
            i.parse("{\n  \"ip\": \"2001:db8::1\",\n  \"port\": 1,\n  \"reachable\": false\n}\n"),
            Ok(false)
        );
        assert!(i.parse("Too many requests").is_err());
    }

    #[test]
    fn urls_name_only_the_port() {
        assert_eq!(
            PortChecker::Transmission.url(51413),
            "https://portcheck.transmissionbt.com/51413"
        );
        assert_eq!(
            PortChecker::Ifconfig.url(6881),
            "https://ifconfig.co/port/6881"
        );
    }

    /// Asks both real services about port 1, which nothing listens on, over IPv4 and
    /// IPv6. Off by default: it needs the internet. `cargo test -p apitorrent-engine --lib
    /// live_port_check -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs the internet"]
    async fn live_port_check() {
        for checker in PortChecker::ALL {
            for family in [0, 1] {
                let answer = check(checker, 1, family, "apitorrent-test").await;
                println!("{} family {family}: {answer:?}", checker.host());
                if family == 0 {
                    assert_eq!(answer, Ok(false), "{}", checker.host());
                }
            }
        }
    }

    #[test]
    fn cooldown_keeps_checks_apart_per_service() {
        let c = Cooldown::default();
        let t0 = std::time::Instant::now();
        assert!(c.take(PortChecker::Transmission, t0).is_ok());
        assert!(c.take(PortChecker::Ifconfig, t0).is_ok(), "another service");
        let wait = c
            .take(PortChecker::Transmission, t0 + Duration::from_secs(10))
            .unwrap_err();
        assert_eq!(wait, Duration::from_secs(20));
        assert!(c.take(PortChecker::Transmission, t0 + COOLDOWN).is_ok());
    }
}
