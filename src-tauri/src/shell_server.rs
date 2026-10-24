//! Read-only HTTP server for the shell's own pages.
//!
//! `dsh web` sets a `SameSite=Strict` auth cookie on the token redirect, and
//! WebKit withholds that cookie when the redirect is initiated from a
//! cross-site page. Tauri only serves the bundled shell assets from
//! `tauri://localhost`, which is a different site from the loopback `dsh web`
//! server, so loading the token URL straight from the loading page lands on the
//! 401 page.
//!
//! Serving the same embedded assets from a loopback http origin puts the shell's
//! pages on the same site as `dsh web` (same host; SameSite ignores the port),
//! so the token handshake succeeds in a single hop with no intermediate page.
//!
//! The server is read-only: it answers `GET` by looking the path up in the Tauri
//! asset resolver and never writes anything or reaches the network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use tauri::AppHandle;
use url::Url;

/// A running shell static server. Dropping the handle is fine: the worker
/// thread keeps serving until the process exits.
pub struct ShellServer {
    origin: Url,
}

impl ShellServer {
    /// `http://127.0.0.1:<port>/` of the running server.
    pub fn origin(&self) -> &Url {
        &self.origin
    }
}

/// Starts the server on an OS-assigned loopback port.
pub fn start(app: AppHandle) -> std::io::Result<ShellServer> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let origin =
        Url::parse(&format!("http://127.0.0.1:{port}/")).expect("a loopback origin is a valid URL");

    thread::Builder::new()
        .name("shell-static".into())
        .spawn(move || serve(listener, app))?;

    Ok(ShellServer { origin })
}

/// Serves one request per connection from the embedded assets.
fn serve(listener: TcpListener, app: AppHandle) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut buffer = [0u8; 8192];
        let Ok(read) = stream.read(&mut buffer) else {
            continue;
        };
        if read == 0 {
            continue;
        }
        let request = String::from_utf8_lossy(&buffer[..read]);
        let target = request.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));

        let (status, mime, body) = if path == "/__bridge" {
            bridge_response(query)
        } else {
            let relative = path.trim_start_matches('/');
            let relative = if relative.is_empty() {
                "loading.html"
            } else {
                relative
            };
            let resolver = app.asset_resolver();
            match resolver.get(relative.to_string()) {
                Some(asset) => (
                    "200 OK",
                    asset.mime_type().to_string(),
                    asset.bytes().to_vec(),
                ),
                None => (
                    "404 Not Found",
                    "text/plain; charset=utf-8".to_string(),
                    b"not found".to_vec(),
                ),
            }
        };
        write_response(&mut stream, status, &mime, &body);
    }
}

/// Serves `/__bridge?to=<loopback-url>`: a tiny page on the shell's loopback http
/// origin that redirects to `dsh web`.
///
/// Navigating to `dsh web` from a Tauri-origin page is cross-site, so WebKit
/// withholds the `SameSite=Strict` auth cookie and the app would 401. Routing
/// through this same-site page makes the redirect same-site, so the cookie is
/// sent. The target must be a loopback http(s) URL, so this is not an open
/// redirect.
fn bridge_response(query: &str) -> (&'static str, String, Vec<u8>) {
    let target = url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == "to")
        .and_then(|(_, value)| Url::parse(&value).ok())
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && match url.host() {
                    Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                    Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                    _ => false,
                }
        });
    match target {
        Some(url) => {
            let escaped = url.as_str().replace('&', "&amp;").replace('"', "&quot;");
            let html = format!(
                "<!doctype html><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0;url={escaped}\"><title>DSH Desktop</title>"
            );
            (
                "200 OK",
                "text/html; charset=utf-8".to_string(),
                html.into_bytes(),
            )
        }
        None => (
            "400 Bad Request",
            "text/plain; charset=utf-8".to_string(),
            b"bad bridge target".to_vec(),
        ),
    }
}

/// Writes one HTTP/1.1 response and closes the connection.
fn write_response(stream: &mut impl Write, status: &str, mime: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}
