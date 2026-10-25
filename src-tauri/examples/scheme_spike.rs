//! Throwaway probe kept as evidence for ADR-006; not part of the app.
#![allow(clippy::collapsible_if, clippy::type_complexity)]

//! Spike: what can a Tauri / WKWebView custom scheme (`dsh-app://`) actually do?
//!
//! Run with `cargo run -p dsh-desktop --example scheme_spike` (or
//! `cargo run --manifest-path src-tauri/Cargo.toml --example scheme_spike`).
//! The window loads `dsh-app://app/`; the inlined page runs five probes and
//! reports them back. Everything is recorded in JSON under `/tmp/dsh-desktop/`.
//!
//! `SPIKE_MODE=sync` registers the scheme with the *synchronous*
//! `register_uri_scheme_protocol` instead of the asynchronous one, so the two
//! response mechanisms can be compared back to back. The sync run writes
//! `/tmp/dsh-desktop/scheme-spike-sync.json`.
//!
//! The example deliberately uses only the dependencies the `dsh-desktop`
//! package already has.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tauri::http::{Request, Response, StatusCode, header};

const SCHEME: &str = "dsh-app";
const PAGE_HTML: &str = include_str!("scheme_spike_page.html");
const OUT_DIR: &str = "/tmp/dsh-desktop";
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Page + prototype init-script shim for `SPIKE_MODE=shim`.
const SHIM_PAGE_HTML: &str = include_str!("shim_spike_page.html");
const SHIM_INIT_JS: &str = include_str!("shim_init.js");

// ---------------------------------------------------------------------------
// shared state
// ---------------------------------------------------------------------------

struct Shared {
    mode: String,
    out_path: String,
    /// Requests that arrived on the loopback TCP server.
    loopback_log: Mutex<Vec<Value>>,
    /// Events observed inside the custom scheme handler (chunk write times, WS
    /// handshakes, beacon chunks, ...).
    scheme_log: Mutex<Vec<Value>>,
    /// Body of the page's `POST /report` (if CORS let it through).
    report_post: Mutex<Option<String>>,
    /// `/__report/<i>/<total>/<b64url>` beacons sent through the scheme handler.
    beacon_slots: Mutex<Vec<Option<String>>>,
    finished: AtomicBool,
}

impl Shared {
    fn new(mode: &str) -> Self {
        let out_path = match mode {
            "async" => format!("{OUT_DIR}/scheme-spike.json"),
            "shim" => format!("{OUT_DIR}/shim-e2e.json"),
            other => format!("{OUT_DIR}/scheme-spike-{other}.json"),
        };
        Self {
            mode: mode.to_string(),
            out_path,
            loopback_log: Mutex::new(Vec::new()),
            scheme_log: Mutex::new(Vec::new()),
            report_post: Mutex::new(None),
            beacon_slots: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
        }
    }

    fn log_loopback(&self, value: Value) {
        if let Ok(mut guard) = self.loopback_log.lock() {
            guard.push(value);
        }
    }

    fn log_scheme(&self, value: Value) {
        if let Ok(mut guard) = self.scheme_log.lock() {
            guard.push(value);
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Writes the spike result once and exits the process.
fn finish(shared: &Arc<Shared>, source: &str, payload: Option<String>, exit_code: i32) -> bool {
    if shared.finished.swap(true, Ordering::SeqCst) {
        return false;
    }
    let report = payload.as_deref().and_then(|raw| {
        serde_json::from_str::<Value>(raw)
            .ok()
            .or_else(|| Some(Value::String(raw.to_string())))
    });
    let loopback_log = shared
        .loopback_log
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let scheme_log = shared
        .scheme_log
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let report_post = shared
        .report_post
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();

    let result = json!({
        "spike": "wkwebview-custom-scheme",
        "mode": shared.mode,
        "source": source,
        "generated_at_unix_ms": now_unix_ms(),
        "report_raw_len": payload.as_ref().map(|p| p.len()),
        "report": report,
        "report_post_body": report_post,
        "scheme_handler_log": scheme_log,
        "loopback_request_log": loopback_log,
    });
    let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());
    let _ = std::fs::create_dir_all(OUT_DIR);
    let _ = std::fs::write(&shared.out_path, &text);

    println!(
        "\n[spike] ===== result written to {} ({} bytes, source={source}) =====",
        shared.out_path,
        text.len()
    );
    if let Some(Value::Object(map)) = result.get("report").cloned() {
        if let Some(Value::Object(steps)) = map.get("steps") {
            for (name, step) in steps {
                let ok = step.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let dur = step
                    .get("duration_ms")
                    .and_then(Value::as_f64)
                    .unwrap_or(-1.0);
                let err = step
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                println!(
                    "[spike]   {:<28} {:>4}  {:>8.1}ms  {}",
                    name,
                    if ok { "ok" } else { "FAIL" },
                    dur,
                    err
                );
            }
        } else if let Some(fatal) = map.get("fatal").and_then(Value::as_str) {
            println!("[spike]   page fatal: {fatal}");
        }
    }
    println!("[spike] exiting in 500ms");
    std::thread::sleep(Duration::from_millis(500));
    std::process::exit(exit_code);
}

// ---------------------------------------------------------------------------
// small hashes / encodings (no extra crates)
// ---------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Length + head/tail hex + two 32-bit checksums. The JS side computes the very
/// same numbers, so equality proves the body arrived whole and unmodified.
fn digest(bytes: &[u8]) -> Value {
    let mut sum: u32 = 0;
    let mut mix: u32 = 2166136261; // FNV-1a 32
    for &b in bytes {
        sum = sum.wrapping_add(b as u32);
        mix = (mix ^ b as u32).wrapping_mul(16777619);
    }
    let head = &bytes[..bytes.len().min(32)];
    let tail = &bytes[bytes.len().saturating_sub(32)..];
    json!({
        "len": bytes.len(),
        "head_hex": hex(head),
        "tail_hex": hex(tail),
        "sum": sum,
        "mix": mix,
    })
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = if i < 20 {
                ((b & c) | ((!b) & d), 0x5A827999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9EBA1)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1BBCDC)
            } else {
                (b ^ c ^ d, 0xCA62C1D6)
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for i in 0..5 {
        out[4 * i..4 * i + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for ch in input.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// loopback HTTP server (thread per connection)
// ---------------------------------------------------------------------------

fn start_loopback(shared: Arc<Shared>) -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::Builder::new()
        .name("spike-loopback".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let shared = shared.clone();
                std::thread::spawn(move || {
                    let _ = handle_conn(stream, shared);
                });
            }
        })?;
    Ok(port)
}

fn read_request(
    reader: &mut BufReader<TcpStream>,
) -> std::io::Result<(String, String, HashMap<String, String>, Vec<u8>)> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "empty request",
        ));
    }
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut body = Vec::new();
    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        let mut buf = vec![0u8; len];
        let mut read = 0;
        while read < len {
            match reader.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => break,
            }
        }
        buf.truncate(read);
        body = buf;
    }
    Ok((method, target, headers, body))
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn cors_echo(origin: &Option<String>) -> Vec<(String, String)> {
    match origin {
        Some(o) => vec![
            ("Access-Control-Allow-Origin".to_string(), o.clone()),
            (
                "Access-Control-Allow-Credentials".to_string(),
                "true".to_string(),
            ),
            ("Vary".to_string(), "Origin".to_string()),
        ],
        None => vec![("Access-Control-Allow-Origin".to_string(), "*".to_string())],
    }
}

fn handle_conn(mut stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let (method, target, headers, body) = read_request(&mut reader)?;
    let path = target.split('?').next().unwrap_or("/").to_string();
    let origin = headers.get("origin").cloned();

    shared.log_loopback(json!({
        "method": method,
        "path": path,
        "origin": origin.clone().unwrap_or_else(|| "<absent>".to_string()),
        "cookie": headers.get("cookie").cloned().unwrap_or_else(|| "<absent>".to_string()),
        "referer": headers.get("referer").cloned().unwrap_or_else(|| "<absent>".to_string()),
        "user_agent": headers.get("user-agent").cloned().unwrap_or_else(|| "<absent>".to_string()),
        "access_control_request_headers": headers.get("access-control-request-headers").cloned(),
        "x_spike": headers.get("x-spike").cloned(),
        "access_control_request_method": headers.get("access-control-request-method").cloned(),
        "content_type": headers.get("content-type").cloned(),
        "content_length": headers.get("content-length").cloned(),
        "body_len": body.len(),
        "body_digest": if body.is_empty() { Value::Null } else { digest(&body) },
    }));

    if method == "OPTIONS" {
        let mut h = cors_echo(&origin);
        h.push((
            "Access-Control-Allow-Methods".to_string(),
            "GET, POST, OPTIONS".to_string(),
        ));
        if let Some(req_headers) = headers.get("access-control-request-headers") {
            h.push((
                "Access-Control-Allow-Headers".to_string(),
                req_headers.clone(),
            ));
        }
        h.push(("Access-Control-Max-Age".to_string(), "600".to_string()));
        h.push(("Content-Type".to_string(), "text/plain".to_string()));
        return write_response(&mut stream, 204, "No Content", &h, b"");
    }

    match path.as_str() {
        "/post" => {
            let payload = digest(&body);
            let json_text = json!({
                "received": "post",
                "origin": origin.clone().unwrap_or_else(|| "<absent>".to_string()),
                "digest": payload,
            })
            .to_string();
            let mut h = cors_echo(&origin);
            h.push(("Content-Type".to_string(), "application/json".to_string()));
            write_response(&mut stream, 200, "OK", &h, json_text.as_bytes())
        }
        // ---- shim end-to-end endpooints (fake host surface) ----------------
        "/plugins/x.json" => {
            let json_text = json!({
                "path": "/plugins/x.json",
                "served_by": "fake-loopback-host",
                "origin_seen": origin.clone().unwrap_or_else(|| "<absent>".to_string()),
                "ua": headers.get("user-agent").cloned().unwrap_or_default(),
            })
            .to_string();
            let mut h = cors_echo(&origin);
            h.push(("Content-Type".to_string(), "application/json".to_string()));
            write_response(&mut stream, 200, "OK", &h, json_text.as_bytes())
        }
        "/echo-json" => {
            let text = String::from_utf8_lossy(&body).to_string();
            let json_text = json!({
                "path": "/echo-json",
                "method": method,
                "origin_seen": origin.clone().unwrap_or_else(|| "<absent>".to_string()),
                "content_type": headers.get("content-type").cloned().unwrap_or_else(|| "<absent>".to_string()),
                "x_spike": headers.get("x-spike").cloned().unwrap_or_else(|| "<absent>".to_string()),
                "body_len": body.len(),
                "body_mix": digest(&body)["mix"].clone(),
                "body_text": if text.len() <= 120 { text.clone() } else { text.chars().take(120).collect::<String>() },
            })
            .to_string();
            let mut h = cors_echo(&origin);
            h.push(("Content-Type".to_string(), "application/json".to_string()));
            write_response(&mut stream, 200, "OK", &h, json_text.as_bytes())
        }
        "/upload" => {
            let content_type = headers.get("content-type").cloned().unwrap_or_default();
            let boundary = content_type
                .split("boundary=")
                .nth(1)
                .map(|b| b.trim_matches('"').to_string())
                .unwrap_or_default();
            let file_part = multipart_part(&body, &boundary, "file");
            let meta_part = multipart_part(&body, &boundary, "meta");
            let file_digest = file_part.as_deref().map(digest);
            let json_text = json!({
                "path": "/upload",
                "origin_seen": origin.clone().unwrap_or_else(|| "<absent>".to_string()),
                "content_type": content_type,
                "content_length": headers.get("content-length").cloned().unwrap_or_default(),
                "body_len": body.len(),
                "file_len": file_digest.as_ref().and_then(|d| d["len"].as_u64()),
                "file_mix": file_digest.as_ref().and_then(|d| d["mix"].as_u64()),
                "file_head_hex": file_digest.as_ref().map(|d| d["head_hex"].clone()),
                "meta": meta_part.map(|p| String::from_utf8_lossy(&p).to_string()),
            })
            .to_string();
            let mut h = cors_echo(&origin);
            h.push(("Content-Type".to_string(), "application/json".to_string()));
            write_response(&mut stream, 200, "OK", &h, json_text.as_bytes())
        }
        "/cors/none" => {
            let mut h = vec![("Content-Type".to_string(), "text/plain".to_string())];
            h.push((
                "X-Note".to_string(),
                "no-cors-headers-on-purpose".to_string(),
            ));
            write_response(
                &mut stream,
                200,
                "OK",
                &h,
                format!(
                    "no-cors-headers ok; server saw origin={}",
                    origin.clone().unwrap_or_else(|| "<absent>".to_string())
                )
                .as_bytes(),
            )
        }
        "/cors/echo" => {
            let json_text = json!({
                "path": "/cors/echo",
                "origin": origin.clone().unwrap_or_else(|| "<absent>".to_string()),
                "cookie": headers.get("cookie").cloned().unwrap_or_else(|| "<absent>".to_string()),
            })
            .to_string();
            let mut h = cors_echo(&origin);
            h.push(("Content-Type".to_string(), "application/json".to_string()));
            h.push((
                "Set-Cookie".to_string(),
                "httpcookie=1; Path=/; SameSite=Lax".to_string(),
            ));
            write_response(&mut stream, 200, "OK", &h, json_text.as_bytes())
        }
        "/cors/star" => {
            let json_text = json!({
                "path": "/cors/star",
                "origin": origin.clone().unwrap_or_else(|| "<absent>".to_string()),
            })
            .to_string();
            let h = vec![
                ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ];
            write_response(&mut stream, 200, "OK", &h, json_text.as_bytes())
        }
        "/sse" => sse_http(&mut stream, &shared),
        "/ws" => websocket(&mut stream, &mut reader, &headers, &target, &shared),
        "/report" => {
            let text = String::from_utf8_lossy(&body).to_string();
            if let Ok(mut g) = shared.report_post.lock() {
                *g = Some(text);
            }
            let mut h = cors_echo(&origin);
            h.push(("Content-Type".to_string(), "application/json".to_string()));
            write_response(
                &mut stream,
                200,
                "OK",
                &h,
                format!("{{\"accepted\":true,\"body_len\":{}}}", body.len()).as_bytes(),
            )
        }
        _ => {
            let h = vec![("Content-Type".to_string(), "text/plain".to_string())];
            write_response(&mut stream, 404, "Not Found", &h, b"not found")
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extracts one `multipart/form-data` field body by field name.
fn multipart_part(body: &[u8], boundary: &str, name: &str) -> Option<Vec<u8>> {
    if boundary.is_empty() {
        return None;
    }
    let needle = format!("name=\"{name}\"");
    let start = find_bytes(body, needle.as_bytes())?;
    let rest = &body[start..];
    let header_end = find_bytes(rest, b"\r\n\r\n")? + 4;
    let data = &rest[header_end..];
    let terminator = format!("\r\n--{boundary}");
    let end = find_bytes(data, terminator.as_bytes())?;
    Some(data[..end].to_vec())
}

/// 3 SSE events, 1s apart, `Transfer-Encoding: chunked`.
fn sse_http(stream: &mut TcpStream, shared: &Arc<Shared>) -> std::io::Result<()> {
    let started = Instant::now();
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Transfer-Encoding: chunked\r\n\
                Access-Control-Allow-Origin: *\r\n\
                Connection: close\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    for n in 1..=3u32 {
        std::thread::sleep(Duration::from_millis(1000));
        let write_ms = started.elapsed().as_millis() as u64;
        let payload = format!("data: {{\"n\":{n},\"write_ms\":{write_ms}}}\n\n");
        stream.write_all(format!("{:x}\r\n", payload.len()).as_bytes())?;
        stream.write_all(payload.as_bytes())?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
        shared.log_loopback(json!({
            "event": "sse_http_write",
            "n": n,
            "write_ms": write_ms,
        }));
    }
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

fn read_frame<R: Read>(reader: &mut R) -> std::io::Result<Option<(u8, bool, Vec<u8>)>> {
    let mut head = [0u8; 2];
    if reader.read(&mut head[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut head[1..])?;
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let mut len = (head[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0u8; 2];
        reader.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        let mut b = [0u8; 8];
        reader.read_exact(&mut b)?;
        len = u64::from_be_bytes(b) as usize;
    }
    let mut mask = [0u8; 4];
    if masked {
        reader.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload)?;
    }
    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    Ok(Some((opcode, fin, payload)))
}

fn websocket(
    stream: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    headers: &HashMap<String, String>,
    target: &str,
    shared: &Arc<Shared>,
) -> std::io::Result<()> {
    let key = headers
        .get("sec-websocket-key")
        .cloned()
        .unwrap_or_default();
    let accept = base64(&sha1(format!("{key}{WS_GUID}").as_bytes()));
    shared.log_scheme(json!({
        "event": "ws_handshake",
        "target": target,
        "origin": headers.get("origin").cloned().unwrap_or_else(|| "<absent>".to_string()),
        "cookie": headers.get("cookie").cloned().unwrap_or_else(|| "<absent>".to_string()),
        "user_agent": headers.get("user-agent").cloned().unwrap_or_else(|| "<absent>".to_string()),
        "sec_websocket_key": key,
        "sec_websocket_accept_sent": accept,
    }));
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    stream.set_read_timeout(Some(Duration::from_secs(8))).ok();

    let mut exchanged = 0;
    loop {
        match read_frame(reader) {
            Ok(Some((opcode, _fin, payload))) => {
                let text = String::from_utf8_lossy(&payload).to_string();
                shared.log_scheme(json!({
                    "event": "ws_frame_in",
                    "opcode": opcode,
                    "len": payload.len(),
                    "text_prefix": text.chars().take(120).collect::<String>(),
                }));
                if opcode == 1 {
                    let pong = b"pong";
                    let mut out = vec![0x81u8, pong.len() as u8];
                    out.extend_from_slice(pong);
                    stream.write_all(&out)?;
                    stream.flush()?;
                    shared.log_scheme(json!({
                        "event": "ws_frame_out",
                        "opcode": 1,
                        "text": "pong",
                    }));
                    exchanged += 1;
                } else if opcode == 8 {
                    shared.log_scheme(json!({ "event": "ws_close_frame_from_page" }));
                    break;
                }
            }
            Ok(None) => break,
            Err(err) => {
                shared.log_scheme(json!({ "event": "ws_read_end", "error": err.to_string() }));
                break;
            }
        }
        if exchanged >= 1 {
            std::thread::sleep(Duration::from_millis(300));
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// custom scheme handler
// ---------------------------------------------------------------------------

fn plain(status: StatusCode, mime: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap_or_else(|_| Response::new(Vec::new()))
}

fn scheme_sse_body(shared: &Arc<Shared>, threaded: bool) -> Vec<u8> {
    let started = Instant::now();
    let mut body = String::new();
    for n in 1..=3u32 {
        std::thread::sleep(Duration::from_millis(1000));
        let write_ms = started.elapsed().as_millis() as u64;
        body.push_str(&format!("data: {{\"n\":{n},\"write_ms\":{write_ms}}}\n\n"));
        shared.log_scheme(json!({
            "event": "scheme_sse_chunk",
            "n": n,
            "write_ms": write_ms,
            "produced_in_spawned_thread": threaded,
        }));
        println!("[spike] scheme handler produced SSE chunk {n} at {write_ms}ms");
    }
    body.into_bytes()
}

/// `/__report/<index>/<total>/<base64url chunk>` – the reliable reporting
/// channel: same-origin with the page, so CORS/loopback restrictions do not
/// apply.
fn handle_beacon(rest: &str, shared: &Arc<Shared>) -> Response<Vec<u8>> {
    let mut it = rest.splitn(3, '/');
    let index = it.next().unwrap_or("");
    let total = it.next().unwrap_or("");
    let chunk = it.next().unwrap_or("");
    let (Ok(index), Ok(total)) = (index.parse::<usize>(), total.parse::<usize>()) else {
        return plain(
            StatusCode::BAD_REQUEST,
            "text/plain",
            b"bad beacon".to_vec(),
        );
    };
    let mut guard = match shared.beacon_slots.lock() {
        Ok(g) => g,
        Err(_) => {
            return plain(
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain",
                b"poisoned".to_vec(),
            );
        }
    };
    if guard.len() != total {
        *guard = vec![None; total];
    }
    if let Some(slot) = guard.get_mut(index) {
        *slot = Some(chunk.to_string());
    }
    let complete = guard.iter().all(|slot| slot.is_some());
    let joined = if complete {
        Some(
            guard
                .iter()
                .filter_map(|s| s.clone())
                .collect::<Vec<String>>()
                .join(""),
        )
    } else {
        None
    };
    drop(guard);
    if let Some(b64) = joined {
        println!(
            "[spike] beacon complete ({total} chunks, {} b64 chars)",
            b64.len()
        );
        let shared = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let bytes = b64url_decode(&b64).unwrap_or_default();
            match String::from_utf8(bytes) {
                Ok(text) => {
                    finish(&shared, "beacon", Some(text), 0);
                }
                Err(err) => {
                    eprintln!("[spike] beacon payload is not utf-8: {err}");
                    finish(&shared, "beacon-invalid-utf8", None, 4);
                }
            }
        });
    }
    plain(
        StatusCode::OK,
        "text/plain",
        format!("beacon {index}/{total}").into_bytes(),
    )
}

fn scheme_route(
    shared: &Arc<Shared>,
    port: u16,
    req: &Request<Vec<u8>>,
    threaded: bool,
) -> Response<Vec<u8>> {
    let path = req.uri().path().to_string();
    let started = Instant::now();
    shared.log_scheme(json!({
        "event": "scheme_request",
        "method": req.method().to_string(),
        "uri": req.uri().to_string(),
        "path": path,
        "body_len": req.body().len(),
        "query": req.uri().query(),
        "threaded": threaded,
    }));

    if path == "/" || path == "/index.html" {
        let template = if shared.mode == "shim" {
            SHIM_PAGE_HTML
        } else {
            PAGE_HTML
        };
        let html = template
            .replace("__PORT__", &port.to_string())
            .replace("__MODE__", &shared.mode);
        return plain(
            StatusCode::OK,
            "text/html; charset=utf-8",
            html.into_bytes(),
        );
    }
    if path == "/scheme-ping" {
        return plain(
            StatusCode::OK,
            "application/json",
            json!({"pong": true, "path": path}).to_string().into_bytes(),
        );
    }
    if path == "/__echo-body" {
        let payload = json!({
            "received": "scheme-handler",
            "method": req.method().to_string(),
            "uri": req.uri().to_string(),
            "content_type": req
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<absent>"),
            "digest": digest(req.body()),
            "handler_threaded": threaded,
        });
        return plain(
            StatusCode::OK,
            "application/json",
            payload.to_string().into_bytes(),
        );
    }
    if path == "/scheme-sse" || path == "/scheme-sse-sync" {
        let body = scheme_sse_body(shared, threaded);
        let elapsed = started.elapsed().as_millis() as u64;
        shared.log_scheme(json!({
            "event": "scheme_sse_respond",
            "path": path,
            "handler_elapsed_ms": elapsed,
            "body_bytes": body.len(),
            "threaded": threaded,
        }));
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-store")
            .header("X-Handler-Elapsed-Ms", elapsed.to_string())
            .body(body)
            .unwrap_or_else(|_| Response::new(Vec::new()));
    }
    if let Some(rest) = path.strip_prefix("/__report/") {
        return handle_beacon(rest, shared);
    }
    plain(
        StatusCode::NOT_FOUND,
        "text/plain",
        format!("scheme handler: no route for {path}").into_bytes(),
    )
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

fn main() {
    let mode = std::env::var("SPIKE_MODE").unwrap_or_else(|_| "async".to_string());
    let mode = match mode.as_str() {
        "sync" => "sync".to_string(),
        "shim" => "shim".to_string(),
        _ => "async".to_string(),
    };
    let shared = Arc::new(Shared::new(&mode));

    let port = start_loopback(shared.clone()).expect("loopback listener");
    println!(
        "[spike] mode={mode} loopback=http://127.0.0.1:{port} out={}",
        shared.out_path
    );

    // Watchdog: never hang the terminal forever.
    {
        let watchdog = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(150));
            if !watchdog.finished.load(Ordering::SeqCst) {
                eprintln!("[spike] watchdog: page never reported, writing partial result");
                finish(&watchdog, "watchdog-timeout", None, 2);
            }
        });
    }

    let scheme_shared = shared.clone();
    let builder = tauri::Builder::default();

    let builder = if mode == "sync" {
        builder.register_uri_scheme_protocol(SCHEME, move |_ctx, req| {
            let started = Instant::now();
            scheme_shared.log_scheme(json!({
                "event": "sync_handler_enter",
                "path": req.uri().path(),
                "thread": "main",
            }));
            let resp = scheme_route(&scheme_shared, port, &req, false);
            scheme_shared.log_scheme(json!({
                "event": "sync_handler_return",
                "path": req.uri().path(),
                "blocked_main_thread_ms": started.elapsed().as_millis() as u64,
            }));
            resp
        })
    } else {
        builder.register_asynchronous_uri_scheme_protocol(SCHEME, move |_ctx, req, responder| {
            scheme_shared.log_scheme(json!({
                "event": "async_handler_enter",
                "path": req.uri().path(),
                "spawning_thread": true,
            }));
            let shared = scheme_shared.clone();
            std::thread::spawn(move || {
                let resp = scheme_route(&shared, port, &req, true);
                responder.respond(resp);
            });
        })
    };

    let setup_shared = shared.clone();
    let mode_for_setup = mode.clone();
    let result = builder
        .setup(move |app| {
            let url: tauri::Url = format!("{SCHEME}://app/").parse()?;
            let mut window =
                tauri::WebviewWindowBuilder::new(app, "spike", tauri::WebviewUrl::External(url))
                    .title("WKWebView dsh-app scheme spike")
                    .inner_size(900.0, 700.0);
            // `shim` mode installs the prototype rewrite shim the same way the
            // real bridge will: as a Tauri initialization script (document start).
            if mode_for_setup == "shim" {
                let script = SHIM_INIT_JS.replace("__PORT__", &port.to_string());
                println!(
                    "[spike] installing init script shim ({} bytes)",
                    script.len()
                );
                window = window.initialization_script(script);
            }
            let window = window.build()?;

            // Fallback channel: the page can park the payload in `location.hash`.
            let shared = setup_shared.clone();
            std::thread::spawn(move || {
                for _ in 0..600 {
                    std::thread::sleep(Duration::from_millis(500));
                    if shared.finished.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Ok(url) = window.url() {
                        if let Some(fragment) = url.fragment() {
                            if let Some(encoded) = fragment.strip_prefix("r=") {
                                if let Some(bytes) = b64url_decode(encoded) {
                                    if let Ok(text) = String::from_utf8(bytes) {
                                        println!(
                                            "[spike] hash channel delivered {} bytes",
                                            text.len()
                                        );
                                        finish(&shared, "location.hash", Some(text), 0);
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!());

    if let Err(err) = result {
        eprintln!("[spike] tauri runtime error: {err}");
    }
    if !shared.finished.load(Ordering::SeqCst) {
        finish(&shared, "run-returned", None, 3);
    }
}
