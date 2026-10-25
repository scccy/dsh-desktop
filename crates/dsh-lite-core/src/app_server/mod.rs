//! Loopback HTTP origin for the application window.
//!
//! The official Electron shell serves the packaged frontend from
//! `dsh-app://app` and proxies everything else to the Desktop Host from the same
//! scheme handler. A system WebView cannot replace that handler with a
//! streaming one, so this module provides an equivalent origin on
//! `http://127.0.0.1:<port>/`:
//!
//! * `/`, `/index.html`, `/assets/*`, the favicons and the web manifest are read
//!   from the packaged `dsh-web-frontend/dist` directory, with the boot script
//!   injected before the frontend module runs;
//! * `/_shell/*` answers shell-owned pages through [`ShellAssets`], which
//!   `src-tauri` backs with the Tauri asset resolver;
//! * every other path is forwarded to the authenticated Host, streaming both
//!   directions, with the Host cookie injected and the response's connection and
//!   cookie headers withheld;
//! * WebSocket upgrades are proxied on the same port, so the renderer never sees
//!   the Host cookie.
//!
//! The window keeps `dsh-app://app` as its page origin, so requests from the
//! renderer reach this loopback origin cross-origin: every answer carries
//! `access-control-allow-origin` echoing the renderer's origin, and `OPTIONS`
//! preflights are answered locally instead of travelling to the Host. Only
//! [`RENDER_ORIGIN`] and this server's own origin are accepted; anything else is
//! a 403, matching the official `forwardWebRequest` origin check.
//!
//! Serving the packaged documents here is also the fallback if the scheme
//! handler ever has to be replaced: pointing the window at [`AppServer::origin`]
//! makes the page same-origin with its own API traffic.
//!
//! Nothing here depends on Tauri: the shell supplies paths, the cookie and the
//! asset callback, and every rule above is covered by `cargo test`.

mod auth;
mod document;
mod proxy;
mod websocket;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{self, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use url::Url;

pub use auth::{AuthError, authenticate_host};
pub use document::BOOT_SCRIPT;

/// Page origin of the application window on macOS and Linux.
///
/// The WebView reports a custom scheme page as `<scheme>://<resource>` there.
pub const RENDER_ORIGIN: &str = "dsh-app://app";

/// Every origin the shell's own pages can report, across Tauri platforms.
///
/// Tauri documents two shapes for custom scheme pages: `<scheme>://localhost/...`
/// on macOS and Linux, and `http://<scheme>.localhost/...` on Windows and
/// Android. The registration is `dsh-app`, so a Windows origin is
/// `http://dsh-app.localhost`.
pub const WINDOW_ORIGINS: [&str; 3] = [
    RENDER_ORIGIN,
    "dsh-app://localhost",
    "http://dsh-app.localhost",
];

/// Boxed body error shared by every response body this server builds.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Response body type of the application server.
pub(crate) type AppBody = BoxBody<Bytes, BoxError>;

/// Methods a preflight accepts when the renderer does not name one.
const ALLOWED_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, OPTIONS";

/// Request headers a preflight accepts when the renderer does not name any.
const ALLOWED_REQUEST_HEADERS: &str = "content-type, authorization";

/// How long a renderer may cache a preflight answer.
const PREFLIGHT_MAX_AGE: &str = "600";

/// A shell-owned page served from the application origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellAsset {
    /// HTTP status; only 2xx sends a document.
    pub status: u16,
    /// Media type written as `content-type`.
    pub content_type: String,
    /// Complete response body.
    pub body: Vec<u8>,
}

/// Resolver for the shell's own pages, injected by `src-tauri`.
///
/// It receives the part of the path after `/_shell/` (`loading.html`,
/// `error.html`, …) and returns `None` for anything it does not own.
pub trait ShellAssets: Send + Sync {
    /// Returns the page for `path`, when the shell owns it.
    fn resolve(&self, path: &str) -> Option<ShellAsset>;
}

impl<F> ShellAssets for F
where
    F: Fn(&str) -> Option<ShellAsset> + Send + Sync,
{
    fn resolve(&self, path: &str) -> Option<ShellAsset> {
        self(path)
    }
}

/// Route diagnostics for the server.
///
/// The shell passes its [`crate::logging::Logger`]; nothing here records a
/// cookie, a token or a query string.
pub trait RouteLog: Send + Sync {
    /// Records one route decision.
    fn debug(&self, message: &str);
}

impl RouteLog for crate::logging::Logger {
    fn debug(&self, message: &str) {
        crate::logging::Logger::debug(self, message);
    }
}

/// Everything the server needs to serve the window.
pub struct AppServerConfig {
    /// Packaged `dsh-web-frontend/dist` directory.
    pub dist_dir: PathBuf,
    /// Authenticated Desktop Host origin, for example `http://127.0.0.1:4100`.
    pub host_origin: Url,
    /// Cookie returned by [`authenticate_host`].
    pub cookie: String,
    /// Script injected into the index document; see [`BOOT_SCRIPT`].
    pub boot_script: String,
    /// Shell-owned pages served under `/_shell/`.
    pub shell_assets: Option<Arc<dyn ShellAssets>>,
    /// Additional page origin accepted besides [`RENDER_ORIGIN`] and this
    /// server's own origin.
    ///
    /// Only needed when the window is pointed at some other origin; the shipped
    /// layout (`dsh-app://app`) works without it.
    pub render_origin: Option<String>,
    /// Route diagnostics.
    pub logger: Option<Arc<dyn RouteLog>>,
}

impl AppServerConfig {
    /// Config with the official boot script and no injected pages.
    pub fn new(dist_dir: impl Into<PathBuf>, host_origin: Url, cookie: impl Into<String>) -> Self {
        Self {
            dist_dir: dist_dir.into(),
            host_origin,
            cookie: cookie.into(),
            boot_script: BOOT_SCRIPT.to_owned(),
            shell_assets: None,
            render_origin: None,
            logger: None,
        }
    }
}

/// A running application server.
pub struct AppServer {
    origin: Url,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl AppServer {
    /// Binds an OS-assigned loopback port and starts serving.
    pub async fn start(config: AppServerConfig) -> std::io::Result<Self> {
        if !matches!(config.host_origin.scheme(), "http" | "https") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the host origin must be http or https",
            ));
        }
        if authority(&config.host_origin).is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the host origin must have a host",
            ));
        }
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let port = listener.local_addr()?.port();
        let own_origin = format!("http://127.0.0.1:{port}");
        let origin = Url::parse(&format!("{own_origin}/"))
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let allowed_origins = allowed_origins(&own_origin, config.render_origin.as_deref());

        let (shutdown, shutdown_rx) = watch::channel(false);
        let state = AppState {
            dist_dir: Arc::new(config.dist_dir),
            boot_script: Arc::from(config.boot_script),
            cookie: Arc::from(config.cookie),
            host_origin: Arc::new(config.host_origin),
            own_origin: Arc::from(
                normalized_origin(&own_origin).unwrap_or_else(|| own_origin.clone()),
            ),
            allowed_origins: Arc::new(allowed_origins),
            shell_assets: config.shell_assets,
            logger: config.logger,
            shutdown: shutdown_rx.clone(),
        };
        state.debug(format!(
            "app_server: serving {} on {}",
            state.dist_dir.display(),
            state.own_origin
        ));
        let task = tokio::spawn(run(listener, state, shutdown_rx));
        Ok(Self {
            origin,
            shutdown,
            task,
        })
    }

    /// `http://127.0.0.1:<port>/` of the running server.
    pub fn origin(&self) -> &Url {
        &self.origin
    }

    /// Stops accepting, closes every connection and releases the port.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// Shared, immutable server state cloned into every connection.
#[derive(Clone)]
pub(crate) struct AppState {
    dist_dir: Arc<PathBuf>,
    boot_script: Arc<str>,
    cookie: Arc<str>,
    host_origin: Arc<Url>,
    own_origin: Arc<str>,
    allowed_origins: Arc<Vec<String>>,
    shell_assets: Option<Arc<dyn ShellAssets>>,
    logger: Option<Arc<dyn RouteLog>>,
    shutdown: watch::Receiver<bool>,
}

impl AppState {
    /// Records one route decision when a logger is installed.
    pub(crate) fn debug(&self, message: impl AsRef<str>) {
        if let Some(logger) = &self.logger {
            logger.debug(message.as_ref());
        }
    }

    /// Authenticated Host origin.
    pub(crate) fn host_origin(&self) -> &Url {
        &self.host_origin
    }

    /// Host cookie value, including its name.
    pub(crate) fn cookie(&self) -> &str {
        &self.cookie
    }

    /// `scheme://host[:port]` of the Host origin, as the `origin` header value.
    pub(crate) fn host_origin_string(&self) -> String {
        self.host_origin.origin().ascii_serialization()
    }

    /// `access-control-allow-origin` value for a renderer request.
    ///
    /// Same-origin requests need no CORS headers; anything else that passed
    /// [`AppState::is_allowed_origin`] is echoed verbatim, because a browser only
    /// accepts an `access-control-allow-origin` that matches the `origin` header
    /// it sent character for character.
    pub(crate) fn cors_echo(&self, origin: Option<&str>) -> Option<String> {
        let origin = origin?;
        if normalized_origin(origin).as_deref() == Some(self.own_origin.as_ref()) {
            return None;
        }
        self.is_allowed_origin(origin).then(|| origin.to_owned())
    }

    /// Whether a request origin may reach this server.
    pub(crate) fn origin_allowed(&self, origin: Option<&str>) -> bool {
        match origin {
            None => true,
            Some(value) => self.is_allowed_origin(value),
        }
    }

    /// Whether `origin` is one of the shell's page origins or this server.
    ///
    /// Both sides are parsed and serialized, so `DSH-APP://APP`, a trailing
    /// slash and an explicit default port all compare equal.
    pub(crate) fn is_allowed_origin(&self, origin: &str) -> bool {
        let Some(normalized) = normalized_origin(origin) else {
            return false;
        };
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == &normalized)
    }

    /// Receiver that fires when the server shuts down.
    pub(crate) fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.clone()
    }
}

/// Accepts connections until a shutdown is requested.
async fn run(listener: TcpListener, state: AppState, mut shutdown: watch::Receiver<bool>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    connections.spawn(serve_connection(stream, state.clone()));
                }
                Err(error) => {
                    // Loopback accepts only fail transiently (for example when
                    // the process is out of descriptors), so back off instead of
                    // spinning.
                    state.debug(format!("app_server: accept failed: {error}"));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            _ = shutdown.changed() => break,
        }
    }
    drop(listener);
    // Aborting the connections also drops every upgraded socket, so the port is
    // free by the time `shutdown()` resolves.
    connections.shutdown().await;
}

/// Serves HTTP/1.1 (including upgrades) on one accepted connection.
async fn serve_connection(stream: TcpStream, state: AppState) {
    let mut shutdown = state.shutdown_receiver();
    let logging = state.clone();
    let service = service_fn(move |request| {
        let state = state.clone();
        async move { Ok::<_, Infallible>(handle(request, state).await) }
    });
    let connection = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    tokio::pin!(connection);
    tokio::select! {
        result = connection.as_mut() => {
            if let Err(error) = result {
                logging.debug(format!("app_server: connection ended: {error}"));
            }
        }
        _ = shutdown.changed() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}

/// Enforces the origin gate, answers preflights, and routes the request.
async fn handle(request: Request<Incoming>, state: AppState) -> Response<AppBody> {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if !state.origin_allowed(origin.as_deref()) {
        state.debug(format!(
            "app_server: {} {} -> 403 (request origin)",
            request.method(),
            request.uri().path()
        ));
        return status_response(StatusCode::FORBIDDEN);
    }
    // A cross-origin preflight never reaches the Host: the answer depends only on
    // this server's own policy, and the renderer would otherwise pay a round trip
    // (or a 405 from a route that does not implement OPTIONS) for every new
    // method or header it uses.
    if is_preflight(&request) {
        state.debug(format!(
            "app_server: OPTIONS {} -> 204 (preflight)",
            request.uri().path()
        ));
        return preflight_response(&request, state.cors_echo(origin.as_deref()));
    }
    let cors = state.cors_echo(origin.as_deref());
    let mut response = route_request(request, state).await;
    // An upgrade answer carries no fetchable body, so it stays free of CORS.
    if response.status() != StatusCode::SWITCHING_PROTOCOLS
        && let Some(cors) = cors
    {
        apply_cors(&mut response, &cors);
    }
    response
}

/// Whether the request is a CORS preflight that must be answered locally.
fn is_preflight(request: &Request<Incoming>) -> bool {
    request.method() == Method::OPTIONS
        && request.headers().contains_key(header::ORIGIN)
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Answer for a renderer preflight: no body, no Host traffic.
fn preflight_response(request: &Request<Incoming>, cors: Option<String>) -> Response<AppBody> {
    let mut response = status_response(StatusCode::NO_CONTENT);
    let headers = response.headers_mut();
    headers.append(header::VARY, HeaderValue::from_static("Origin"));
    headers.append(
        header::VARY,
        HeaderValue::from_static("Access-Control-Request-Method, Access-Control-Request-Headers"),
    );
    let Some(cors) = cors else {
        return response;
    };
    if let Ok(origin) = HeaderValue::from_str(&cors) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    let methods = request
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static(ALLOWED_METHODS));
    headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, methods);
    let allowed_headers = request
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static(ALLOWED_REQUEST_HEADERS));
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, allowed_headers);
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(PREFLIGHT_MAX_AGE),
    );
    response
}

/// Adds the CORS answer to a response for a cross-origin renderer request.
fn apply_cors(response: &mut Response<AppBody>, origin: &str) {
    let headers = response.headers_mut();
    if let Ok(origin) = HeaderValue::from_str(origin) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    headers.append(header::VARY, HeaderValue::from_static("Origin"));
}

/// Routes one request: WebSocket upgrade, packaged document, or Host proxy.
async fn route_request(request: Request<Incoming>, state: AppState) -> Response<AppBody> {
    if websocket::wants_websocket(request.headers()) {
        return websocket::proxy(request, state).await;
    }
    let pathname = request.uri().path().to_owned();
    let method = request.method().clone();
    if let Some(route) = document::route(&pathname) {
        return static_response(route, &pathname, &method, &state).await;
    }
    proxy::forward(request, &state).await
}

/// Answers a packaged document or a shell-owned page.
async fn static_response(
    route: document::StaticRoute,
    pathname: &str,
    method: &Method,
    state: &AppState,
) -> Response<AppBody> {
    if !matches!(*method, Method::GET | Method::HEAD) {
        state.debug(format!("app_server: {method} {pathname} -> 405 (document)"));
        return status_response(StatusCode::METHOD_NOT_ALLOWED);
    }
    let head_only = *method == Method::HEAD;
    let read = match route {
        document::StaticRoute::Shell => {
            document::read_shell(state.shell_assets.as_deref(), pathname)
        }
        document::StaticRoute::Index | document::StaticRoute::Asset => {
            document::read_dist(&state.dist_dir, pathname, &state.boot_script).await
        }
    };
    match read {
        Ok(file) => {
            state.debug(format!(
                "app_server: {method} {pathname} -> 200 ({} B)",
                file.body.len()
            ));
            document_response(file, head_only)
        }
        Err(status) => {
            state.debug(format!("app_server: {method} {pathname} -> {status}"));
            status_response(
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            )
        }
    }
}

/// Response for a document; `HEAD` keeps the headers but sends no body.
fn document_response(file: document::StaticFile, head_only: bool) -> Response<AppBody> {
    let length = file.body.len();
    let body = if head_only {
        empty_body()
    } else {
        bytes_body(Bytes::from(file.body))
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&file.content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(length) = HeaderValue::from_str(&length.to_string()) {
        headers.insert(CONTENT_LENGTH, length);
    }
    response
}

/// Response with a status and no body.
pub(crate) fn status_response(status: StatusCode) -> Response<AppBody> {
    let mut response = Response::new(empty_body());
    *response.status_mut() = status;
    response
}

/// Empty response body.
pub(crate) fn empty_body() -> AppBody {
    Full::new(Bytes::new())
        .map_err(|never: Infallible| -> BoxError { match never {} })
        .boxed()
}

/// Response body from a complete byte buffer.
fn bytes_body(bytes: Bytes) -> AppBody {
    Full::new(bytes)
        .map_err(|never: Infallible| -> BoxError { match never {} })
        .boxed()
}

/// Streamed response body from an upstream response.
///
/// The upstream `content-length` and `transfer-encoding` describe the Host
/// connection and are withheld, and hyper would otherwise synthesise a new
/// `content-length` from the body's exact size. [`Unframed`] hides that size, so
/// every relayed answer reaches the renderer as a chunked stream — the same
/// shape the page sees through the official fetch-based proxy.
pub(crate) fn incoming_body(incoming: Incoming) -> AppBody {
    Unframed::new(incoming)
        .map_err(|error| -> BoxError { Box::new(error) })
        .boxed()
}

/// Body wrapper that hides the inner body's length from the server encoder.
struct Unframed<B> {
    inner: B,
}

impl<B> Unframed<B> {
    fn new(inner: B) -> Self {
        Self { inner }
    }
}

impl<B> Body for Unframed<B>
where
    B: Body + Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(context)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

/// Origins the renderer may use, in canonical form.
///
/// This server's own origin comes first, so [`AppState::cors_echo`] can compare
/// against it directly.
fn allowed_origins(own_origin: &str, render_origin: Option<&str>) -> Vec<String> {
    let mut allowed: Vec<String> = Vec::new();
    for origin in std::iter::once(own_origin)
        .chain(WINDOW_ORIGINS)
        .chain(render_origin)
    {
        if let Some(normalized) = normalized_origin(origin)
            && !allowed.contains(&normalized)
        {
            allowed.push(normalized);
        }
    }
    allowed
}

/// Canonical `scheme://host[:port]` form of an origin string.
///
/// Returns `None` for values a browser would treat as opaque (`null`, a path, a
/// relative reference), so those never match the allow-list.
pub(crate) fn normalized_origin(origin: &str) -> Option<String> {
    let url = Url::parse(origin.trim()).ok()?;
    // A custom scheme such as `dsh-app` is opaque to the URL crate, so the
    // `scheme://host[:port]` form is rebuilt here; hosts are case-insensitive.
    let host = match url.host()? {
        url::Host::Ipv6(address) => format!("[{address}]"),
        other => other.to_string().to_ascii_lowercase(),
    };
    let mut normalized = format!("{}://{host}", url.scheme());
    if let Some(port) = url.port() {
        normalized.push(':');
        normalized.push_str(&port.to_string());
    }
    Some(normalized)
}

/// `host[:port]` of a URL, with IPv6 literals bracketed.
pub(crate) fn authority(url: &Url) -> Option<String> {
    // `Url::host_str` already brackets IPv6 literals, so build the authority from
    // the parsed host instead of string-sniffing.
    let host = match url.host()? {
        url::Host::Ipv6(address) => format!("[{address}]"),
        _ => url.host_str()?.to_owned(),
    };
    match url.port() {
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LogLevel;
    use tempfile::TempDir;

    #[test]
    fn authority_includes_the_port_and_brackets_ipv6() {
        let url = Url::parse("http://127.0.0.1:4100").expect("url");
        assert_eq!(authority(&url).as_deref(), Some("127.0.0.1:4100"));
        let url = Url::parse("http://127.0.0.1").expect("url");
        assert_eq!(authority(&url).as_deref(), Some("127.0.0.1"));
        let url = Url::parse("http://[::1]:4100").expect("url");
        assert_eq!(authority(&url).as_deref(), Some("[::1]:4100"));
    }

    #[test]
    fn origin_gate_accepts_the_page_origin_and_the_server_origin() {
        let state = test_state(None);
        assert!(state.origin_allowed(None));
        assert!(state.origin_allowed(Some(RENDER_ORIGIN)));
        assert!(state.origin_allowed(Some("DSH-APP://APP")));
        assert!(state.origin_allowed(Some("http://127.0.0.1:8080")));
        assert!(!state.origin_allowed(Some("https://example.com")));
        assert!(!state.origin_allowed(Some("null")));
    }

    #[test]
    fn every_tauri_custom_scheme_origin_is_accepted() {
        let state = test_state(None);
        // macOS/Linux report `<scheme>://<resource>`; Windows and Android report
        // `http://<scheme>.localhost`.
        for origin in [
            "dsh-app://app",
            "dsh-app://localhost",
            "http://dsh-app.localhost",
        ] {
            assert!(state.is_allowed_origin(origin), "{origin}");
            assert_eq!(state.cors_echo(Some(origin)).as_deref(), Some(origin));
        }
    }

    #[test]
    fn origins_are_normalized_before_comparison() {
        let state = test_state(None);
        assert!(state.is_allowed_origin("HTTP://DSH-APP.LOCALHOST"));
        assert!(state.is_allowed_origin("http://dsh-app.localhost:80"));
        assert!(state.is_allowed_origin("http://dsh-app.localhost/"));
        assert!(state.is_allowed_origin("  dsh-app://app  "));
        assert!(!state.is_allowed_origin("http://dsh-app.localhost.evil.example"));
        assert!(!state.is_allowed_origin("http://dsh-app.localhost:8080"));
        assert!(!state.is_allowed_origin("not an origin"));
        assert!(!state.is_allowed_origin("null"));
    }

    #[test]
    fn additional_render_origins_are_accepted() {
        let state = test_state(Some("tauri://localhost"));
        assert!(state.origin_allowed(Some("tauri://localhost")));
        assert!(state.origin_allowed(Some(RENDER_ORIGIN)));
    }

    #[test]
    fn the_allow_list_is_deduplicated() {
        let origins = allowed_origins("http://127.0.0.1:8080", Some("http://127.0.0.1:8080/"));
        assert_eq!(
            origins,
            vec![
                "http://127.0.0.1:8080".to_owned(),
                "dsh-app://app".to_owned(),
                "dsh-app://localhost".to_owned(),
                "http://dsh-app.localhost".to_owned(),
            ]
        );
    }

    #[test]
    fn cors_echo_skips_same_origin_and_unknown_origins() {
        let state = test_state(None);
        // Same origin: no CORS headers are needed or sent.
        assert_eq!(state.cors_echo(Some("http://127.0.0.1:8080")), None);
        // The renderer page origin is echoed verbatim.
        assert_eq!(
            state.cors_echo(Some(RENDER_ORIGIN)).as_deref(),
            Some(RENDER_ORIGIN)
        );
        // A foreign origin never receives an echo (it is rejected before this).
        assert_eq!(state.cors_echo(Some("https://example.com")), None);
        assert_eq!(state.cors_echo(None), None);
    }

    #[test]
    fn host_origin_string_drops_the_path() {
        let state = test_state(None);
        assert_eq!(state.host_origin_string(), "http://127.0.0.1:4100");
    }

    #[test]
    fn routes_are_recorded_and_never_contain_the_cookie() {
        let dir = TempDir::new().expect("temp dir");
        let logger = Arc::new(
            crate::logging::Logger::open(dir.path(), LogLevel::Debug, false).expect("logger"),
        );
        let state = AppState {
            dist_dir: Arc::new(PathBuf::from("/tmp/dist")),
            boot_script: Arc::from(BOOT_SCRIPT),
            cookie: Arc::from("dsh=secret-value"),
            host_origin: Arc::new(Url::parse("http://127.0.0.1:4100").expect("url")),
            own_origin: Arc::from("http://127.0.0.1:8080"),
            allowed_origins: Arc::new(vec!["http://127.0.0.1:8080".to_owned()]),
            shell_assets: None,
            logger: Some(logger),
            shutdown: watch::channel(false).1,
        };
        state.debug(format!("app_server: GET /api -> {}", state.cookie().len()));
        let logged =
            std::fs::read_to_string(dir.path().join("dsh-desktop.log")).unwrap_or_default();
        let expected = format!("app_server: GET /api -> {}", "dsh=secret-value".len());
        assert!(logged.contains(&expected), "{logged}");
        // The route line can never carry the credential it was built from.
        assert!(!logged.contains("secret-value"));
    }

    fn test_state(render_origin: Option<&str>) -> AppState {
        let allowed = allowed_origins("http://127.0.0.1:8080", render_origin);
        AppState {
            dist_dir: Arc::new(PathBuf::from("/tmp/dist")),
            boot_script: Arc::from(BOOT_SCRIPT),
            cookie: Arc::from("dsh=secret-value"),
            host_origin: Arc::new(Url::parse("http://127.0.0.1:4100").expect("url")),
            own_origin: Arc::from("http://127.0.0.1:8080"),
            allowed_origins: Arc::new(allowed),
            shell_assets: None,
            logger: None,
            shutdown: watch::channel(false).1,
        }
    }
}
