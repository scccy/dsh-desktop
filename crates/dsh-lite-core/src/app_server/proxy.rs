//! Request forwarding to the authenticated Desktop Host.
//!
//! This is the Rust counterpart of `forwardWebRequest` in
//! `apps/desktop/src/web-document.ts`: the Host-bound credentials replace
//! whatever the renderer sent, connection-level framing and cookie headers are
//! kept on this side of the proxy, and the response body is streamed so server
//! sent events reach the page as they arrive.
//!
//! Two deliberate differences from the Node implementation:
//!
//! * `accept-encoding` is dropped instead of decoded. The Host answers with an
//!   identity body, so the withheld `content-encoding` header can never describe
//!   a body the renderer did not receive.
//! * the upstream request is sent over a fresh loopback connection with raw
//!   hyper, so no client library can follow a `3xx` or buffer a chunked body.

use std::io;
use std::time::Duration;

use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::time::timeout;
use url::Url;

use crate::app_server::{AppBody, AppState};

/// How long a fresh Host connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Request headers never relayed to the Host.
///
/// The first four are replaced by the Host-bound values (`host`, `origin`,
/// `cookie`) or dropped (`sec-fetch-site`); the rest describe this proxy's own
/// connection, and `accept-encoding`/`expect` are handled by this hop.
const DROPPED_REQUEST_HEADERS: [&str; 14] = [
    "host",
    "origin",
    "cookie",
    "sec-fetch-site",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
    "accept-encoding",
    "expect",
];

/// Response headers never relayed to the renderer.
///
/// `set-cookie` would hand the Host's authentication cookie to the page's cookie
/// jar, which the shell owns instead; the remaining names describe the upstream
/// connection (its encoding, length and hop-by-hop transport).
const WITHHELD_RESPONSE_HEADERS: [&str; 11] = [
    "set-cookie",
    "content-encoding",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
];

/// Host routes whose immutable cache headers are useless across launches.
const PLUGIN_BUNDLE_PREFIX: &str = "/plugins/";

/// Whether an inbound request header is relayed to the Host.
pub(crate) fn forwards_request_header(name: &HeaderName) -> bool {
    !DROPPED_REQUEST_HEADERS
        .iter()
        .any(|dropped| name.as_str().eq_ignore_ascii_case(dropped))
}

/// Whether an upstream response header is relayed to the renderer.
pub(crate) fn forwards_response_header(name: &HeaderName) -> bool {
    !WITHHELD_RESPONSE_HEADERS
        .iter()
        .any(|withheld| name.as_str().eq_ignore_ascii_case(withheld))
}

/// Whether `pathname` is a plugin bundle, which is always served `no-store`.
pub(crate) fn is_plugin_bundle(pathname: &str) -> bool {
    pathname.starts_with(PLUGIN_BUNDLE_PREFIX)
}

/// Whether the response is a server-sent event stream.
pub(crate) fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

/// Absolute request URI for `request_uri` on the Host origin.
pub(crate) fn upstream_uri(origin: &Url, request_uri: &Uri) -> Option<Uri> {
    let authority = crate::app_server::authority(origin)?;
    let path_and_query = request_uri
        .path_and_query()
        .map_or("/", |path_and_query| path_and_query.as_str());
    format!("{}://{authority}{path_and_query}", origin.scheme())
        .parse()
        .ok()
}

/// Forwards one renderer request to the Host and streams the answer back.
pub(crate) async fn forward(request: Request<Incoming>, state: &AppState) -> Response<AppBody> {
    let method = request.method().clone();
    let pathname = request.uri().path().to_owned();
    let Some(uri) = upstream_uri(state.host_origin(), request.uri()) else {
        state.debug(format!(
            "app_server: {method} {pathname} -> 502 (no host uri)"
        ));
        return crate::app_server::status_response(StatusCode::BAD_GATEWAY);
    };
    let Some(authority) = crate::app_server::authority(state.host_origin()) else {
        return crate::app_server::status_response(StatusCode::BAD_GATEWAY);
    };
    let cookie = match HeaderValue::from_str(state.cookie()) {
        Ok(cookie) => cookie,
        Err(_) => {
            state.debug(format!(
                "app_server: {method} {pathname} -> 500 (unusable cookie)"
            ));
            return crate::app_server::status_response(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let (parts, body) = request.into_parts();
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if forwards_request_header(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    let Ok(host) = HeaderValue::from_str(&authority) else {
        return crate::app_server::status_response(StatusCode::BAD_GATEWAY);
    };
    headers.insert(header::HOST, host);
    headers.insert(header::COOKIE, cookie);

    let mut outgoing = Request::new(body);
    *outgoing.method_mut() = parts.method;
    *outgoing.uri_mut() = uri;
    *outgoing.version_mut() = Version::HTTP_11;
    *outgoing.headers_mut() = headers;

    let stream = match connect(state.host_origin()).await {
        Ok(stream) => stream,
        Err(error) => {
            state.debug(format!(
                "app_server: {method} {pathname} -> 502 (connect: {error})"
            ));
            return crate::app_server::status_response(StatusCode::BAD_GATEWAY);
        }
    };
    let (mut sender, connection) =
        match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
            Ok(handshake) => handshake,
            Err(error) => {
                state.debug(format!(
                    "app_server: {method} {pathname} -> 502 (handshake: {error})"
                ));
                return crate::app_server::status_response(StatusCode::BAD_GATEWAY);
            }
        };
    // The connection future drives reads for the response body; the request
    // future reports every failure the caller can act on.
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let upstream = match sender.send_request(outgoing).await {
        Ok(upstream) => upstream,
        Err(error) => {
            state.debug(format!(
                "app_server: {method} {pathname} -> 502 (request: {error})"
            ));
            return crate::app_server::status_response(StatusCode::BAD_GATEWAY);
        }
    };

    let status = upstream.status();
    let mut headers = HeaderMap::new();
    for (name, value) in upstream.headers() {
        if forwards_response_header(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    if is_plugin_bundle(&pathname) {
        headers.remove(header::CACHE_CONTROL);
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    // The withheld `content-length` turns every relayed answer into a chunked
    // stream; an event stream must also be marked uncacheable, because WebKit
    // otherwise keeps the open response in its HTTP cache entry.
    if is_event_stream(&headers) && !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    let mut response = Response::new(crate::app_server::incoming_body(upstream.into_body()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    state.debug(format!(
        "app_server: {method} {pathname} -> {} (host)",
        status.as_u16()
    ));
    response
}

/// Opens a fresh loopback connection to the Host origin.
async fn connect(origin: &Url) -> io::Result<TcpStream> {
    if origin.scheme() != "http" {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the host origin is not plain http",
        ));
    }
    let authority = crate::app_server::authority(origin).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "the host origin has no host")
    })?;
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(authority.as_str()))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "connecting to the host timed out")
        })??;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_bound_request_headers_are_replaced_or_dropped() {
        for name in ["host", "origin", "cookie", "sec-fetch-site"] {
            assert!(!forwards_request_header(
                &HeaderName::from_bytes(name.as_bytes()).expect("header name")
            ));
        }
    }

    #[test]
    fn hop_by_hop_request_headers_are_dropped() {
        for name in [
            "connection",
            "keep-alive",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "proxy-authenticate",
            "proxy-authorization",
        ] {
            assert!(
                !forwards_request_header(
                    &HeaderName::from_bytes(name.as_bytes()).expect("header name")
                ),
                "{name} must not reach the host"
            );
        }
    }

    #[test]
    fn content_headers_survive_the_request_hop() {
        for name in [
            "content-type",
            "content-length",
            "accept",
            "x-request-id",
            "authorization",
        ] {
            assert!(
                forwards_request_header(
                    &HeaderName::from_bytes(name.as_bytes()).expect("header name")
                ),
                "{name} must reach the host"
            );
        }
    }

    #[test]
    fn connection_and_cookie_response_headers_are_withheld() {
        for name in [
            "set-cookie",
            "content-encoding",
            "content-length",
            "transfer-encoding",
            "connection",
            "keep-alive",
            "te",
            "trailer",
            "upgrade",
            "proxy-authenticate",
            "proxy-authorization",
        ] {
            assert!(
                !forwards_response_header(
                    &HeaderName::from_bytes(name.as_bytes()).expect("header name")
                ),
                "{name} must not reach the renderer"
            );
        }
        for name in ["content-type", "cache-control", "etag", "last-modified"] {
            assert!(
                forwards_response_header(
                    &HeaderName::from_bytes(name.as_bytes()).expect("header name")
                ),
                "{name} must reach the renderer"
            );
        }
    }

    #[test]
    fn event_streams_are_detected_with_parameters() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(is_event_stream(&headers));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert!(!is_event_stream(&headers));
    }

    #[test]
    fn only_plugin_routes_are_treated_as_bundles() {
        assert!(is_plugin_bundle("/plugins/x.js"));
        assert!(is_plugin_bundle("/plugins/"));
        assert!(!is_plugin_bundle("/plugin/x.js"));
        assert!(!is_plugin_bundle("/api/plugins/x.js"));
    }

    #[test]
    fn upstream_uri_keeps_path_and_query_and_drops_the_origin() {
        let origin = Url::parse("http://127.0.0.1:4100").expect("origin");
        let request: Uri = "/api/sessions?limit=5".parse().expect("uri");
        assert_eq!(
            upstream_uri(&origin, &request)
                .expect("absolute uri")
                .to_string(),
            "http://127.0.0.1:4100/api/sessions?limit=5"
        );
        let root: Uri = "/".parse().expect("uri");
        assert_eq!(
            upstream_uri(&origin, &root)
                .expect("absolute uri")
                .to_string(),
            "http://127.0.0.1:4100/"
        );
    }
}
