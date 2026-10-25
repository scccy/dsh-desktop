//! End-to-end tests of the loopback application server.
//!
//! Every test starts the real server against a mock Desktop Host that records
//! what reached it, then talks to the server over loopback HTTP/1.1 or
//! WebSocket exactly like the renderer would.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{accept_hdr_async, connect_async};
use url::Url;

use dsh_lite_core::app_server::{
    AppServer, AppServerConfig, AuthError, BOOT_SCRIPT, RENDER_ORIGIN, ShellAsset,
    authenticate_host,
};

/// Cookie the mock Host issues and the server must inject.
const COOKIE: &str = "dsh=secret-value";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Packaged frontend layout the static routes expect.
fn dist_fixture() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("temp dir");
    std::fs::create_dir_all(dir.path().join("assets")).expect("assets dir");
    std::fs::write(
        dir.path().join("index.html"),
        "<!doctype html>\n<html lang=\"en\">\n  <head>\n    <meta charset=\"utf-8\" />\n  </head>\n  <body>app</body>\n</html>\n",
    )
    .expect("index");
    std::fs::write(dir.path().join("assets/app-abc.js"), "console.log('app')\n").expect("js");
    std::fs::write(dir.path().join("assets/app-abc.css"), "body{}\n").expect("css");
    std::fs::write(dir.path().join("assets/data.bin"), [0u8, 1, 2, 3]).expect("bin");
    std::fs::write(dir.path().join("favicon.svg"), "<svg />").expect("favicon");
    std::fs::write(dir.path().join("favicon-dark.svg"), "<svg />").expect("favicon dark");
    std::fs::write(dir.path().join("manifest.webmanifest"), "{}").expect("manifest");
    dir
}

/// One request the mock Host received.
#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    target: String,
    headers: HeaderMap,
    body: Bytes,
}

impl SeenRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// Mock Desktop Host: records requests and answers the routes the tests need.
struct MockHost {
    origin: Url,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl MockHost {
    async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    return;
                };
                let record = record.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let record = record.clone();
                        async move { Ok::<_, Infallible>(host_response(request, record).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            origin: Url::parse(&format!("http://{addr}")).expect("origin"),
            seen,
        }
    }

    fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("seen").clone()
    }

    fn last(&self) -> SeenRequest {
        self.seen().pop().expect("the host received a request")
    }

    /// `127.0.0.1:<port>` as it appears in the forwarded `host` header.
    fn authority(&self) -> String {
        format!(
            "{}:{}",
            self.origin.host_str().expect("host"),
            self.origin.port().expect("port")
        )
    }
}

/// Answers one request; every request is recorded before routing.
async fn host_response(
    request: Request<Incoming>,
    record: Arc<Mutex<Vec<SeenRequest>>>,
) -> Response<BoxBody<Bytes, Infallible>> {
    let (parts, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map(|body| body.to_bytes())
        .unwrap_or_default();
    let target = parts.uri.path_and_query().map_or_else(
        || parts.uri.path().to_owned(),
        |value| value.as_str().to_owned(),
    );
    record.lock().expect("record").push(SeenRequest {
        method: parts.method.as_str().to_owned(),
        target,
        headers: parts.headers.clone(),
        body: body.clone(),
    });

    match parts.uri.path() {
        "/sse" => {
            let mut response = Response::new(sse_body());
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            response
        }
        "/redirect" => {
            let mut response = text_response(StatusCode::SEE_OTHER, "moved");
            response.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_static("http://elsewhere.invalid/"),
            );
            response.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_static("host-only=1; HttpOnly"),
            );
            response
        }
        "/leaky" => {
            let mut response = text_response(StatusCode::OK, "leaky");
            let headers = response.headers_mut();
            headers.insert(
                header::SET_COOKIE,
                HeaderValue::from_static("host-only=1; HttpOnly"),
            );
            headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("max-age=60"),
            );
            response
        }
        "/upload" => text_response(StatusCode::OK, &body.len().to_string()),
        "/api/sessions" => text_response(StatusCode::OK, "[\"session\"]"),
        path if path.starts_with("/plugins/") => {
            let mut response =
                Response::new(full_body(Bytes::from_static(b"export const plugin = 1\n")));
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/javascript; charset=utf-8"),
            );
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            response
        }
        _ => text_response(StatusCode::NOT_FOUND, "not found"),
    }
}

fn text_response(status: StatusCode, body: &str) -> Response<BoxBody<Bytes, Infallible>> {
    let mut response = Response::new(full_body(Bytes::from(body.to_owned())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn full_body(bytes: Bytes) -> BoxBody<Bytes, Infallible> {
    Full::new(bytes)
        .map_err(|never: Infallible| match never {})
        .boxed()
}

/// Three chunks, 200 ms apart, so a buffering proxy is visible in the timing.
fn sse_body() -> BoxBody<Bytes, Infallible> {
    let chunks = futures_util::stream::unfold(0usize, |index| async move {
        if index >= 3 {
            return None;
        }
        if index > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let frame = Frame::data(Bytes::from(format!("data: chunk-{index}\n\n")));
        Some((Ok::<_, Infallible>(frame), index + 1))
    });
    BodyExt::boxed(StreamBody::new(chunks))
}

// ---------------------------------------------------------------------------
// Client helpers
// ---------------------------------------------------------------------------

/// One response received from the app server.
struct ClientResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl ClientResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Opens a request on `origin`; the response body is left streaming.
async fn send(
    origin: &Url,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Bytes>,
) -> Response<Incoming> {
    let stream = TcpStream::connect(authority_of(origin))
        .await
        .expect("connect");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("{}{path}", origin.as_str().trim_end_matches('/')))
        .version(hyper::Version::HTTP_11);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(full_body(body.unwrap_or_default()))
        .expect("request");
    sender.send_request(request).await.expect("response")
}

/// Reads a whole response.
async fn call(
    origin: &Url,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Bytes>,
) -> ClientResponse {
    let response = send(origin, method, path, headers, body).await;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    ClientResponse {
        status,
        headers,
        body,
    }
}

fn authority_of(origin: &Url) -> SocketAddr {
    let host = origin.host_str().expect("host");
    let port = origin.port().expect("port");
    format!("{host}:{port}").parse().expect("socket address")
}

/// Starts the server against `host` with the prepared dist fixture.
async fn start_server(dist: &Path, host: &MockHost) -> AppServer {
    AppServer::start(AppServerConfig::new(dist, host.origin.clone(), COOKIE))
        .await
        .expect("app server")
}

// ---------------------------------------------------------------------------
// Static documents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_document_carries_the_boot_script() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    for path in ["/", "/index.html"] {
        let response = call(server.origin(), "GET", path, &[], None).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert_eq!(
            response.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        let body = response.text();
        assert!(body.contains(&format!("<head>{BOOT_SCRIPT}")), "{body}");
        assert_eq!(body.matches(BOOT_SCRIPT).count(), 1);
        assert!(body.contains("__DSH_BOOT_READY__ = Promise.withResolvers()"));
    }
    assert!(host.seen().is_empty(), "documents never reach the host");
}

#[tokio::test]
async fn head_requests_send_headers_without_a_body() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(server.origin(), "HEAD", "/", &[], None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.is_empty(), "HEAD must not send a body");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    let length: usize = response
        .header("content-length")
        .expect("content length")
        .parse()
        .expect("numeric length");
    assert!(length > BOOT_SCRIPT.len());
}

#[tokio::test]
async fn assets_use_the_official_mime_table() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    for (path, expected) in [
        ("/assets/app-abc.js", "text/javascript; charset=utf-8"),
        ("/assets/app-abc.css", "text/css; charset=utf-8"),
        ("/assets/data.bin", "application/octet-stream"),
        ("/favicon.svg", "image/svg+xml"),
        ("/favicon-dark.svg", "image/svg+xml"),
        ("/manifest.webmanifest", "application/octet-stream"),
    ] {
        let response = call(server.origin(), "GET", path, &[], None).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert_eq!(response.header("content-type"), Some(expected), "{path}");
    }
    assert!(host.seen().is_empty());
}

#[tokio::test]
async fn missing_documents_are_404_and_traversal_is_403() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let missing = call(server.origin(), "GET", "/assets/missing.js", &[], None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    for path in [
        "/assets/../../../../etc/passwd",
        "/assets/%2e%2e/%2e%2e/etc/passwd",
        "/assets/..%2f..%2fetc%2fpasswd",
    ] {
        let response = call(server.origin(), "GET", path, &[], None).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
        assert!(!response.text().contains("root:"));
    }

    let malformed = call(server.origin(), "GET", "/assets/%zz.js", &[], None).await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    assert!(
        host.seen().is_empty(),
        "static failures never reach the host"
    );
}

#[tokio::test]
async fn non_get_methods_on_documents_are_405() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    for method in ["POST", "PUT", "DELETE"] {
        let response = call(server.origin(), method, "/assets/app-abc.js", &[], None).await;
        assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
    assert!(host.seen().is_empty());
}

#[tokio::test]
async fn unknown_paths_are_forwarded_to_the_host() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(server.origin(), "GET", "/api/sessions", &[], None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.text(), "[\"session\"]");
    let seen = host.last();
    assert_eq!(seen.method, "GET");
    assert_eq!(seen.target, "/api/sessions");
}

#[tokio::test]
async fn shell_pages_are_served_through_the_injected_callback() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let mut config = AppServerConfig::new(dist.path(), host.origin.clone(), COOKIE);
    config.shell_assets = Some(Arc::new(|path: &str| match path {
        "loading.html" => Some(ShellAsset {
            status: 200,
            content_type: "text/html; charset=utf-8".to_owned(),
            body: b"<html><body>loading</body></html>".to_vec(),
        }),
        "gone.html" => Some(ShellAsset {
            status: 410,
            content_type: "text/plain".to_owned(),
            body: b"gone".to_vec(),
        }),
        _ => None,
    }));
    let server = AppServer::start(config).await.expect("app server");

    let page = call(server.origin(), "GET", "/_shell/loading.html", &[], None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("loading"));

    let missing = call(server.origin(), "GET", "/_shell/nope.html", &[], None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let failing = call(server.origin(), "GET", "/_shell/gone.html", &[], None).await;
    assert_eq!(failing.status, StatusCode::GONE);
    assert!(host.seen().is_empty());
}

// ---------------------------------------------------------------------------
// HTTP proxy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn host_bound_headers_are_replaced_and_hop_headers_are_dropped() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(
        server.origin(),
        "GET",
        "/api/sessions?limit=5",
        &[
            ("host", "evil.example"),
            ("origin", RENDER_ORIGIN),
            ("cookie", "page-owned=1"),
            ("sec-fetch-site", "cross-site"),
            ("accept-encoding", "gzip, br"),
            ("x-request-id", "abc123"),
        ],
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);

    let seen = host.last();
    assert_eq!(seen.target, "/api/sessions?limit=5");
    assert_eq!(
        seen.header("cookie"),
        Some(COOKIE),
        "the shell cookie replaces the page's"
    );
    assert_eq!(seen.header("x-request-id"), Some("abc123"));
    assert_eq!(seen.header("host"), Some(host.authority().as_str()));
    assert_eq!(
        seen.header("origin"),
        None,
        "the renderer origin is not relayed"
    );
    assert_eq!(seen.header("sec-fetch-site"), None);
    assert_eq!(
        seen.header("accept-encoding"),
        None,
        "responses are not decoded, so identity is requested"
    );
}

#[tokio::test]
async fn response_cookie_and_framing_headers_are_withheld() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(server.origin(), "GET", "/leaky", &[], None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.text(), "leaky");
    assert_eq!(
        response.header("set-cookie"),
        None,
        "the host cookie stays in the shell"
    );
    assert_eq!(response.header("content-encoding"), None);
    assert_eq!(response.header("content-length"), None);
    assert_eq!(response.header("cache-control"), Some("max-age=60"));
    assert_eq!(
        response.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
}

#[tokio::test]
async fn plugin_bundles_are_no_store() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(server.origin(), "GET", "/plugins/demo/index.js", &[], None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(
        response.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(host.last().target, "/plugins/demo/index.js");
}

#[tokio::test]
async fn post_bodies_are_streamed_to_the_host() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let payload = Bytes::from(vec![b'a'; 200 * 1024]);
    let response = call(
        server.origin(),
        "POST",
        "/upload",
        &[("content-type", "application/octet-stream")],
        Some(payload.clone()),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.text(), payload.len().to_string());
    let seen = host.last();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.body.len(), payload.len());
    assert_eq!(
        seen.header("content-type"),
        Some("application/octet-stream")
    );
}

#[tokio::test]
async fn head_requests_through_the_proxy_have_no_body() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(server.origin(), "HEAD", "/api/sessions", &[], None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.is_empty());
    assert_eq!(host.last().method, "HEAD");
}

#[tokio::test]
async fn redirects_are_relayed_without_following_them() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(server.origin(), "GET", "/redirect", &[], None).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        response.header("location"),
        Some("http://elsewhere.invalid/")
    );
    assert_eq!(response.header("set-cookie"), None);
    assert_eq!(host.seen().len(), 1, "the redirect target is not fetched");
}

#[tokio::test]
async fn sse_chunks_arrive_before_the_next_one_is_written() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let started = Instant::now();
    let response = send(server.origin(), "GET", "/sse", &[], None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );

    let mut body = response.into_body();
    let first = body
        .frame()
        .await
        .expect("a frame")
        .expect("a valid frame")
        .into_data()
        .expect("data frame");
    let first_delay = started.elapsed();
    assert_eq!(&first[..], b"data: chunk-0\n\n");
    assert!(
        first_delay < Duration::from_millis(200),
        "first chunk was buffered for {first_delay:?}"
    );

    let mut received = 1;
    let mut rest = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("frame");
        if let Ok(data) = frame.into_data() {
            received += 1;
            rest.extend_from_slice(&data);
        }
    }
    assert_eq!(received, 3, "every chunk arrives");
    let rest = String::from_utf8_lossy(&rest).into_owned();
    assert!(
        rest.contains("chunk-1") && rest.contains("chunk-2"),
        "{rest}"
    );
}

#[tokio::test]
async fn an_unreachable_host_is_502() {
    let dist = dist_fixture();
    // A listener that is dropped immediately leaves a closed loopback port.
    let closed = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = closed.local_addr().expect("addr");
    drop(closed);
    let server = AppServer::start(AppServerConfig::new(
        dist.path(),
        Url::parse(&format!("http://{addr}")).expect("origin"),
        COOKIE,
    ))
    .await
    .expect("app server");

    let response = call(server.origin(), "GET", "/api/sessions", &[], None).await;
    assert_eq!(response.status, StatusCode::BAD_GATEWAY);
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_origins_are_rejected_before_any_upstream_traffic() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(
        server.origin(),
        "GET",
        "/api/sessions",
        &[("origin", "https://evil.example")],
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert!(host.seen().is_empty());
}

#[tokio::test]
async fn the_page_origin_gets_a_cors_answer() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(
        server.origin(),
        "GET",
        "/api/sessions",
        &[("origin", RENDER_ORIGIN)],
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some(RENDER_ORIGIN)
    );
    assert!(
        response
            .header("vary")
            .is_some_and(|vary| vary.to_ascii_lowercase().contains("origin")),
        "the answer varies by origin"
    );
    // The credentialed cookie is still injected by the shell, never by the page.
    assert_eq!(host.last().header("cookie"), Some(COOKIE));
}

#[tokio::test]
async fn every_window_origin_is_accepted_and_echoed() {
    // Tauri reports custom scheme pages differently per platform; the renderer
    // may legitimately send any of these, and each is echoed verbatim.
    for origin in [
        RENDER_ORIGIN,
        "dsh-app://localhost",
        "http://dsh-app.localhost",
    ] {
        let dist = dist_fixture();
        let host = MockHost::start().await;
        let server = start_server(dist.path(), &host).await;
        let response = call(
            server.origin(),
            "GET",
            "/api/sessions",
            &[("origin", origin)],
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{origin}");
        assert_eq!(
            response.header("access-control-allow-origin"),
            Some(origin),
            "{origin}"
        );
    }
}

#[tokio::test]
async fn preflights_are_answered_for_every_window_origin() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    for origin in ["dsh-app://localhost", "http://dsh-app.localhost"] {
        let response = call(
            server.origin(),
            "OPTIONS",
            "/api/sessions",
            &[
                ("origin", origin),
                ("access-control-request-method", "DELETE"),
                ("access-control-request-headers", "content-type"),
            ],
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::NO_CONTENT, "{origin}");
        assert_eq!(
            response.header("access-control-allow-origin"),
            Some(origin),
            "{origin}"
        );
        assert_eq!(
            response.header("access-control-allow-methods"),
            Some("DELETE")
        );
    }
    assert!(host.seen().is_empty());
}

#[tokio::test]
async fn origin_variants_and_default_ports_are_matched() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    // Scheme, host and the http default port are normalized away; a custom
    // scheme has no default port, so `dsh-app://localhost:80` stays distinct.
    for origin in ["HTTP://DSH-APP.LOCALHOST:80", "http://dsh-app.localhost/"] {
        let response = call(
            server.origin(),
            "GET",
            "/api/sessions",
            &[("origin", origin)],
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{origin}");
    }

    // A lookalike host with an extra port is a different origin.
    let lookalike = call(
        server.origin(),
        "GET",
        "/api/sessions",
        &[("origin", "http://dsh-app.localhost:9999")],
        None,
    )
    .await;
    assert_eq!(lookalike.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn same_origin_requests_need_no_cors_headers() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;
    let own_origin = server.origin().as_str().trim_end_matches('/').to_owned();

    let response = call(
        server.origin(),
        "GET",
        "/api/sessions",
        &[("origin", own_origin.as_str())],
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header("access-control-allow-origin"), None);
}

#[tokio::test]
async fn preflights_are_answered_locally() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;

    let response = call(
        server.origin(),
        "OPTIONS",
        "/api/sessions",
        &[
            ("origin", RENDER_ORIGIN),
            ("access-control-request-method", "POST"),
            (
                "access-control-request-headers",
                "content-type, x-request-id",
            ),
        ],
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some(RENDER_ORIGIN)
    );
    assert_eq!(
        response.header("access-control-allow-methods"),
        Some("POST")
    );
    assert_eq!(
        response.header("access-control-allow-headers"),
        Some("content-type, x-request-id")
    );
    assert_eq!(response.header("access-control-max-age"), Some("600"));
    assert!(response.body.is_empty());
    assert!(
        host.seen().is_empty(),
        "a preflight must not reach the host"
    );

    // A plain OPTIONS is not a preflight and still belongs to the host.
    let forwarded = call(
        server.origin(),
        "OPTIONS",
        "/api/sessions",
        &[("origin", RENDER_ORIGIN)],
        None,
    )
    .await;
    assert_eq!(forwarded.status, StatusCode::OK);
    assert_eq!(host.last().method, "OPTIONS");
}

// ---------------------------------------------------------------------------
// WebSocket proxy
// ---------------------------------------------------------------------------

/// A mock Host that upgrades WebSockets and echoes text frames.
struct MockSocketHost {
    origin: Url,
    handshakes: Arc<Mutex<Vec<(String, HeaderMap)>>>,
}

impl MockSocketHost {
    async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handshakes = Arc::new(Mutex::new(Vec::new()));
        let record = handshakes.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    return;
                };
                let record = record.clone();
                tokio::spawn(async move {
                    // tungstenite's error variant is large; this stub never errors.
                    #[allow(clippy::result_large_err)]
                    let callback = move |request: &Request<()>, mut response: Response<()>| {
                        record
                            .lock()
                            .expect("handshakes")
                            .push((request.uri().to_string(), request.headers().clone()));
                        if let Some(protocol) = request
                            .headers()
                            .get(header::SEC_WEBSOCKET_PROTOCOL)
                            .cloned()
                        {
                            response
                                .headers_mut()
                                .insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
                        }
                        Ok(response)
                    };
                    let Ok(upgrade) = accept_hdr_async(stream, callback).await else {
                        return;
                    };
                    let (mut sink, mut source) = upgrade.split();
                    while let Some(message) = source.next().await {
                        let Ok(message) = message else { break };
                        match message {
                            Message::Text(text) => {
                                if sink
                                    .send(Message::text(format!("echo: {text}")))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        Self {
            origin: Url::parse(&format!("http://{addr}")).expect("origin"),
            handshakes,
        }
    }

    fn handshakes(&self) -> Vec<(String, HeaderMap)> {
        self.handshakes.lock().expect("handshakes").clone()
    }
}

#[tokio::test]
async fn websockets_are_proxied_with_the_shell_credentials() {
    let dist = dist_fixture();
    let host = MockSocketHost::start().await;
    let server = AppServer::start(AppServerConfig::new(
        dist.path(),
        host.origin.clone(),
        COOKIE,
    ))
    .await
    .expect("app server");

    let url = format!(
        "ws://{}/events/stream?since=4",
        authority_of(server.origin())
    );
    let mut request = url.into_client_request().expect("client request");
    request
        .headers_mut()
        .insert(header::ORIGIN, HeaderValue::from_static(RENDER_ORIGIN));
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("dsh-events"),
    );
    let (socket, response) = connect_async(request).await.expect("client handshake");
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok()),
        Some("dsh-events"),
        "the negotiated subprotocol is preserved"
    );

    let handshakes = host.handshakes();
    assert_eq!(handshakes.len(), 1);
    let (target, headers) = &handshakes[0];
    assert_eq!(target, "/events/stream?since=4");
    assert_eq!(
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
        Some(COOKIE)
    );
    let expected_origin = host.origin.origin().ascii_serialization();
    assert_eq!(
        headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some(expected_origin.as_str())
    );
    assert_eq!(
        headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok()),
        Some("same-origin")
    );

    // Text frames travel in both directions.
    let (mut sink, mut source) = socket.split();
    sink.send(Message::text("hello")).await.expect("send");
    let echoed = source
        .next()
        .await
        .expect("a frame")
        .expect("a valid frame")
        .into_text()
        .expect("text frame");
    assert_eq!(echoed.as_str(), "echo: hello");
    sink.send(Message::Close(None)).await.expect("close");
}

#[tokio::test]
async fn websockets_from_foreign_origins_are_rejected() {
    let dist = dist_fixture();
    let host = MockSocketHost::start().await;
    let server = AppServer::start(AppServerConfig::new(
        dist.path(),
        host.origin.clone(),
        COOKIE,
    ))
    .await
    .expect("app server");

    let url = format!("ws://{}/events", authority_of(server.origin()));
    let mut request = url.into_client_request().expect("client request");
    request.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://evil.example"),
    );
    assert!(
        connect_async(request).await.is_err(),
        "the upgrade must be rejected"
    );
    assert!(
        host.handshakes().is_empty(),
        "the host never sees a rejected upgrade"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_releases_the_port() {
    let dist = dist_fixture();
    let host = MockHost::start().await;
    let server = start_server(dist.path(), &host).await;
    let addr = authority_of(server.origin());

    let response = call(server.origin(), "GET", "/api/sessions", &[], None).await;
    assert_eq!(response.status, StatusCode::OK);

    server.shutdown().await;
    let refused = TcpStream::connect(addr).await;
    assert!(refused.is_err(), "the port is closed after shutdown");
    let rebound = TcpListener::bind(addr).await;
    assert!(rebound.is_ok(), "the port can be reused");
}

// ---------------------------------------------------------------------------
// Host authentication
// ---------------------------------------------------------------------------

/// One-shot mock Host answering with a fixed raw response.
async fn spawn_auth_host(response: &'static [u8]) -> Url {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let Ok((mut stream, _peer)) = listener.accept().await else {
            return;
        };
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer).await;
        let _ = stream.write_all(response).await;
        let _ = stream.flush().await;
    });
    Url::parse(&format!("http://{addr}/?token=one-shot-token")).expect("url")
}

#[tokio::test]
async fn authenticate_host_takes_the_cookie_from_the_303() {
    let url = spawn_auth_host(
        b"HTTP/1.1 303 See Other\r\nlocation: /\r\nset-cookie: dsh=abc123; Path=/; HttpOnly; SameSite=Strict\r\ncontent-length: 0\r\n\r\n",
    )
    .await;
    let cookie = authenticate_host(&url).await.expect("cookie");
    assert_eq!(cookie, "dsh=abc123");
}

#[tokio::test]
async fn authenticate_host_rejects_a_non_redirect() {
    let url =
        spawn_auth_host(b"HTTP/1.1 200 OK\r\nset-cookie: dsh=abc\r\ncontent-length: 0\r\n\r\n")
            .await;
    match authenticate_host(&url).await {
        Err(AuthError::NotRedirect { status }) => assert_eq!(status, 200),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[tokio::test]
async fn authenticate_host_reports_a_redirect_without_a_cookie() {
    let url =
        spawn_auth_host(b"HTTP/1.1 303 See Other\r\nlocation: /\r\ncontent-length: 0\r\n\r\n")
            .await;
    assert!(matches!(
        authenticate_host(&url).await,
        Err(AuthError::MissingCookie)
    ));
}

#[tokio::test]
async fn authenticate_host_reports_connection_failures() {
    let closed = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = closed.local_addr().expect("addr");
    drop(closed);
    let url = Url::parse(&format!("http://{addr}/?token=one-shot-token")).expect("url");
    assert!(matches!(
        authenticate_host(&url).await,
        Err(AuthError::Connect { .. })
    ));
}

#[tokio::test]
async fn authenticate_host_refuses_non_loopback_urls() {
    let url = Url::parse("https://host.example/?token=one-shot-token").expect("url");
    assert!(matches!(
        authenticate_host(&url).await,
        Err(AuthError::Url { .. })
    ));
}
