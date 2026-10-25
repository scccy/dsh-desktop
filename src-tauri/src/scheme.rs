//! `dsh-app://` document scheme — the renderer's page origin.
//!
//! The official shell registers `dsh-app://` with Electron's `protocol.handle`
//! and answers three kinds of request from it:
//!
//! 1. `dsh-app://shell/<page>` — documents compiled into the shell itself
//!    (`loading.html`, `error.html`, `settings.html`, `header.html`), resolved
//!    through the Tauri asset resolver;
//! 2. `dsh-app://app/` — the packaged `dsh-web-frontend/dist` build: the index
//!    document (rewritten with [`BOOT_SCRIPT`] before the frontend module runs),
//!    `/assets/*`, the favicons and the web manifest;
//! 3. every other path under `app` — a **buffered** GET forwarded to the
//!    authenticated Host with the Host cookie injected (module scripts that the
//!    frontend loads by URL, e.g. `/plugins/…`).
//!
//! # Why the proxy is buffered
//!
//! A `WKURLSchemeHandler` only ever sees complete response bytes: there is no
//! streaming, and an SSE body is delivered in one lump after the upstream closes.
//! This is deliberate and measured (see `docs/desktop-parity.md` §6.1), so the
//! scheme carries only static documents and script GETs. Every dynamic exchange
//! (`fetch`, XHR, `EventSource`, WebSocket) belongs to the loopback application
//! server, whose origin the renderer talks to directly.
//!
//! # Differences from Electron that this module has to absorb
//!
//! - the page keeps `dsh-app://app` as its origin, so the Host cookie never
//!   reaches the renderer's cookie jar: responses lose `set-cookie`;
//! - `content-encoding` / `content-length` / `transfer-encoding` describe our own
//!   hop, not the renderer's, so the upstream values are dropped and replaced by
//!   the buffered body's length;
//! - `/plugins/*` is served `cache-control: no-store`: the Host marks those
//!   bundles immutable under a per-launch revision, so a webview disk cache would
//!   only accumulate bundles no later launch can reuse.
//!
//! 待与 app_server 合并：路由、MIME 表和 `<head>` 注入规则与
//! `dsh_lite_core::app_server::document` 是同一份官方规则；那个模块目前把
//! 这些助手保留为 `pub(crate)`，所以本模块保留一份自包含实现（常量已复用
//! `BOOT_SCRIPT` / `RENDER_ORIGIN` / `ShellAsset`）。等 `app_server` 把路由、
//! MIME 与注入暴露为 `pub` 后，这里应改为调用它们，而不是继续维护副本。

use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dsh_lite_core::app_server::{BOOT_SCRIPT, RENDER_ORIGIN, ShellAsset};
use tauri::http::{HeaderValue, Method, Request, Response, StatusCode, header};
use tauri::{AppHandle, Builder, Wry};
use url::Url;

/// Scheme the renderer documents are served from.
pub const SCHEME: &str = "dsh-app";

/// Host part of the shell's own pages.
const SHELL_HOST: &str = "shell";

/// Host part of the packaged application.
const APP_HOST: &str = "app";

/// Largest Host response the proxy buffers. The proxy only carries scripts and
/// static Host assets; anything larger is a programming error, not a document.
const MAX_PROXY_BODY: usize = 32 * 1024 * 1024;

/// Deadline for reaching the Host.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for one read or write against the Host.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Response headers that describe the upstream hop and must not reach the page.
///
/// This is the official `WITHHELD_RESPONSE_HEADERS` list: `set-cookie` would hand
/// the Host's authentication cookie to the renderer's cookie jar (the shell owns
/// it instead), and the rest describe the Node connection the webview never sees.
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

/// Authenticated Host the scheme proxies its script GETs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTarget {
    /// Origin of the Host, for example `http://127.0.0.1:4100`.
    pub origin: Url,
    /// Host cookie including its name, as returned by the Host handshake.
    pub cookie: String,
}

/// What the handler can reach when a request arrives.
///
/// The packaged documents exist as soon as the dsh install is resolved; the Host
/// only becomes reachable after authentication. A missing Host is answered `503`
/// for the proxied paths and leaves the packaged documents working, and a missing
/// target leaves the shell's own pages working — that is how the loading and
/// error pages can be the first thing the window ever loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemeTarget {
    /// Packaged `dsh-web-frontend/dist` directory.
    pub dist_dir: PathBuf,
    /// Authenticated Host, once its cookie is held.
    pub host: Option<HostTarget>,
}

/// Supplies the current [`SchemeTarget`] for a request.
///
/// The shell passes a closure that reads its own shared state, so this module
/// never depends on the state layout: the handler calls it on every request and
/// picks up a Host that became ready (or went away) in the meantime.
pub type TargetProvider = Arc<dyn Fn(&AppHandle<Wry>) -> Option<SchemeTarget> + Send + Sync>;

/// Resolves a shell page (`loading.html`, …) for the [`SCHEME`] handler.
///
/// Borrowed rather than `'static`: the handler builds it around the request's
/// [`AppHandle`].
type ShellPageResolver<'a> = dyn Fn(&str) -> Option<ShellAsset> + 'a;

/// Registers the [`SCHEME`] handler on the builder.
///
/// Must run before the builder is turned into an application: Tauri collects
/// custom schemes at `Builder` time.
#[must_use]
pub fn install(builder: Builder<Wry>, provider: TargetProvider) -> Builder<Wry> {
    builder.register_asynchronous_uri_scheme_protocol(SCHEME, move |ctx, request, responder| {
        let app = ctx.app_handle().clone();
        let provider = Arc::clone(&provider);
        // WKWebView hands one request at a time to its scheme handler thread and
        // waits for `respond`; dist reads and the buffered Host GET are blocking,
        // so they run on a worker instead of that queue.
        std::thread::spawn(move || responder.respond(dispatch(&provider, &app, &request)));
    })
}

/// Runs one request: resolves the target, then routes it.
fn dispatch(
    provider: &TargetProvider,
    app: &AppHandle,
    request: &Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let target = provider(app);
    let shell = |path: &str| {
        app.asset_resolver()
            .get(path.to_owned())
            .map(|asset| ShellAsset {
                status: 200,
                content_type: asset.mime_type().to_owned(),
                body: asset.bytes().to_vec(),
            })
    };
    respond(target.as_ref(), &shell, request)
}

/// Where a `dsh-app://` request is answered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// A document compiled into the shell; answered by the asset resolver.
    Shell,
    /// `dist/index.html`, rewritten with [`BOOT_SCRIPT`].
    Index,
    /// A file below the packaged dist directory.
    Asset,
    /// Anything else under `app`: a buffered GET to the Host.
    Host,
}

/// Classifies a request by its scheme host and pathname.
///
/// `None` means the host is not ours at all (404). The pathname is the *encoded*
/// one, exactly like the official handler, which tests `url.pathname` before the
/// document reader decodes it.
fn classify(host: &str, pathname: &str) -> Option<Route> {
    if host.eq_ignore_ascii_case(SHELL_HOST) {
        return Some(Route::Shell);
    }
    if !host.eq_ignore_ascii_case(APP_HOST) {
        return None;
    }
    Some(match pathname {
        "/" | "/index.html" => Route::Index,
        "/favicon.svg" | "/favicon-dark.svg" | "/manifest.webmanifest" => Route::Asset,
        path if path.starts_with("/assets/") => Route::Asset,
        _ => Route::Host,
    })
}

/// Answers one `dsh-app://` request.
///
/// `shell` resolves shell-owned pages; tests inject it instead of running a
/// Tauri application.
fn respond(
    target: Option<&SchemeTarget>,
    shell: &ShellPageResolver<'_>,
    request: &Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let uri = request.uri();
    let host = uri.host().unwrap_or_default().to_owned();
    let pathname = uri.path().to_owned();
    let head_only = request.method() == Method::HEAD;
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Some(route) = classify(&host, &pathname) else {
        return status(StatusCode::NOT_FOUND);
    };
    match route {
        // A missing (or unreadable) shell document would otherwise be an empty
        // 404 — a white window that explains nothing. Serve the built-in
        // document instead: it names the page that is missing and what to do.
        Route::Shell => match shell_page(shell, &pathname) {
            Some(asset) => document_response(&asset.content_type, asset.body, head_only),
            None => fallback_document(StatusCode::NOT_FOUND, &pathname, head_only),
        },
        Route::Index if target.is_none() => {
            // The renderer document without a resolved dist directory: the boot
            // never got far enough to have one. Still a page, not a blank window.
            fallback_document(StatusCode::SERVICE_UNAVAILABLE, &pathname, head_only)
        }
        Route::Index | Route::Asset => {
            let Some(target) = target else {
                return status(StatusCode::SERVICE_UNAVAILABLE);
            };
            let index = route == Route::Index;
            match read_dist(&target.dist_dir, &pathname, index) {
                Ok(file) => document_response(file.content_type, file.body, head_only),
                Err(status) => plain_status(status),
            }
        }
        Route::Host => {
            let Some(host) = target.and_then(|target| target.host.as_ref()) else {
                return status(StatusCode::SERVICE_UNAVAILABLE);
            };
            proxy(host, request, head_only)
        }
    }
}

/// Built-in document served when a document the shell itself should provide is
/// missing.
///
/// Its whole reason to exist is that the alternative is an empty response: the
/// WebView then shows a white window with no title, no explanation and nothing
/// in the log that points at the real problem (a damaged installation). The page
/// is self-contained — no stylesheet, no script, no `invoke` — because whatever
/// is missing may well be the stylesheet.
fn fallback_document(status: StatusCode, pathname: &str, head_only: bool) -> Response<Vec<u8>> {
    // The path comes straight from the request, so it is escaped before it is
    // interpolated: an error page must never be a way to inject markup.
    let pathname = html_escape(pathname);
    let body = format!(
        "<!doctype html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\n\
         <title>DSH Desktop — 页面缺失 / Page missing</title>\n\
         <style>body{{font:14px/1.6 -apple-system,system-ui,sans-serif;margin:0;padding:48px;color:#17181b;background:#f9fafb}}\n\
         h1{{font-size:18px;margin:0 0 12px}}code{{background:#eceef1;padding:1px 4px;border-radius:3px}}\n\
         p{{margin:0 0 8px;max-width:60ch}}.sub{{color:#5b616b}}</style></head><body>\n\
         <h1>DSH Desktop 的页面缺失 / A DSH Desktop page is missing</h1>\n\
         <p>壳没有取到它自己的文档 <code>{pathname}</code>。这通常意味着安装不完整。</p>\n\
         <p class=\"sub\">The shell could not read its own document <code>{pathname}</code>.\n\
         This usually means the installation is incomplete.</p>\n\
         <p>请重新安装 DSH Desktop；日志在应用数据目录的 <code>logs/</code> 下。</p>\n\
         <p class=\"sub\">Reinstall DSH Desktop. Logs are in <code>logs/</code> under the application data directory.</p>\n\
         </body></html>\n"
    );
    let mut response = document_response("text/html; charset=utf-8", body.into_bytes(), head_only);
    *response.status_mut() = status;
    response
}

/// Escapes the five characters that matter inside HTML text and attributes.
fn html_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Resolves a shell page, refusing anything that is not a flat document name.
fn shell_page(shell: &ShellPageResolver<'_>, pathname: &str) -> Option<ShellAsset> {
    let relative = pathname.strip_prefix('/')?;
    let decoded = percent_decode(relative)?;
    if decoded.is_empty()
        || decoded == "."
        || decoded == ".."
        || decoded.contains('/')
        || decoded.contains('\\')
    {
        return None;
    }
    let asset = shell(&decoded)?;
    (200..=299).contains(&asset.status).then_some(asset)
}

/// A packaged document ready to be sent.
struct StaticFile {
    content_type: &'static str,
    body: Vec<u8>,
}

/// Reads `pathname` from the dist directory; `Err` is the status to answer with.
fn read_dist(root: &Path, pathname: &str, index: bool) -> Result<StaticFile, u16> {
    let decoded = percent_decode(pathname).ok_or(400u16)?;
    let relative = if index {
        "/index.html".to_owned()
    } else {
        decoded
    };
    let target = contained_path(root, &relative).ok_or(403u16)?;
    let content = std::fs::read(&target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            404u16
        } else {
            500u16
        }
    })?;
    let body = if index {
        inject_boot_script(&content)
    } else {
        content
    };
    Ok(StaticFile {
        content_type: content_type(&target),
        body,
    })
}

/// Media type from the extension table of the official `web-document.ts`.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Inserts [`BOOT_SCRIPT`] after the first `<head>` tag.
///
/// This is the official `body.toString().replace('<head>', '<head>' + BOOT)`; an
/// index without a literal `<head>` is served unchanged rather than failing boot.
fn inject_boot_script(content: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(content)
        .replacen("<head>", &format!("<head>{BOOT_SCRIPT}"), 1)
        .into_bytes()
}

/// Resolves `decoded` below `root`, rejecting everything outside it.
///
/// The official comparison is `target.startsWith(root + sep)`, so the root
/// directory itself is not servable either.
fn contained_path(root: &Path, decoded: &str) -> Option<PathBuf> {
    let target = lexical_resolve(root, decoded);
    match target.strip_prefix(root) {
        Ok(remainder) if !remainder.as_os_str().is_empty() => Some(target),
        _ => None,
    }
}

/// Normalizes `decoded` against `root` the way Node's `path.resolve` does.
fn lexical_resolve(root: &Path, decoded: &str) -> PathBuf {
    let mut resolved = root.to_path_buf();
    for segment in decoded.split(SEPARATORS) {
        match segment {
            "" | "." => {}
            ".." => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// Path separators accepted while resolving a document path.
#[cfg(windows)]
const SEPARATORS: [char; 2] = ['/', '\\'];

/// Path separators accepted while resolving a document path.
#[cfg(not(windows))]
const SEPARATORS: [char; 1] = ['/'];

/// Response with a status and no body.
fn status(code: StatusCode) -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = code;
    response
}

/// Response for a numeric status the document readers report.
fn plain_status(code: u16) -> Response<Vec<u8>> {
    status(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
}

/// Response for a packaged document; `HEAD` keeps the headers but sends no body.
fn document_response(content_type: &str, body: Vec<u8>, head_only: bool) -> Response<Vec<u8>> {
    let length = body.len();
    let mut response = status(StatusCode::OK);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    *response.body_mut() = if head_only { Vec::new() } else { body };
    response
}

// ---------------------------------------------------------------------------
// buffered Host proxy
// ---------------------------------------------------------------------------

/// A Host response, fully read into memory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProxyResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Why a proxied request produced no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyError {
    /// The Host origin is unusable.
    Invalid,
    /// The Host could not be reached.
    Connect,
    /// The exchange failed or timed out.
    Io,
    /// The Host answer could not be framed.
    Malformed,
    /// The answer exceeded [`MAX_PROXY_BODY`].
    TooLarge,
}

/// Forwards one script GET to the Host and buffers the answer.
fn proxy(host: &HostTarget, request: &Request<Vec<u8>>, head_only: bool) -> Response<Vec<u8>> {
    // The official forwarder rejects any origin but the renderer's.
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or_default();
        if !origin.eq_ignore_ascii_case(RENDER_ORIGIN) {
            return status(StatusCode::FORBIDDEN);
        }
    }
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    // A protocol-relative target would redirect the request at another host.
    if path_and_query.starts_with("//") {
        return status(StatusCode::BAD_REQUEST);
    }
    let accept = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok());
    let upstream = match fetch(&host.origin, &path_and_query, &host.cookie, accept) {
        Ok(upstream) => upstream,
        Err(ProxyError::Io) => return status(StatusCode::GATEWAY_TIMEOUT),
        Err(_) => return status(StatusCode::BAD_GATEWAY),
    };
    build_response(upstream, &path_and_query, head_only)
}

/// Sends one `GET` and reads the complete answer.
fn fetch(
    origin: &Url,
    path_and_query: &str,
    cookie: &str,
    accept: Option<&str>,
) -> Result<ProxyResponse, ProxyError> {
    let host = origin.host_str().ok_or(ProxyError::Invalid)?;
    let port = origin.port_or_known_default().ok_or(ProxyError::Invalid)?;
    let authority = authority(origin).ok_or(ProxyError::Invalid)?;
    let request = format!(
        "GET {path_and_query} HTTP/1.1\r\nhost: {authority}\r\ncookie: {cookie}\r\naccept: {}\r\naccept-encoding: identity\r\nconnection: close\r\n\r\n",
        accept.unwrap_or("*/*")
    );

    let mut stream = connect(host, port)?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|_| ProxyError::Io)?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|_| ProxyError::Io)?;
    stream
        .write_all(request.as_bytes())
        .map_err(|_| ProxyError::Io)?;
    stream.flush().map_err(|_| ProxyError::Io)?;

    // `connection: close` frames the answer at EOF, so the whole response can be
    // read before it is parsed. The identity encoding keeps the bytes as sent.
    let mut raw = Vec::new();
    BufReader::new(stream)
        .take(MAX_PROXY_BODY as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|_| ProxyError::Io)?;
    if raw.len() > MAX_PROXY_BODY {
        return Err(ProxyError::TooLarge);
    }
    parse_response(&raw).ok_or(ProxyError::Malformed)
}

/// Connects to the first reachable address of the Host.
fn connect(host: &str, port: u16) -> Result<TcpStream, ProxyError> {
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| ProxyError::Connect)?
        .collect();
    if addresses.is_empty() {
        return Err(ProxyError::Connect);
    }
    let mut last = ProxyError::Connect;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(_) => last = ProxyError::Connect,
        }
    }
    Err(last)
}

/// Parses a complete HTTP/1.1 response.
///
/// Kept free of sockets so every framing rule has a plain unit test.
fn parse_response(raw: &[u8]) -> Option<ProxyResponse> {
    let (head_end, body_start) = header_end(raw)?;
    let head = std::str::from_utf8(&raw[..head_end]).ok()?;
    let mut lines = head.split('\n').map(|line| line.trim_end_matches('\r'));
    let mut status_parts = lines.next()?.split_whitespace();
    if !status_parts.next()?.starts_with("HTTP/") {
        return None;
    }
    let status = status_parts.next()?.parse::<u16>().ok()?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }

    let body_bytes = &raw[body_start..];
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
    });
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok());
    let body = if matches!(status, 204 | 304) {
        Vec::new()
    } else if chunked {
        dechunk(body_bytes)?
    } else if let Some(length) = length {
        body_bytes.get(..length.min(body_bytes.len()))?.to_vec()
    } else {
        body_bytes.to_vec()
    };
    Some(ProxyResponse {
        status,
        headers,
        body,
    })
}

/// Position of the blank line that ends the header block, and of the body.
fn header_end(raw: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'\n' {
            // `\n\n` or `\r\n\r\n`
            if raw.get(index + 1) == Some(&b'\n') {
                return Some((index, index + 2));
            }
            if raw.get(index + 1) == Some(&b'\r') && raw.get(index + 2) == Some(&b'\n') {
                return Some((index, index + 3));
            }
        }
        index += 1;
    }
    None
}

/// Decodes a chunked body, ignoring any trailers.
fn dechunk(raw: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let mut cursor = 0;
    loop {
        let (line, next) = line_at(raw, cursor)?;
        let size_text = line.split(';').next()?.trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        cursor = next;
        if size == 0 {
            return Some(body);
        }
        let end = cursor.checked_add(size)?;
        body.extend_from_slice(raw.get(cursor..end)?);
        cursor = end;
        // Each chunk is followed by its own line break.
        let (_, next) = line_at(raw, cursor)?;
        cursor = next;
    }
}

/// Reads one line starting at `cursor`, returning it and the next position.
fn line_at(raw: &[u8], cursor: usize) -> Option<(&str, usize)> {
    let rest = raw.get(cursor..)?;
    let end = rest.iter().position(|byte| *byte == b'\n')?;
    let line = std::str::from_utf8(&rest[..end])
        .ok()?
        .trim_end_matches('\r');
    Some((line, cursor + end + 1))
}

/// Builds the renderer-facing response from a buffered Host answer.
fn build_response(
    upstream: ProxyResponse,
    path_and_query: &str,
    head_only: bool,
) -> Response<Vec<u8>> {
    let plugin_bundle = path_and_query.starts_with("/plugins/");
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY));
    for (name, value) in &upstream.headers {
        if is_withheld_header(name) {
            continue;
        }
        // The plugin override replaces the Host's own cache policy.
        if plugin_bundle && name.eq_ignore_ascii_case("cache-control") {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    if plugin_bundle {
        builder = builder.header(header::CACHE_CONTROL.as_str(), "no-store");
    }
    builder = builder.header(
        header::CONTENT_LENGTH.as_str(),
        upstream.body.len().to_string(),
    );
    let body = if head_only { Vec::new() } else { upstream.body };
    builder
        .body(body)
        .unwrap_or_else(|_| status(StatusCode::BAD_GATEWAY))
}

/// Whether an upstream header must not be relayed.
fn is_withheld_header(name: &str) -> bool {
    WITHHELD_RESPONSE_HEADERS
        .iter()
        .any(|withheld| name.eq_ignore_ascii_case(withheld))
}

/// `host[:port]` of a URL, with IPv6 literals bracketed.
fn authority(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// Decodes percent escapes like `decodeURIComponent`.
///
/// Malformed escapes and sequences that are not valid UTF-8 are rejected, which
/// is what turns the official 400 into a rejected path here.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Value of one hexadecimal digit.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn get(target: &str) -> Request<Vec<u8>> {
        Request::builder()
            .method("GET")
            .uri(target)
            .body(Vec::new())
            .expect("request")
    }

    fn no_shell(_: &str) -> Option<ShellAsset> {
        None
    }

    fn ready_target(origin: &str) -> SchemeTarget {
        SchemeTarget {
            dist_dir: PathBuf::from("/tmp/dist"),
            host: Some(HostTarget {
                origin: Url::parse(origin).expect("url"),
                cookie: "dsh-auth=abc".to_owned(),
            }),
        }
    }

    /// Directory removed when the test ends; `src-tauri` has no `tempfile`.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("dsh-scheme-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// A dist directory holding an index document and one module script.
    fn dist_fixture(name: &str) -> TempDir {
        let directory = TempDir::new(name);
        fs::create_dir_all(directory.path().join("assets")).expect("assets dir");
        fs::write(
            directory.path().join("index.html"),
            "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>",
        )
        .expect("index");
        fs::write(
            directory.path().join("assets/app.js"),
            "export const x = 1\n",
        )
        .expect("module");
        fs::write(directory.path().join("favicon.svg"), "<svg/>").expect("favicon");
        directory
    }

    #[test]
    fn scheme_hosts_and_paths_route_like_the_official_handler() {
        assert_eq!(classify("app", "/"), Some(Route::Index));
        assert_eq!(classify("app", "/index.html"), Some(Route::Index));
        assert_eq!(classify("app", "/assets/index-abc.js"), Some(Route::Asset));
        assert_eq!(classify("app", "/favicon.svg"), Some(Route::Asset));
        assert_eq!(classify("app", "/favicon-dark.svg"), Some(Route::Asset));
        assert_eq!(classify("app", "/manifest.webmanifest"), Some(Route::Asset));
        assert_eq!(classify("app", "/plugins/x/index.js"), Some(Route::Host));
        assert_eq!(classify("app", "/api/sessions"), Some(Route::Host));
        assert_eq!(classify("shell", "/loading.html"), Some(Route::Shell));
        assert_eq!(classify("APP", "/"), Some(Route::Index));
        assert_eq!(classify("SHELL", "/error.html"), Some(Route::Shell));
        assert_eq!(classify("other", "/"), None);
        assert_eq!(classify("", "/"), None);
    }

    #[test]
    fn the_index_document_gets_the_boot_script_once() {
        let page = "<html><head><meta charset=\"utf-8\"></head></html>";
        let injected = String::from_utf8(inject_boot_script(page.as_bytes())).expect("utf-8");
        assert!(injected.contains(&format!("<head>{BOOT_SCRIPT}")));
        assert_eq!(injected.matches(BOOT_SCRIPT).count(), 1);
        assert_eq!(injected.matches("<head>").count(), 1);
    }

    #[test]
    fn an_index_without_a_head_is_served_unchanged() {
        let page = "<html><body>no head</body></html>";
        assert_eq!(inject_boot_script(page.as_bytes()), page.as_bytes());
    }

    #[test]
    fn the_mime_table_matches_the_official_one() {
        for (name, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("main.js", "text/javascript; charset=utf-8"),
            ("style.css", "text/css; charset=utf-8"),
            ("logo.svg", "image/svg+xml"),
            ("data.json", "application/json"),
            ("font.woff2", "font/woff2"),
            ("icon.png", "image/png"),
            ("favicon.ico", "image/x-icon"),
            ("manifest.webmanifest", "application/octet-stream"),
            ("UPPER.JS", "application/octet-stream"),
        ] {
            assert_eq!(content_type(Path::new(name)), expected, "for {name}");
        }
    }

    #[test]
    fn documents_never_escape_the_dist_directory() {
        let root = Path::new("/tmp/dist");
        assert_eq!(
            contained_path(root, "/assets/app.js"),
            Some(PathBuf::from("/tmp/dist/assets/app.js"))
        );
        assert_eq!(contained_path(root, "/../secret"), None);
        assert_eq!(contained_path(root, "/../../etc/passwd"), None);
        assert_eq!(contained_path(root, "/assets/../../secret"), None);
        assert_eq!(contained_path(root, "/"), None);
    }

    #[test]
    fn encoded_traversal_cannot_smuggle_a_path() {
        let decoded = percent_decode("/..%2f..%2fetc%2fpasswd").expect("decodes");
        assert_eq!(decoded, "/../../etc/passwd");
        assert_eq!(contained_path(Path::new("/tmp/dist"), &decoded), None);
        assert_eq!(percent_decode("/a%"), None);
        assert_eq!(percent_decode("/%80"), None);
    }

    #[test]
    fn only_get_and_head_are_answered() {
        let request = Request::builder()
            .method("POST")
            .uri("dsh-app://app/")
            .body(Vec::new())
            .expect("request");
        let response = respond(None, &no_shell, &request);
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn an_unknown_scheme_host_is_a_404() {
        let response = respond(None, &no_shell, &get("dsh-app://elsewhere/index.html"));
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn shell_pages_load_without_a_target() {
        let shell = |path: &str| {
            (path == "loading.html").then(|| ShellAsset {
                status: 200,
                content_type: "text/html; charset=utf-8".to_owned(),
                body: b"<html>loading</html>".to_vec(),
            })
        };
        let response = respond(None, &shell, &get("dsh-app://shell/loading.html"));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body(), b"<html>loading</html>");
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );

        let missing = respond(None, &shell, &get("dsh-app://shell/error.html"));
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let traversal = respond(None, &shell, &get("dsh-app://shell/../index.html"));
        assert_eq!(traversal.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_missing_shell_page_is_not_an_error_page_loop() {
        let erroring = |_: &str| {
            Some(ShellAsset {
                status: 500,
                content_type: "text/html".to_owned(),
                body: Vec::new(),
            })
        };
        let response = respond(None, &erroring, &get("dsh-app://shell/error.html"));
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_missing_shell_document_is_a_readable_page_not_a_white_window() {
        let response = respond(None, &no_shell, &get("dsh-app://shell/error.html"));
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = String::from_utf8(response.body().clone()).expect("utf-8");
        assert!(body.contains("error.html"), "the page names it: {body}");
        assert!(body.contains("</html>"));
        assert!(
            !body.contains("<script"),
            "the fallback must not depend on anything that may be missing too"
        );
    }

    #[test]
    fn the_renderer_document_without_a_target_still_renders_a_page() {
        let response = respond(None, &no_shell, &get("dsh-app://app/"));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = String::from_utf8(response.body().clone()).expect("utf-8");
        assert!(body.contains("</html>"));
    }

    #[test]
    fn the_fallback_page_sends_no_body_for_a_head_request() {
        let request = Request::builder()
            .method(Method::HEAD)
            .uri("dsh-app://shell/error.html")
            .body(Vec::new())
            .expect("request");
        let response = respond(None, &no_shell, &request);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.body().is_empty());
        assert!(response.headers().get(header::CONTENT_LENGTH).is_some());
    }

    #[test]
    fn packaged_documents_are_served_with_their_mime_type() {
        let directory = dist_fixture("documents");
        let target = SchemeTarget {
            dist_dir: directory.path().to_path_buf(),
            host: None,
        };

        let index = respond(Some(&target), &no_shell, &get("dsh-app://app/index.html"));
        assert_eq!(index.status(), StatusCode::OK);
        assert_eq!(
            index.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = String::from_utf8(index.body().clone()).expect("utf-8");
        assert!(
            body.contains(BOOT_SCRIPT),
            "index must carry the boot script"
        );

        let module = respond(
            Some(&target),
            &no_shell,
            &get("dsh-app://app/assets/app.js"),
        );
        assert_eq!(module.status(), StatusCode::OK);
        assert_eq!(
            module.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(module.body(), b"export const x = 1\n");
    }

    #[test]
    fn head_keeps_the_headers_but_sends_no_body() {
        let directory = dist_fixture("head");
        let target = SchemeTarget {
            dist_dir: directory.path().to_path_buf(),
            host: None,
        };
        let request = Request::builder()
            .method("HEAD")
            .uri("dsh-app://app/assets/app.js")
            .body(Vec::new())
            .expect("request");
        let response = respond(Some(&target), &no_shell, &request);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.body().is_empty());
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).unwrap(),
            "19"
        );
    }

    #[test]
    fn a_missing_document_is_a_404_and_a_traversal_is_not_served() {
        let directory = dist_fixture("missing");
        let target = SchemeTarget {
            dist_dir: directory.path().to_path_buf(),
            host: None,
        };
        let missing = respond(
            Some(&target),
            &no_shell,
            &get("dsh-app://app/assets/missing.js"),
        );
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let traversal = respond(
            Some(&target),
            &no_shell,
            &get("dsh-app://app/assets/../../../../etc/passwd"),
        );
        assert!(matches!(
            traversal.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ));
    }

    #[test]
    fn a_missing_host_answer_is_a_503_not_an_empty_page() {
        let directory = dist_fixture("unavailable");
        // dist known, Host not authenticated yet
        let target = SchemeTarget {
            dist_dir: directory.path().to_path_buf(),
            host: None,
        };
        let response = respond(
            Some(&target),
            &no_shell,
            &get("dsh-app://app/plugins/x/index.js"),
        );
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        // nothing known at all
        let response = respond(None, &no_shell, &get("dsh-app://app/"));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_foreign_origin_is_refused_before_reaching_the_host() {
        let target = ready_target("http://127.0.0.1:9");
        let request = Request::builder()
            .method("GET")
            .uri("dsh-app://app/plugins/x.js")
            .header("origin", "https://evil.example")
            .body(Vec::new())
            .expect("request");
        let response = respond(Some(&target), &no_shell, &request);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_chunked_host_answer_is_decoded_and_its_cookie_withheld() {
        let raw = concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/javascript\r\n",
            "set-cookie: dsh-auth=secret; Path=/\r\n",
            "content-encoding: gzip\r\n",
            "transfer-encoding: chunked\r\n",
            "cache-control: public, max-age=31536000, immutable\r\n",
            "\r\n",
            "5\r\nhello\r\n",
            "6\r\n world\r\n",
            "0\r\n\r\n",
        );
        let upstream = parse_response(raw.as_bytes()).expect("parsed");
        assert_eq!(upstream.status, 200);
        assert_eq!(upstream.body, b"hello world");

        let response = build_response(upstream, "/plugins/x/index.js", false);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body(), b"hello world");
        assert!(
            response.headers().get(header::SET_COOKIE).is_none(),
            "the Host cookie must never reach the renderer"
        );
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(response.headers().get(header::TRANSFER_ENCODING).is_none());
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).unwrap(),
            "11"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
    }

    #[test]
    fn a_plain_host_answer_keeps_its_cache_policy() {
        let raw = concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/javascript\r\n",
            "content-length: 2\r\n",
            "cache-control: public, max-age=600\r\n",
            "\r\n",
            "ok",
        );
        let upstream = parse_response(raw.as_bytes()).expect("parsed");
        assert_eq!(upstream.body, b"ok");
        let response = build_response(upstream, "/something.js", false);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=600"
        );
    }

    #[test]
    fn a_connection_closed_body_without_length_is_read_to_the_end() {
        let raw = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\r\nall of it";
        let upstream = parse_response(raw.as_bytes()).expect("parsed");
        assert_eq!(upstream.body, b"all of it");
    }

    #[test]
    fn a_malformed_answer_is_rejected() {
        assert!(parse_response(b"").is_none());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\ncontent-type: x\r\n").is_none());
        assert!(parse_response(b"nonsense\r\n\r\n").is_none());
        assert!(parse_response(b"HTTP/1.1 abc OK\r\n\r\n").is_none());
    }

    #[test]
    fn host_headers_are_withheld_case_insensitively() {
        for name in [
            "Set-Cookie",
            "CONTENT-LENGTH",
            "Transfer-Encoding",
            "connection",
        ] {
            assert!(is_withheld_header(name), "{name} must be withheld");
        }
        for name in ["content-type", "cache-control", "etag"] {
            assert!(!is_withheld_header(name), "{name} must be relayed");
        }
    }

    /// One-shot upstream server: answers `response` and reports the request line.
    fn upstream_server(response: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 4096];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let _ = sender.send(String::from_utf8_lossy(&buffer[..read]).into_owned());
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (port, receiver)
    }

    #[test]
    fn a_script_get_reaches_the_host_with_the_cookie_and_keeps_no_cookie() {
        let (port, requests) = upstream_server(concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/javascript; charset=utf-8\r\n",
            "set-cookie: dsh-auth=secret\r\n",
            "transfer-encoding: chunked\r\n",
            "\r\n",
            "4\r\nmod.\r\n0\r\n\r\n",
        ));
        let origin = Url::parse(&format!("http://127.0.0.1:{port}")).expect("url");
        let target = SchemeTarget {
            dist_dir: PathBuf::from("/tmp/dist"),
            host: Some(HostTarget {
                origin,
                cookie: "dsh-auth=abc".to_owned(),
            }),
        };
        let request = Request::builder()
            .method("GET")
            .uri("dsh-app://app/plugins/x/index.js?rev=7")
            .header("accept", "*/*")
            .body(Vec::new())
            .expect("request");

        let response = respond(Some(&target), &no_shell, &request);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body(), b"mod.");
        assert!(response.headers().get(header::SET_COOKIE).is_none());

        let sent = requests
            .recv_timeout(Duration::from_secs(5))
            .expect("request");
        assert!(
            sent.starts_with("GET /plugins/x/index.js?rev=7 HTTP/1.1\r\n"),
            "sent: {sent}"
        );
        assert!(sent.contains("cookie: dsh-auth=abc"), "sent: {sent}");
        assert!(sent.contains("accept-encoding: identity"), "sent: {sent}");
        assert!(sent.contains("connection: close"), "sent: {sent}");
    }

    #[test]
    fn an_unreachable_host_is_a_bad_gateway() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let origin = Url::parse(&format!("http://127.0.0.1:{port}")).expect("url");
        let target = SchemeTarget {
            dist_dir: PathBuf::from("/tmp/dist"),
            host: Some(HostTarget {
                origin,
                cookie: "dsh-auth=abc".to_owned(),
            }),
        };
        let response = respond(
            Some(&target),
            &no_shell,
            &get("dsh-app://app/plugins/x/index.js"),
        );
        assert!(
            matches!(
                response.status(),
                StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT
            ),
            "unexpected status {}",
            response.status()
        );
    }
}
