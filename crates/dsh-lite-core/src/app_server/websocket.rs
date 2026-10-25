//! WebSocket proxy between the renderer and the Desktop Host.
//!
//! The official shell keeps the Host cookie in the main process and adds it in
//! `webRequest.onBeforeSendHeaders` for `ws://127.0.0.1/*`. A WebView has no
//! equivalent hook, so the renderer connects to this proxy instead and the shell
//! performs the upstream handshake itself: the Host sees the shell's `cookie`,
//! an `origin` equal to its own origin, and `sec-fetch-site: same-origin`.
//!
//! Frames are relayed verbatim in both directions until either side closes.

use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use url::Url;

use crate::app_server::{AppBody, AppState};

/// How long the upstream handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upstream socket as tokio-tungstenite creates it (loopback plain TCP).
type HostSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Whether the request asks to upgrade to a WebSocket.
pub(crate) fn wants_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("websocket"))
        })
}

/// WebSocket URL for `request_uri` on the Host origin.
pub(crate) fn host_socket_url(origin: &Url, request_uri: &Uri) -> Option<Url> {
    let scheme = match origin.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => return None,
    };
    let authority = crate::app_server::authority(origin)?;
    let path_and_query = request_uri
        .path_and_query()
        .map_or("/", |path_and_query| path_and_query.as_str());
    Url::parse(&format!("{scheme}://{authority}{path_and_query}")).ok()
}

/// Answers a WebSocket upgrade, relaying frames to the authenticated Host.
pub(crate) async fn proxy(mut request: Request<Incoming>, state: AppState) -> Response<AppBody> {
    let pathname = request.uri().path().to_owned();
    let Some(key) = request
        .headers()
        .get(header::SEC_WEBSOCKET_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        state.debug(format!("app_server: websocket {pathname} -> 400 (no key)"));
        return crate::app_server::status_response(StatusCode::BAD_REQUEST);
    };
    let Some(url) = host_socket_url(state.host_origin(), request.uri()) else {
        state.debug(format!(
            "app_server: websocket {pathname} -> 502 (no host url)"
        ));
        return crate::app_server::status_response(StatusCode::BAD_GATEWAY);
    };
    let Ok(cookie) = HeaderValue::from_str(state.cookie()) else {
        return crate::app_server::status_response(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let origin_header = match HeaderValue::from_str(&state.host_origin_string()) {
        Ok(value) => value,
        Err(_) => return crate::app_server::status_response(StatusCode::BAD_GATEWAY),
    };
    let protocol = request
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .cloned();

    // `into_client_request` for a `ws://` string fills in the mandatory handshake
    // headers (including a fresh `sec-websocket-key`); the shell adds the
    // credentials the Host expects.
    let Ok(mut upstream) = url.as_str().into_client_request() else {
        state.debug(format!("app_server: websocket {pathname} -> 502 (bad url)"));
        return crate::app_server::status_response(StatusCode::BAD_GATEWAY);
    };
    {
        let headers = upstream.headers_mut();
        headers.insert(header::COOKIE, cookie);
        headers.insert(header::ORIGIN, origin_header);
        headers.insert(
            HeaderName::from_static("sec-fetch-site"),
            HeaderValue::from_static("same-origin"),
        );
        if let Some(protocol) = protocol {
            headers.insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
        }
    }

    let handshake = timeout(HANDSHAKE_TIMEOUT, connect_async(upstream)).await;
    let (host, response) = match handshake {
        Ok(Ok(pair)) => pair,
        Ok(Err(error)) => {
            state.debug(format!(
                "app_server: websocket {pathname} -> 502 (handshake: {error})"
            ));
            return crate::app_server::status_response(StatusCode::BAD_GATEWAY);
        }
        Err(_) => {
            state.debug(format!(
                "app_server: websocket {pathname} -> 504 (handshake timed out)"
            ));
            return crate::app_server::status_response(StatusCode::GATEWAY_TIMEOUT);
        }
    };
    let selected_protocol = response
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .cloned();

    let mut switching = Response::new(crate::app_server::empty_body());
    *switching.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = switching.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    let accept = derive_accept_key(key.as_bytes());
    if let Ok(accept) = HeaderValue::from_str(&accept) {
        headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    }
    if let Some(protocol) = selected_protocol {
        headers.insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
    }

    // The upgrade has to be claimed before the 101 leaves this function.
    let upgrade = hyper::upgrade::on(&mut request);
    let shutdown = state.shutdown_receiver();
    state.debug(format!(
        "app_server: websocket {pathname} -> 101 (host upgraded)"
    ));
    tokio::spawn(async move {
        bridge(upgrade, host, shutdown).await;
    });
    switching
}

/// Relays frames between the upgraded renderer socket and the Host socket.
async fn bridge(
    upgrade: hyper::upgrade::OnUpgrade,
    host: HostSocket,
    mut shutdown: watch::Receiver<bool>,
) {
    let Ok(upgraded) = upgrade.await else {
        return;
    };
    let client = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
    let (client_sink, client_stream) = client.split();
    let (host_sink, host_stream) = host.split();
    let to_host = pump(client_stream, host_sink);
    let to_client = pump(host_stream, client_sink);
    tokio::select! {
        () = to_host => {}
        () = to_client => {}
        _ = shutdown.changed() => {}
    }
}

/// Copies every frame from `source` to `sink` until a close or a failure.
async fn pump<S, K>(mut source: S, mut sink: K)
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    K: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    while let Some(message) = source.next().await {
        let Ok(message) = message else { return };
        let closing = matches!(message, Message::Close(_));
        if sink.send(message).await.is_err() {
            return;
        }
        if closing {
            return;
        }
    }
    // The peer vanished without a close frame; close the other side politely.
    let _ = sink.send(Message::Close(None)).await;
}
