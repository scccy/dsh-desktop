//! End-to-end check of the app server against a real Desktop Host.
//!
//! Ignored by default because it needs a real dsh installation, Node on PATH and
//! the packaged frontend. Run it explicitly:
//!
//! ```sh
//! DSH_DESKTOP_E2E_DSH_PATH=/path/to/node_modules/@deepseek-ai/dsh \
//! DSH_DESKTOP_E2E_DIST=/path/to/dsh-web-frontend/dist \
//! DSH_DESKTOP_E2E_NODE=$(command -v node) \
//!   cargo test -p dsh-lite-core --test app_server_e2e -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use url::Url;

use dsh_lite_core::app_server::{
    AppServer, AppServerConfig, BOOT_SCRIPT, RENDER_ORIGIN, authenticate_host,
};
use dsh_lite_core::desktop_host::{self, HostSpec};

#[test]
#[ignore = "requires a real dsh install and the packaged frontend"]
fn serves_the_real_host_end_to_end() {
    let dsh_package = match std::env::var("DSH_DESKTOP_E2E_DSH_PATH") {
        Ok(value) => PathBuf::from(value),
        Err(_) => return,
    };
    let dist_dir = match std::env::var("DSH_DESKTOP_E2E_DIST") {
        Ok(value) => PathBuf::from(value),
        Err(_) => return,
    };
    let node = std::env::var("DSH_DESKTOP_E2E_NODE").unwrap_or_else(|_| "node".to_owned());
    let spec = HostSpec {
        node: node.into(),
        script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("src-tauri")
            .join("host")
            .join("desktop-host.mjs"),
        dsh_package,
        project_dir: HostSpec::profile_dir_from_process_env().expect("a profile directory"),
        version: "0.1.0".to_owned(),
        env: Vec::new(),
    };
    let gate = desktop_host::probe_version(&spec, Duration::from_secs(20)).expect("dsh --version");
    assert!(gate.is_supported(), "this dsh is refused");
    let mut host = desktop_host::spawn(&spec, Duration::from_secs(30)).expect("desktop host ready");
    let ready_url = host.ready().url.clone();
    let origin = host.ready().origin.clone();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let cookie = authenticate_host(&ready_url).await.expect("cookie");
        assert!(cookie.contains('='), "the cookie carries a name and value");
        assert!(!cookie.contains(';'), "attributes are stripped: {cookie}");
        assert!(!cookie.is_empty());

        let server =
            AppServer::start(AppServerConfig::new(&dist_dir, origin.clone(), cookie.clone()))
                .await
                .expect("app server");

        // The exchanged cookie is what the Host accepts; the same path is refused
        // without it.
        let authority = format!(
            "{}:{}",
            origin.host_str().expect("host"),
            origin.port().expect("port")
        );
        let anonymous = request(&origin, "GET", "/", &[("host", &authority)]).await;
        let authenticated = request(
            &origin,
            "GET",
            "/",
            &[("host", &authority), ("cookie", &cookie)],
        )
        .await;
        assert_eq!(authenticated.status, StatusCode::OK);
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "the Host must require the shell's cookie"
        );

        // Static: the packaged index with the boot script injected.
        let index = get(server.origin(), "/", &[]).await;
        assert_eq!(index.status, StatusCode::OK);
        assert_eq!(
            index.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        let body = String::from_utf8_lossy(&index.body);
        assert!(
            body.contains(&format!("<head>{BOOT_SCRIPT}")),
            "boot script injected"
        );
        assert!(body.contains("<div id=\"root\""), "the real frontend index");

        // Static: a packaged asset and the manifest.
        let asset_path = first_asset(&dist_dir).expect("a packaged asset");
        let asset = get(server.origin(), &asset_path, &[]).await;
        assert_eq!(asset.status, StatusCode::OK);
        assert_eq!(
            asset.header("content-type"),
            Some("text/javascript; charset=utf-8")
        );
        let manifest = get(server.origin(), "/manifest.webmanifest", &[]).await;
        assert_eq!(manifest.status, StatusCode::OK);

        // Proxy: `//` reaches the Host's app shell, which requires the cookie.
        // The identical path with a bogus cookie is refused, proving that the
        // forwarded request is authenticated by the cookie this server injects.
        let shell = get(server.origin(), "//", &[]).await;
        assert_eq!(shell.status, StatusCode::OK);
        assert_eq!(shell.header("set-cookie"), None);
        let stranger =
            AppServer::start(AppServerConfig::new(&dist_dir, origin.clone(), "dsh=bogus"))
                .await
                .expect("app server");
        let bogus = get(stranger.origin(), "//", &[]).await;
        stranger.shutdown().await;
        assert_eq!(
            bogus.status,
            StatusCode::UNAUTHORIZED,
            "a bogus cookie must not be accepted"
        );

        // Proxy: an unknown Host path answers instead of failing to connect.
        let unknown = get(server.origin(), "/__dsh_e2e_unknown_path", &[]).await;
        assert_ne!(unknown.status, StatusCode::BAD_GATEWAY);

        // CORS: the renderer origin is echoed on real answers and preflights.
        let cors = get(
            server.origin(),
            "/__dsh_e2e_unknown_path",
            &[("origin", RENDER_ORIGIN)],
        )
        .await;
        assert_eq!(
            cors.header("access-control-allow-origin"),
            Some(RENDER_ORIGIN)
        );
        let preflight = request(
            server.origin(),
            "OPTIONS",
            "/api",
            &[
                ("origin", RENDER_ORIGIN),
                ("access-control-request-method", "POST"),
                ("access-control-request-headers", "content-type"),
            ],
        )
        .await;
        assert_eq!(preflight.status, StatusCode::NO_CONTENT);
        assert_eq!(
            preflight.header("access-control-allow-origin"),
            Some(RENDER_ORIGIN)
        );

        // WebSocket: the renderer's Remote mux channel upgrades through the proxy
        // with the shell's cookie, while a foreign origin is refused here.
        let socket_authority = format!(
            "{}:{}",
            server.origin().host_str().expect("host"),
            server.origin().port().expect("port")
        );
        let mut upgrade = format!("ws://{socket_authority}/api/remote.mux")
            .into_client_request()
            .expect("ws request");
        upgrade
            .headers_mut()
            .insert(header::ORIGIN, HeaderValue::from_static(RENDER_ORIGIN));
        let (_, upgraded) = connect_async(upgrade)
            .await
            .expect("the websocket upgrades through the proxy");
        assert_eq!(upgraded.status(), StatusCode::SWITCHING_PROTOCOLS);

        let mut foreign = format!("ws://{socket_authority}/api/remote.mux")
            .into_client_request()
            .expect("ws request");
        foreign.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://elsewhere.invalid"),
        );
        assert!(
            connect_async(foreign).await.is_err(),
            "a foreign origin must not be upgraded"
        );

        println!(
            "host: anonymous={} authenticated={} | app: index={}B asset={asset_path} {:?} manifest={} | proxy: shell={} bogus={} unknown={} | cors: get={} preflight={} | websocket={}",
            anonymous.status.as_u16(),
            authenticated.status.as_u16(),
            index.body.len(),
            asset.header("content-type"),
            manifest.status.as_u16(),
            shell.status.as_u16(),
            bogus.status.as_u16(),
            unknown.status.as_u16(),
            cors.status.as_u16(),
            preflight.status.as_u16(),
            upgraded.status().as_u16(),
        );
        server.shutdown().await;
    });

    host.stop(Duration::from_secs(8));
}

struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

async fn get(origin: &Url, path: &str, headers: &[(&str, &str)]) -> Response {
    request(origin, "GET", path, headers).await
}

async fn request(origin: &Url, method: &str, path: &str, headers: &[(&str, &str)]) -> Response {
    let stream = TcpStream::connect((
        origin.host_str().expect("host"),
        origin.port().expect("port"),
    ))
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
        .uri(format!("{}{path}", origin.as_str().trim_end_matches('/')));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(http_body_util::Full::new(Bytes::new()))
        .expect("request");
    let response = sender.send_request(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    Response {
        status,
        headers,
        body,
    }
}

/// First JavaScript asset below `dist/assets`, as a request path.
fn first_asset(dist_dir: &std::path::Path) -> Option<String> {
    let mut entries: Vec<_> = std::fs::read_dir(dist_dir.join("assets"))
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".js"))
        .collect();
    entries.sort();
    entries.first().map(|name| format!("/assets/{name}"))
}
