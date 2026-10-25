//! One-shot authentication against the Desktop Host.
//!
//! The Host prints an authenticated URL carrying a one-shot token. Requesting it
//! with a plain loopback `GET` answers `303` with a `set-cookie` header, and the
//! cookie value becomes the `cookie` header for every forwarded request and
//! upgrade.
//!
//! The exchange is written by hand instead of pulling an HTTP client into the
//! crate: the response is a single redirect and the body is drained and thrown
//! away. Nothing here logs or formats the token URL.

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use url::Url;

/// Largest response head accepted; a redirect head is far smaller.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Largest body drained before the connection is dropped.
const MAX_DRAIN_BYTES: usize = 256 * 1024;

/// Upper bound on the whole exchange.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why the Host authentication failed.
///
/// The variants never carry the token URL or the cookie, so an error can be
/// logged as-is.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The URL cannot be used for a loopback exchange.
    #[error("the desktop host authentication URL is unusable: {reason}")]
    Url {
        /// What is wrong with the URL.
        reason: &'static str,
    },
    /// The connection failed, or the Host did not answer in time.
    #[error("could not reach the desktop host: {source}")]
    Connect {
        /// Underlying transport failure.
        #[source]
        source: io::Error,
    },
    /// The status line or headers could not be parsed.
    #[error("the desktop host sent a malformed authentication response")]
    Malformed,
    /// The Host answered with something other than the one-shot redirect.
    #[error("the desktop host answered {status} instead of a 303 redirect")]
    NotRedirect {
        /// Status code that was received.
        status: u16,
    },
    /// The redirect carried no cookie.
    #[error("the desktop host did not set an authentication cookie")]
    MissingCookie,
}

/// Exchanges the Host launch URL for its authority-bound cookie.
///
/// Returns the cookie pair before the first `;`, exactly like the official
/// `authenticateWebHost`, so `Attributes` such as `SameSite` never reach the
/// forwarded requests.
pub async fn authenticate_host(url: &Url) -> Result<String, AuthError> {
    let loopback = url
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("localhost") || is_loopback_ip(host));
    if url.scheme() != "http" || !loopback {
        return Err(AuthError::Url {
            reason: "the URL is not a loopback http URL",
        });
    }
    let authority = crate::app_server::authority(url).ok_or(AuthError::Url {
        reason: "the URL has no host",
    })?;
    let target = match (url.path(), url.query()) {
        ("", None) => "/".to_owned(),
        (path, None) => path.to_owned(),
        (path, Some(query)) => format!("{path}?{query}"),
    };
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {authority}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );

    let exchange = async {
        let mut stream = TcpStream::connect(authority.as_str())
            .await
            .map_err(connect_error)?;
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(connect_error)?;
        stream.flush().await.map_err(connect_error)?;
        let head = read_head(&mut stream).await?;
        drain(&mut stream).await;
        parse_head(&head)
    };
    timeout(EXCHANGE_TIMEOUT, exchange)
        .await
        .map_err(|_| AuthError::Connect {
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                "the desktop host did not answer in time",
            ),
        })?
}

/// Whether `host` is a loopback IP literal.
fn is_loopback_ip(host: &str) -> bool {
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// Wraps a transport failure.
fn connect_error(source: io::Error) -> AuthError {
    AuthError::Connect { source }
}

/// Reads response bytes until the head terminator and returns them.
async fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>, AuthError> {
    let mut head = Vec::new();
    let mut buffer = [0u8; 2048];
    loop {
        if let Some(end) = head_end(&head) {
            head.truncate(end);
            return Ok(head);
        }
        if head.len() > MAX_HEAD_BYTES {
            return Err(AuthError::Malformed);
        }
        let read = stream.read(&mut buffer).await.map_err(connect_error)?;
        if read == 0 {
            // A connection that closes before the terminator still parsed as far
            // as it got; the parser rejects an incomplete head.
            return Err(AuthError::Malformed);
        }
        head.extend_from_slice(&buffer[..read]);
    }
}

/// Discards the redirect body; the connection closes afterwards.
async fn drain(stream: &mut TcpStream) {
    let mut buffer = [0u8; 4096];
    let mut drained = 0;
    loop {
        if drained >= MAX_DRAIN_BYTES {
            return;
        }
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => drained += read,
        }
    }
}

/// Offset just past the blank line that ends the response head.
fn head_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| start + 4)
}

/// Parses the status line and the cookie out of a complete response head.
fn parse_head(head: &[u8]) -> Result<String, AuthError> {
    let text = std::str::from_utf8(head).map_err(|_| AuthError::Malformed)?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or(AuthError::Malformed)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(AuthError::Malformed)?;
    if status != 303 {
        return Err(AuthError::NotRedirect { status });
    }
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("set-cookie") {
            let pair = value.split(';').next().unwrap_or_default().trim();
            if pair.is_empty() {
                return Err(AuthError::MissingCookie);
            }
            return Ok(pair.to_owned());
        }
    }
    Err(AuthError::MissingCookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_is_truncated_at_the_first_semicolon() {
        let head = b"HTTP/1.1 303 See Other\r\nset-cookie: dsh=abc123; Path=/; HttpOnly\r\nlocation: /\r\n\r\n";
        assert_eq!(parse_head(head).expect("cookie"), "dsh=abc123");
    }

    #[test]
    fn header_name_lookup_is_case_insensitive() {
        let head = b"HTTP/1.1 303 See Other\r\nSet-Cookie: token=1; SameSite=Strict\r\n\r\n";
        assert_eq!(parse_head(head).expect("cookie"), "token=1");
    }

    #[test]
    fn other_statuses_are_reported_as_is() {
        let head = b"HTTP/1.1 200 OK\r\nset-cookie: dsh=abc\r\n\r\n";
        match parse_head(head) {
            Err(AuthError::NotRedirect { status }) => assert_eq!(status, 200),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn a_redirect_without_a_cookie_is_rejected() {
        let head = b"HTTP/1.1 303 See Other\r\nlocation: /\r\n\r\n";
        assert!(matches!(parse_head(head), Err(AuthError::MissingCookie)));
    }

    #[test]
    fn malformed_status_lines_are_rejected() {
        assert!(matches!(
            parse_head(b"not http\r\n\r\n"),
            Err(AuthError::Malformed)
        ));
        assert!(matches!(
            parse_head(b"HTTP/1.1 abc OK\r\n\r\n"),
            Err(AuthError::Malformed)
        ));
    }

    #[test]
    fn only_loopback_urls_are_accepted() {
        assert!(is_loopback_ip("127.0.0.1"));
        assert!(is_loopback_ip("::1"));
        assert!(!is_loopback_ip("10.0.0.1"));
        assert!(!is_loopback_ip("example.com"));
    }
}
