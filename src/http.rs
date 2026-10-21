//! HTTP helpers shared by the Jira, Confluence, Gerrit and EXA Reporter clients.

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::fmt;
use std::time::Duration;
use tracing::warn;
use url::Url;

/// Request timeout for Jira, Confluence and Gerrit. A Jira search page of 100
/// issues with links takes 5-20 s; 90 s leaves room for a slow moment.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(90);
/// Longest error-body excerpt kept in an error message.
const ERROR_BODY_CHARS: usize = 200;
/// Tries per request: rate limits and gateway errors on Atlassian cloud
/// usually clear within seconds, so two retries are worth it before failing.
const ATTEMPTS: u32 = 3;
/// Longest wait honoured from a `Retry-After` header.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// A reply with an HTTP error status.
#[derive(Debug)]
pub(crate) struct StatusError {
    /// Path of the request.
    pub(crate) path: String,
    /// HTTP status code.
    pub(crate) code: u16,
    /// Start of the reply body.
    pub(crate) excerpt: String,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GET {}: HTTP {}: {}", self.path, self.code, self.excerpt)
    }
}

impl std::error::Error for StatusError {}

/// The HTTP status of a failed request, if it got that far.
pub(crate) fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<StatusError>().map(|s| s.code)
}

/// A blocking HTTP agent with the standard timeout.
pub(crate) fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(TIMEOUT).build()
}

/// `Authorization` header value for HTTP basic auth.
pub(crate) fn basic_auth(user: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
}

/// GETs `url` and returns the body, retrying rate limits, gateway errors and
/// refused or dropped connections, but not timeouts: a server that took the
/// whole timeout once will likely take it again. Errors name the path and
/// HTTP status; the credentials in `auth` never appear in them.
pub(crate) fn get_text(agent: &ureq::Agent, url: &Url, auth: Option<&str>) -> Result<String> {
    let mut attempt = 1;
    loop {
        let mut request = agent
            .request_url("GET", url)
            .set("Accept", "application/json");
        if let Some(auth) = auth {
            request = request.set("Authorization", auth);
        }
        let wait = match request.call() {
            Ok(response) => {
                return response
                    .into_string()
                    .with_context(|| format!("reading the reply from {}", url.path()));
            }
            Err(ureq::Error::Status(code, response)) => {
                let wait = retry_after(response.header("Retry-After"));
                if attempt >= ATTEMPTS || !retry_status(code) {
                    let body = response
                        .into_string()
                        .unwrap_or_else(|e| format!("(body unreadable: {e})"));
                    let excerpt = body.chars().take(ERROR_BODY_CHARS).collect();
                    return Err(StatusError {
                        path: url.path().to_string(),
                        code,
                        excerpt,
                    }
                    .into());
                }
                warn!("GET {}: HTTP {code}, retrying", url.path());
                wait
            }
            Err(ureq::Error::Transport(t)) => {
                let io = std::error::Error::source(&t)
                    .and_then(|e| e.downcast_ref::<std::io::Error>())
                    .map(std::io::Error::kind);
                if attempt >= ATTEMPTS || !retry_transport(t.kind(), io) {
                    return Err(anyhow::anyhow!("GET {}: {t}", url.path()));
                }
                warn!("GET {}: {t}, retrying", url.path());
                None
            }
        };
        std::thread::sleep(wait.unwrap_or_else(|| Duration::from_secs(2u64.pow(attempt))));
        attempt += 1;
    }
}

/// Rate limits and gateway errors clear by themselves; other statuses do not.
fn retry_status(code: u16) -> bool {
    matches!(code, 429 | 502 | 503 | 504)
}

/// A refused or dropped connection is worth another try. A timeout is not,
/// and neither is a failed TLS handshake (a certificate that does not match
/// the pin fails the same way every time); ureq reports a connect timeout and
/// a TLS failure as `ConnectionFailed` too, so the I/O error decides.
fn retry_transport(kind: ureq::ErrorKind, io: Option<std::io::ErrorKind>) -> bool {
    use std::io::ErrorKind as Io;
    match kind {
        ureq::ErrorKind::ConnectionFailed => matches!(
            io,
            Some(Io::ConnectionRefused | Io::ConnectionReset | Io::ConnectionAborted)
        ),
        ureq::ErrorKind::Io => matches!(
            io,
            Some(Io::ConnectionReset | Io::ConnectionAborted | Io::BrokenPipe | Io::UnexpectedEof)
        ),
        _ => false,
    }
}

/// The wait a `Retry-After: <seconds>` header asks for, capped.
fn retry_after(header: Option<&str>) -> Option<Duration> {
    header?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|s| Duration::from_secs(s).min(MAX_RETRY_AFTER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(429, true ; "rate limited")]
    #[test_case(503, true ; "unavailable")]
    #[test_case(400, false ; "bad query")]
    #[test_case(401, false ; "bad credentials")]
    #[test_case(500, false ; "server bug")]
    fn which_statuses_are_retried(code: u16, retried: bool) {
        assert_eq!(retry_status(code), retried);
    }

    #[test_case(ureq::ErrorKind::ConnectionFailed, Some(std::io::ErrorKind::ConnectionRefused), true ; "refused")]
    #[test_case(ureq::ErrorKind::ConnectionFailed, Some(std::io::ErrorKind::ConnectionReset), true ; "reset during the handshake")]
    #[test_case(ureq::ErrorKind::ConnectionFailed, Some(std::io::ErrorKind::TimedOut), false ; "connect timeout")]
    #[test_case(ureq::ErrorKind::ConnectionFailed, Some(std::io::ErrorKind::InvalidData), false ; "TLS failure")]
    #[test_case(ureq::ErrorKind::ConnectionFailed, None, false ; "no cause given")]
    #[test_case(ureq::ErrorKind::Io, Some(std::io::ErrorKind::ConnectionReset), true ; "dropped")]
    #[test_case(ureq::ErrorKind::Io, Some(std::io::ErrorKind::TimedOut), false ; "timed out")]
    #[test_case(ureq::ErrorKind::Io, Some(std::io::ErrorKind::WouldBlock), false ; "read timeout")]
    #[test_case(ureq::ErrorKind::Dns, None, false ; "unknown host")]
    fn which_transport_errors_are_retried(
        kind: ureq::ErrorKind,
        io: Option<std::io::ErrorKind>,
        retried: bool,
    ) {
        assert_eq!(retry_transport(kind, io), retried);
    }

    #[test]
    fn basic_auth_encodes_user_and_password() {
        assert_eq!(
            basic_auth("Aladdin", "open sesame"),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test_case(Some("7"), Some(Duration::from_secs(7)) ; "seconds")]
    #[test_case(Some("3600"), Some(MAX_RETRY_AFTER) ; "capped")]
    #[test_case(Some("Wed, 21 Oct 2026 07:28:00 GMT"), None ; "http date is not honoured")]
    #[test_case(None, None ; "absent")]
    fn retry_after_reads_seconds(header: Option<&str>, expected: Option<Duration>) {
        assert_eq!(retry_after(header), expected);
    }

    #[test]
    fn status_errors_keep_their_code_through_context() {
        let e: anyhow::Error = StatusError {
            path: "/rest".into(),
            code: 400,
            excerpt: "bad".into(),
        }
        .into();
        let e = e.context("reading linked fixes");

        assert_eq!(status_of(&e), Some(400));
        assert_eq!(
            format!("{e:#}"),
            "reading linked fixes: GET /rest: HTTP 400: bad"
        );
        assert_eq!(status_of(&anyhow::anyhow!("other")), None);
    }
}
