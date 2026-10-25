# Spike：WKWebView 自定义 scheme（`dsh-app://app`）能力实证


> **状态（2026-10-25）**：本 spike 的结论已采纳为 [ADR-006](../architecture.md#adr-006-渲染层-origin-拆分) 并落地到 P1/P2（`scheme.rs` + `app_server` + `bridge/preload.js` shim）。本文保留为原始证据。

> 目的：决定 `docs/desktop-parity.md` §6.1/§6.2 里 P1 架构的走向 —— 主窗口页面到底从
> `dsh-app://app` 加载（复刻 Electron 的 `protocol.handle`），还是退回回环
> `http://127.0.0.1`。官方的做法是把 `dsh-app://app/` 做成**流式代理**（转发请求体、返回
> 流式 `Response`、处理 CORS/cookie）；本 spike 只回答一件事：**Tauri/WKWebView 能不能等价**。
>
> 结论先行：**不能等价，必须按接口类型拆分**（静态走 scheme，动态流量走回环）。
> 第二轮补测（§5）进一步验证了拆分后的**实现路径**：用 init script 把 `dsh-app://app/*`
> 重写到回环 origin 的 shim 策略端到端可行（流式未被破坏，无 `fetch`/XHR/上传/Request 重建缺口）。

## 1. 环境与复现

| 项 | 值 |
| --- | --- |
| 机器 | macOS 27.0.1 (26A434)，arm64 |
| 工具链 | rustc 1.97.1 (8bab26f4f 2026-07-14) |
| Tauri / wry | `tauri 2.12.4` / `wry 0.57.0`（`Cargo.lock` 锁定值） |
| 页面 User-Agent | `Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)` |
| 真实 GUI 会话 | 是（`cargo run --example` 起窗口，900×700，加载 `dsh-app://app/`） |

复现命令（只新增 `src-tauri/examples/`，未改动 `main.rs` / `crates/` / `Cargo.toml` 依赖）：

```bash
# 异步 handler（register_asynchronous_uri_scheme_protocol）
cargo run -p dsh-desktop --example scheme_spike
# 同步 handler 对照组（register_uri_scheme_protocol）
SPIKE_MODE=sync cargo run -p dsh-desktop --example scheme_spike
# init script shim 端到端（§5）
SPIKE_MODE=shim cargo run -p dsh-desktop --example scheme_spike
```

产物：

- 程序：[scheme_spike.rs](../../src-tauri/examples/scheme_spike.rs)、[scheme_spike_page.html](../../src-tauri/examples/scheme_spike_page.html)
- 异步结果：`/tmp/dsh-desktop/scheme-spike.json`（71,506 B，单轮约 11.2 s 跑完 15 个探测）
- 同步结果：`/tmp/dsh-desktop/scheme-spike-sync.json`（84,411 B）
- shim 结果：`/tmp/dsh-desktop/shim-e2e.json`（64,726 B，10 个探测）
- shim 原型与页面：[shim_init.js](../../src-tauri/examples/shim_init.js)（作为 Tauri `initialization_script` 注入）、[shim_spike_page.html](../../src-tauri/examples/shim_spike_page.html)

页面里所有探测都带 try/catch，失败也记录错误原文；结果回传走 `dsh-app://app/__report/<i>/<n>/<b64url>`
（同源，不受 CORS 影响），回环 `POST /report` 只作为 CORS 证据（它自己也过了 preflight）。
完整性校验用的不是 sha256，而是两边独立计算的 `len + 前 32 字节 hex + 后 32 字节 hex + 加和(mod 2³²) + FNV-1a(mod 2³²)`，
对"截断/改写"判定足够。

## 2. 结论表

| # | 问题 | 结论 | 关键证据（原始值） | 对 P1 架构的影响 |
| --- | --- | --- | --- | --- |
| 1 | `dsh-app://app` 页面是 secure context 吗？ | **YES**（但对麦克风没用） | `location.origin = dsh-app://app`；`isSecureContext = true`；`typeof crypto.randomUUID = "function"`；`typeof crypto.subtle = "object"`；`crossOriginIsolated = false` | P3 的 secure context 前提成立；但见 §4 风险项：`navigator.mediaDevices` 根本不存在 |
| 2 | POST 16KB 请求体能完整到达吗？ | **YES**（三个方向都完整） | fetch：`server.len = 16384`、`head_hex/tail_hex/sum/mix` 五个字段全等（`all_match: true`）；XHR：同样 `all_match: true`；`dsh-app://app/__echo-body`：handler 收到 `body_len: 16384` 且 `all_match: true` | 请求体转发不是问题：页面→回环、页面→scheme handler 都能拿到完整 body |
| 3 | Tauri scheme handler 能流式响应吗？ | **NO**（异步/同步都被整体缓冲） | 异步：handler 内 3 次写入发生在 1003/2006/3011 ms，而页面 `fetch` **3013 ms** 才拿到响应头、body reader 只读到 **1 个 chunk**（`span_ms: 0`）；`EventSource` open@3016 ms、两条消息全部在 3016 ms。对照回环 `/sse`：`header_ms 4`，增量到达 1077/2082/3086 ms（`span_ms 2009`） | **SSE / 流式回复绝不能再走 `dsh-app://`**；页面必须直连回环 app server。这是 P1 拆分的决定性证据 |
| 4 | 从 `dsh-app://app` fetch 回环 http 的 CORS 行为？ | **需要 CORS，且 Origin 头就是 `dsh-app://app`** | 服务端收到的 `Origin: dsh-app://app`（所有回环请求一致）；不带 CORS 头 → 页面 `TypeError: Load failed`（服务端日志显示请求其实到达了）；`Access-Control-Allow-Origin: dsh-app://app` + `Access-Control-Allow-Credentials: true` → 200 `type:"cors"`，JSON body 可读；`Access-Control-Allow-Origin: *` 对无凭据 GET 也通过 | app server 必须回显 `Origin`（不能写死 `*`，因为要带凭据），并正确处理 `OPTIONS` preflight（`POST application/json` 会先发 preflight，实测到达：`ACRM: POST`、`ACRH: content-type`） |
| 5 | 从 `dsh-app://app` 能连 `ws://127.0.0.1` 吗？cookie 呢？ | **WS 能连、Origin 同样是 `dsh-app://app`；但页面 cookie 完全不可用** | WS：`open_ms: 12`，手写握手服务端记录 `origin: dsh-app://app`、`cookie: <absent>`，页面发 `ping-from-page`、12 ms 收到 `pong`；没有 mixed-content 报错。Cookie：`document.cookie = "spikecookie=1; path=/"` 不抛错但读回 `""`；回环响应 `Set-Cookie: httpcookie=1` 后第二次 `credentials:"include"` 请求服务端仍收到 `cookie: <absent>` | WS 可以从自定义 scheme 页面直接连回环（§6.2 的 Rust WS 代理端点可行）；但 cookie 只能由 Rust 主进程持有并在握手时注入，webview cookie jar 不可用 —— 与 §6.2 既有结论一致 |

### 补充：Tauri 同步 handler 对照组

| 模式 | handler 登记方式 | 页面观察 | 主线程 |
| --- | --- | --- | --- |
| async | `register_asynchronous_uri_scheme_protocol` | `/scheme-sse`：`header_ms 3013`，1 个 chunk，`span_ms 0`；EventSource open@3016 ms，全部消息同时到达 | handler 立刻把请求交给 `std::thread`，不阻塞事件循环 |
| sync | `register_uri_scheme_protocol` | 同上：`header_ms 3004`，1 个 chunk，`span_ms 0`；EventSource open@3003 ms，消息同一毫秒到达 | **阻塞主线程约 3 s**（两次 `/scheme-sse` 分别 `blocked_main_thread_ms: 3003` 与 `3002`） |

即：**同步 handler 除了冻结 UI 之外，流式收益为零**；异步 handler 只是把"等 3 秒"挪到了后台线程，
响应仍然是"全量字节一次 `didReceiveData`"。源码侧与结论完全一致：

- `wry-0.57.0/src/wkwebview/class/url_scheme_handler.rs::start_task`：`task.didReceiveResponse(&response)` → **一次** `task.didReceiveData(&data)` → `task.didFinish()`，`data` 来自 `sent_response.into_body()`（完整 `Cow<[u8]>`）。
- `tauri-2.12.4/src/app.rs::UriSchemeResponder::respond`：签名就是"收一个完整的 `http::Response<Cow<'static,[u8]>>`"，没有流式通道。
- 同一文件里 wry 会读 `request.HTTPBody` 与 `HTTPBodyStream` 拼成 `Vec<u8>`，所以 Q2 的"请求体完整"是可信的。

## 3. 原始证据（摘自 `/tmp/dsh-desktop/scheme-spike.json`）

### 3.1 Q1 secure context（`steps.env.value`）

```json
{
  "location_href": "dsh-app://app/", "location_protocol": "dsh-app:",
  "location_host": "app", "location_origin": "dsh-app://app",
  "document_baseURI": "dsh-app://app/",
  "rel_dot_x": "dsh-app://app/x",
  "rel_abs_assets": "dsh-app://app/assets/app.js",
  "rel_parent_z": "dsh-app://app/z",
  "isSecureContext": true, "crossOriginIsolated": false,
  "typeof_crypto_randomUUID": "function",
  "typeof_crypto_subtle": "object",
  "typeof_crypto_getRandomValues": "function",
  "typeof_navigator_mediaDevices": "undefined",
  "typeof_mediaDevices_getUserMedia": "undefined",
  "typeof_navigator_clipboard": "object",
  "typeof_clipboard_writeText": "function",
  "typeof_navigator_serviceWorker": "object",
  "cookie_read": "", "permissions_microphone": "state=prompt"
}
```

- **相对 URL 解析正常**：`new URL('./x', document.baseURI)` → `dsh-app://app/x`，`/assets/app.js` → `dsh-app://app/assets/app.js`。前端打包产物用相对路径可以正常加载（这是 P1 静态 dist 的硬前提，已确认）。
- `navigator.clipboard.writeText` 存在（P3 的复制/粘贴可能可走 web API，但 WKWebView 可能要求用户手势，未在本 spike 验证）。
- `crossOriginIsolated = false`（没有 COOP/COEP），意味着 `SharedArrayBuffer` 不可用 —— 若 P3 需要多线程 WASM 要另开方案。

### 3.2 Q2 POST 请求体完整性

```text
步骤 post_fetch_http   → status 200 / type cors / content_type application/json / duration 12 ms
  local  : {len:16384, head_hex:"3639623839643466…393335393933613", tail_hex:"6362643735346530…36386436393264",
            sum:1146586, mix:3669116839}
  server : {len:16384, head_hex:"3639623839643466…393335393933613", tail_hex:"6362643735346530…36386436393264",
            sum:1146586, mix:3669116839}
  match  : {len:true, head_hex:true, tail_hex:true, sum:true, mix:true}  all_match: true
步骤 post_xhr_http     → 同一个 body，all_match: true（status 200）
步骤 post_body_scheme_handler → dsh-app://app/__echo-body
  server_json = {"received":"scheme-handler","method":"POST","uri":"dsh-app://app/__echo-body",
                 "content_type":"text/plain;charset=UTF-8","digest":{…同上…},"handler_threaded":true}
  digest 与页面本地全等，all_match: true
```

回环服务端日志（`loopback_request_log`）里三次 POST 都是 `body_len: 16384`、`content_length: "16384"`。

### 3.3 Q3 流式（同一个 3 段 SSE，三种通道）

```text
# A. 回环 http + fetch（对照组，Transfer-Encoding: chunked + 每 1s 一条）
sse_http_fetch_stream: header_ms 4，arrivals t_ms = 1077 / 2082 / 3086（span_ms 2009）
sse_http_eventsource : open@9ms，data@1014ms、data@2019ms（span_ms 2010）

# B. dsh-app://app/scheme-sse + async handler（handler 内部 1003/2006/3011 ms 写入）
sse_scheme_fetch_stream: header_ms 3013，arrival_count 1，
  唯一 chunk = "data: {\"n\":1,\"write_ms\":1003}\n\ndata: {\"n\":2,\"write_ms\":2006}\n\ndata: {\"n\":3,\"write_ms\":3011}\n\n"
  span_ms 0
sse_scheme_eventsource : open@3016ms，n=1@3016ms，n=2@3016ms（span_ms 0）

# C. 同步 handler 同一 URL：header_ms 3004，arrival_count 1，span_ms 0；handler 阻塞主线程 3003 ms
```

`scheme_handler_log` 佐证写入时刻与响应时刻分离：

```json
{"event":"scheme_sse_chunk","n":1,"write_ms":1003,"produced_in_spawned_thread":true}
{"event":"scheme_sse_chunk","n":2,"write_ms":2006,"produced_in_spawned_thread":true}
{"event":"scheme_sse_chunk","n":3,"write_ms":3011,"produced_in_spawned_thread":true}
{"event":"scheme_sse_respond","path":"/scheme-sse","handler_elapsed_ms":3011,"body_bytes":93,"threaded":true}
```

同步模式：

```json
{"event":"sync_handler_enter","path":"/scheme-sse","thread":"main"}
{"event":"scheme_sse_respond","handler_elapsed_ms":3003,"body_bytes":93,"threaded":false}
{"event":"sync_handler_return","path":"/scheme-sse","blocked_main_thread_ms":3003}
```

### 3.4 Q4 CORS

```text
# 回环服务端看到的 Origin（每个请求都一样）
{"method":"GET","path":"/cors/none","origin":"dsh-app://app","cookie":"<absent>"}
{"method":"POST","path":"/post","origin":"dsh-app://app","body_len":16384}
{"method":"OPTIONS","path":"/report","origin":"dsh-app://app",
 "access_control_request_method":"POST","access_control_request_headers":"content-type"}
{"method":"POST","path":"/report","origin":"dsh-app://app","body_len":8428}
```

```json
// 不带任何 Access-Control-* 头
"cors_no_headers": {"reached":"fetch threw","error":"TypeError: Load failed"}     // 服务端日志里这条 GET 是到达的
// 回显 Access-Control-Allow-Origin: dsh-app://app + Allow-Credentials: true
"cors_echo_origin": {"status_1":200,"status_2":200,
  "body_1":{"origin":"dsh-app://app","cookie":"<absent>","path":"/cors/echo"}}
// Access-Control-Allow-Origin: *
"cors_star": {"status":200,"text":"{\"origin\":\"dsh-app://app\",\"path\":\"/cors/star\"}"}
// 页面自己 POST JSON 报告（触发 preflight，仍成功）
"report_post_loopback": {"reached":true,"status":200,"payload_bytes":8428,
  "response_prefix":"{\"accepted\":true,\"body_len\":8428}"}
```

要点：

1. `Origin` 是**规范化的** `dsh-app://app`，不是 `null` —— 服务端可以精确回显。
2. 没有 CORS 头时浏览器**确实拦截**（`TypeError: Load failed`），但请求已经打到服务端；所以 app server 不能"靠 127.0.0.1 就默认同源"。
3. `POST` + `Content-Type: application/json` 会先发 `OPTIONS` preflight，app server 必须实现（`Allow-Methods` / `Allow-Headers` 回显）。
4. `credentials: "include"` 可以工作，但**cookie 不会被写入/回传**（见 3.5）；`Access-Control-Allow-Origin: *` 与凭据不兼容，必须回显具体 origin。

### 3.5 Q5 WebSocket 与 cookie

```json
"websocket_loopback": {"url":"ws://127.0.0.1:55754/ws","open_ms":12,"send_ok":"sent",
                       "message":"pong","message_ms":12}
"cookie_document": {"after_set":"","set_ok":true,"cookie_enabled":{"ok":true,"value":true}}
"cors_echo_origin": {"body_1":{"cookie":"<absent>"},"body_2":{"cookie":"<absent>"}}
```

```json
// 手写 WS 握手服务端记录
{"event":"ws_handshake","target":"/ws","origin":"dsh-app://app","cookie":"<absent>",
 "user_agent":"Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)"}
{"event":"ws_frame_in","len":14,"opcode":1,"text_prefix":"ping-from-page"}
{"event":"ws_frame_out","opcode":1,"text":"pong"}
```

要点：

1. `ws://127.0.0.1` 从 `dsh-app://app` 页面**可以建连**，虽然页面是 secure context —— 实测没有 mixed-content 拦截（WKWebView 不把自定义 scheme 当 https 处理）。这对 §6.2 的「Rust WS 代理端点由 `streamBaseUrl` 给出」是好消息。
2. 握手 `Origin` 同样是 `dsh-app://app`，Rust 代理可以据此判断来源。
3. **cookie 完全不可用**：`document.cookie` 写入静默丢弃（不抛错、读回空串），回环 `Set-Cookie` 也不落到 webview jar（第二次 `credentials:"include"` 请求仍无 Cookie）。这与 §6.2 的结论一致：cookie 只能由主进程持有 + 在 WS/HTTP 代理侧注入，**不能作为产品方案依赖 webview cookie**。

## 4. 风险项（新增，需决策）

### R1 · P3 麦克风：WKWebView 不实现 Media Capture

- 实测 `typeof navigator.mediaDevices === "undefined"`（`getUserMedia`、`enumerateDevices` 同样 undefined），而同一页面 `isSecureContext === true`、`navigator.permissions.query({name:"microphone"})` 返回 `state=prompt`。
- 即：**这不是 secure context 或权限问题，而是 WKWebView（macOS）根本不提供 Media Capture API**。
- 影响：P3 的"麦克风"不能复用 web 前端现有的 `getUserMedia` 逻辑，必须 **Rust 侧原生采集（AVCaptureSession）**，再通过 IPC/回环把音频（或转写结果）交给页面。工作量和风险等级都要按"新增原生能力"重估，不能按"打开一个 web API"估。
- 关联：同一环境里 `navigator.clipboard.writeText` 存在，说明"部分 web API 有、Media Capture 没有"是 WKWebView 的能力缺口，不是一个统一开关。

### R2 · 大响应/上传的内存模型（未实测，先记录）

scheme handler 的响应必须是完整 `Vec<u8>`，且 wry 会 `NSData::from_vec(content)` 一次性交给 WebKit。静态资源（dist 里的 js/css/字体）没问题，但**任何"大文件/长连接"都不能走 scheme**：这条约束与 Q3 结论互锁，落地时按"接口类型白名单"分流即可。本 spike 未测 >10MB 单响应的峰值内存，若 P1 需要（例如更新包下载走页面），需补测。

### R3 · 未覆盖项

- `EventSource` 自动重连、`fetch` 的 `ReadableStream` 上传（已在 §5.2-A 实测：WKWebView **不支持**）、`sendBeacon`、`<video>` 播放本地 scheme 资源、Service Worker 注册等未在本 spike 覆盖。
- ~~本 spike 的页面是独立 `examples` 二进制，未验证 init script 重写 `fetch`/`XHR` 的端到端效果。~~ 已在 §5 完成（`SPIKE_MODE=shim`，用真实的 `initialization_script` 注入）。仍未覆盖：`Request` 的 `body` 为 `Blob`/`ReadableStream` 时的缓冲成本、`CacheStorage`/`Service Worker` 拦截路径、`sendBeacon` 的重写。

## 5. Shim 原型端到端（init script 重写 fetch/XHR → 回环 origin）

目的：验证 P1 集成前最后一块硬证据 —— 页面仍从 `dsh-app://app` 加载，但**在文档开始前注入的
init script 里把 `dsh-app://app/*` 重写到 `http://127.0.0.1:<port>/*`**，让所有动态流量实际打在
回环 app server 上（CORS 回显 + preflight）。这决定「要不要为『shim 不可行』保留回退开关」。

实现方式刻意贴近产品路径：shim 不是写死在 HTML 里，而是用
`WebviewWindowBuilder::initialization_script(...)` 注入（`SPIKE_MODE=shim`），
即 bridge 未来用的同一个钩子。实测注入时机正确：shim 里读到的 `location.href` 已经是
`dsh-app://app/`（`installed_at_href`），`location.protocol = "dsh-app:"`，
页面里 `fetch_replaced: true`、`xhr_replaced: true`。

### 5.1 结论表

| # | 验证项 | 期望 | 实测 | 结论 |
| --- | --- | --- | --- | --- |
| 1 | `fetch('dsh-app://app/sse')` 经 shim 重写后是否仍增量 | span ≥ 1500 ms | 响应头 **44 ms**；3 段到达 t = **1017 / 2017 / 3022 ms**，`span_ms 2005`；`status 200`、`type "cors"`、`content-type text/event-stream` | **PASS** —— shim + CORS 后端到端**不缓冲** |
| 2 | `EventSource(base + '/sse')`（不 shim，直接指回环） | 增量 | open@5 ms，3 条消息 t = **1008 / 2011 / 3014 ms**，`span_ms 3009` | **PASS** —— EventSource **不需要包装**，只要 URL 是绝对回环地址 |
| 3 | `fetch` POST JSON（带自定义头） | 有 preflight、body 完整 | 服务端记录 `OPTIONS` → `ACRM: POST`、`ACRH: "content-type,x-spike"`、`Origin: dsh-app://app`；随后 `POST` body **632 B**，`body_mix` 与页面本地 FNV 相等（`all_match: true`），`x_spike: "fetch-json"` | **PASS** |
| 4 | XHR 上传 FormData + 1 MB Blob | 服务端长度/校验和对、onprogress ≥ 1 | 服务端 `body_len 1,048,889`、file part `len 1,048,576`、`mix 2606538181` 与页面本地全等（`all_match: true`）；`progress_events 9`、`progress_loaded_max 1,048,889`；另有 `OPTIONS /upload`（`ACRH` 为空） | **PASS**（且证明上传是真流式：分 9 次 progress） |
| 5 | `fetch(new Request('dsh-app://app/plugins/x.json'))` | 重写并返回 200 | `strategy: direct-rebuild-no-body`，服务端收到 `GET /plugins/x.json`、`origin dsh-app://app`、`ok_rewritten: true` | **PASS** |
| 6 | `fetch(new Request(url, {method:'POST', body:'hi'}))` | body 不丢 | `strategy: clone+arrayBuffer`、`body_buffered_bytes: 2`，服务端 `body_text: "hi"`、`body_len: 2` | **PASS**（但必须缓冲，见 5.2-A） |
| 7 | Request 的 `mode` 陷阱 | `same-origin`/`no-cors` 需改 `cors` | 两次都 `mode_overridden: "same-origin->cors"` / `"no-cors->cors"`，返回 200 `type "cors"` 且 body 可读（`no_cors.body_readable: true`） | **PASS**（不改则必然失败，见 5.2-C） |

### 5.2 实测到的坑（bridge-dev 必须照抄的约束）

**A. `Request.body` 不能直接移交给新 Request —— 而且失败后 body 就废了（不能"先试后回退"）。**
最小复现（页面里 `fetch_request_stream_body_trap` 步骤，结果在 `shim-e2e.json`）：

```json
{"source_url":"dsh-app://app/echo-json","rewritten":"http://127.0.0.1:56605/echo-json",
 "rebuild_constructed":true,"stream_body_accepted":false,
 "fetch_error":"NotSupportedError: ReadableStream uploading is not supported",
 "source_body_used":true,"source_clone_after":"throws: TypeError: Body is disturbed or locked"}
```

即：`new Request(target, {body: src.body, duplex:"half"})` **构造成功**，但 `fetch()` 抛
`NotSupportedError`；并且此时原 Request 的 `bodyUsed` 已变 `true`、`clone()` 直接抛
`TypeError: Body is disturbed or locked` —— **先试流式再回退是行不通的**。
正确做法：`input.clone()` 后 `arrayBuffer()` 再重建（本 spike 的做法）。
代价：整个 body 进内存（`body_buffered_bytes` 可观测）。1 MB 级没问题；如果前端有
>几十 MB 的 `fetch(new Request(...))` 上传，应改走 XHR（XHR 上传是真流式，见第 4 项）。

**B. XHR 必须在 `send()` 时才调原生 `open()`，因此 `setRequestHeader` 必须缓冲 + 回放。**
`open()` 推迟到 `send()`（这样相对 URL 才解析得准），而原生 XHR 在 `UNSENT` 状态调用
`setRequestHeader` 会抛 `InvalidStateError: The object's state must be OPENED`。
本 spike 的做法：缓存 `[name, value]` 队列，在 `send()` 里 `nativeOpen(...)` 之后逐条
`nativeSetRequestHeader(...)`。实测服务端收到 `x_spike: "xhr-header"`、
`headers_replayed: 1`，且 `xhr.open/send` 挂在实例上可写（`instance_writable: true`）。

**C. `Request.mode` 必须从 `same-origin` / `no-cors` / `navigate` 改成 `cors`。**
重写后的 URL 是跨源的；沿用 `same-origin` 会被 WebKit 直接拒；`no-cors` 虽然能发，但响应是
opaque（读不到 body），shim 之后前端所有 `r.json()` 都会坏。实测改成 `cors` 后两种都返回
200 且 body 可读。

**D. 上传也会走 preflight，且 preflight 会被 `Max-Age` 缓存。**
`multipart/form-data` + Blob 的 XHR 上传，服务端仍收到一条 `OPTIONS /upload`
（`ACRM: POST`、`ACRH` 为空）—— app server 不能假设"简单请求就没有 OPTIONS"。
另外 3 个"非简单请求"在日志里只出现 2 次 `OPTIONS`：前一次 preflight 响应回了
`Access-Control-Max-Age: 600`，后续 GET + `X-Spike` 命中了 preflight 缓存。含义：
**preflight 响应里必须把该端点允许的 method/header 列全**（本 spike 回显
`Access-Control-Request-Headers`），否则会被缓存"记住"而拦掉后续请求。

**E. 上报/逃生通道要在 shim 之前留存原生引用。**
页面自己 `fetch('dsh-app://app/__report/...')`（走 scheme handler、同源）在本轮被 shim
重写成了回环路径，所以页面改用 `window.__dshShim.nativeFetch` 才能把结果送回 Rust。
bridge 同理：init script 里 `const nativeFetch = window.fetch.bind(window)` 之后必须把
`__dshShim.nativeFetch` 之类的逃生口暴露出来（本 spike 的 `window.__dshShim` 就是原型）。

**F. EventSource 前提是"绝对回环 URL"。**
不包装 EventSource 的代价：前端（或 `streamBaseUrl`）必须给出绝对地址。
若某处仍用相对 URL `new EventSource('/stream')`，它不会被重写，会打到
`dsh-app://app/stream`（scheme handler）。落地时把 EventSource 的 URL 源统一走
`dshBase` 拼接即可，不要再"顺手也包一层 EventSource"。

**G. 本轮不测（记录结论即可）：WS 的自定义头。**
浏览器 `WebSocket` 构造函数没有 header 参数，页面**无法**给升级请求加自定义头；
上一轮实测握手只有 `Origin: dsh-app://app`、`Cookie: <absent>`。
→ **cookie/授权只能由 Rust 代理在升级请求上添加**（§6.2 的既有结论，本轮无新增证据）。

### 5.3 一句话结论

**shim 策略端到端可行**（fetch/XHR/上传/EventSource/Request 重建全部通过，且流式未被破坏），
只有 A–F 六条必须遵守的实现约束；因此不必为"shim 不可行"保留**回退开关**，但建议保留一个
**诊断开关**（环境变量强制页面直接加载回环 origin），成本≈0 且能在一行代码内定位 shim 相关回归。

## 6. 建议（一句话）

**页面 origin 保留 `dsh-app://app`（静态资源、模块脚本、favicon/manifest 走 scheme handler），但所有动态流量（fetch/XHR/EventSource/WS）一律走回环 app server，由它回显 `Origin: dsh-app://app` 并处理 preflight，cookie 由 Rust 主进程持有并注入** —— 即 §6.1 的"按接口类型拆分"，而不是把 `dsh-app://` 当 Electron `protocol.handle` 的等价代理；拆分后的重写层（init script shim）已在本文件 §5 端到端验证通过，不需要为"shim 不可行"保留回退开关。
