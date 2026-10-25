# Preload 桥契约（`window.dshDesktop`）

Tauri 版的 Electron preload 等价物。基线是官方 `apps/desktop/src/preload-app.ts`
（+ `preload-platform.ts` / `preload-theme.ts` / `ipc.ts`）。

- 脚本（三个，纯 JS、无构建、`node --check` 通过）：
  - `src-tauri/bridge/preload.js` —— 主 preload 等价物（`window.dshDesktop` 等）；
  - `src-tauri/bridge/webview-shim.js` —— `<webview>` 兼容层（内嵌浏览器，仅在 `browserGuests` 打开时注入）；
  - `src-tauri/bridge/media-shim.js` —— EventSource 重写 + 原生麦克风合成 MediaStream（总是注入）。
- Rust：`src-tauri/src/bridge.rs`（命令 / 事件 / `BridgeState`）、`src-tauri/src/browser/*`、`src-tauri/src/media/*`
- 注入：content webview 的 `initialization_script`（主 frame，页面前执行）
- **状态（2026-10-25）**：P1/P2 已在 macOS 真机验收（13 injections、`data-platform=darwin`、无 `Desktop keyboard bridge unavailable`、对话流式回复）；P3 的内嵌浏览器与麦克风已接线，`browserGuests` 默认关闭。

## 接线现状（已落地）

```rust
// main.rs：state、命令、注入脚本
.manage(bridge::BridgeState::new(data_dir.clone()))
// invoke_handler 里逐条注册（与 shell 命令合并；也可用 bridge_handler!()）
bridge::desktop_boot, bridge::desktop_console, …, bridge::desktop_browser_release

// main.rs 建 content webview 时（顺序不可换：caps 必须早于 preload，shim 必须晚于它）：
.initialization_script(browser::with_caps(builder, browser_guests))   // 默认不注入
.initialization_script(bridge::bridge_script(Some(bridge::APP_SCHEME_ORIGIN)))  // preload + media shim
.initialization_script(browser::with_shim(builder, browser_guests))   // 默认不注入

// app_runtime::start() 在 Host 就绪、app server 起来之后：
bridge.set_boot(injections.clone(), app_base.clone());  // app_base = "http://127.0.0.1:<port>"
bridge.set_app_base(app_base);

// main.rs 窗口事件：
bridge::notify_workspace_entered(&app);   // 应用文档加载完成
bridge::notify_fullscreen(&app, fullscreen); // 窗口全屏状态变化

// 页面诊断（release 无 inspector 时把 console/未捕获异常写进滚动日志）：
bridge::desktop_console(level, message)
```

命令（`#[tauri::command]`，均为 content webview + app origin 双校验）：
`desktop_boot`, `desktop_console`, `desktop_boot_failed`, `desktop_device_info`, `desktop_directory_pick`,
`desktop_shortcuts_get`, `desktop_shortcuts_edit`, `desktop_shortcuts_recording`,
`desktop_shortcuts_close_window`, `desktop_updates_status`, `desktop_updates_open`,
`desktop_locale_bootstrap`, `desktop_locale_changed`, `desktop_onboarding_api_key`,
`desktop_onboarding_active`, `desktop_platform`, `desktop_app_base`,
`desktop_native_theme_set`, `desktop_platform_open`, `desktop_platform_bounds`,
`desktop_platform_close`, `desktop_browser_acquire`, `desktop_browser_release`,
`desktop_browser_guest_attach`, `desktop_browser_guest_bounds`, `desktop_browser_guest_navigate`,
`desktop_browser_guest_command`, `desktop_browser_guest_state`, `desktop_browser_guest_close`,
`desktop_microphone_status`, `desktop_microphone_start`, `desktop_microphone_stop`。

事件（Rust `emit` → preload `listen`，名字里的 `:` `/` 是 Tauri 允许字符）：

| 事件 | payload | 触发方 |
|---|---|---|
| `dsh-desktop://shortcuts-input` | `DesktopShortcutInput` | 菜单/键盘持有者（`bridge::notify_shortcut_input`；P3 接原生输入） |
| `dsh-desktop://shortcuts-changed` | `ShortcutConfigSnapshot` | `desktop_shortcuts_edit` |
| `dsh-desktop://updates-presentation` | `DesktopUpdatePresentation` | `desktop_updates_open` |
| `dsh-desktop://window-fullscreen` | `bool` | `main.rs` 在窗口尺寸/全屏变化时 emit（已接线） |
| `dsh-desktop://enter-workspace` | `null` | `main.rs` 在 `dsh-app` 文档加载完成后 emit（已接线） |
| `dsh-desktop://browser-open-requested` | `{ lease, url }` | `browser::guest`（新窗口请求，已接线；仅在 `browserGuests` 打开时页面会订阅） |
| `dsh-desktop://browser-state` | `GuestSnapshot` | `browser::guest`（guest 加载中/标题/URL/历史变化） |
| `dsh-desktop://browser-download-blocked` | `{ lease, url }` | `browser::guest`（对齐官方 `will-download` → preventDefault） |
| `dsh-desktop://microphone-state` | `{ state, reason? }` | `media::macos`（`idle/requesting/starting/live/stopping/failed`） |
| `dsh-desktop://boot-failed` | 失败消息 | `desktop_boot_failed` |

`BootPayload`：`{ "injections": Value[], "streamBaseUrl": "http://127.0.0.1:<port>" }`
（camelCase；前端逐字解构这两个键，见 `dist/assets/index-*.js` 的 `Ms.ready()`）。

## 成员对照表

| 桥成员（JS） | 官方签名 | Tauri 后端 | 状态 |
|---|---|---|---|
| `dshDesktopBoot.ready()` | `Promise<{injections,streamBaseUrl}>` | `desktop_boot` | implemented（真机验收：13 injections） |
| `dshDesktopBoot.failed(m)` | `Promise<void>` | `desktop_boot_failed` → 日志 + 事件 + 现有 error.html | implemented |
| `dshDesktop.protocolVersion` | `1` | — | implemented |
| `.deviceInfo()` | `Promise<string>`（`platform/os/app_arch/cpu/memory_gib`，`; ` 分隔） | `desktop_device_info` | implemented（Windows 无 `os` 字段） |
| `.keyboard.subscribe(fn)` | `() => void` | DOM keydown 合成 + `shortcuts-input` 事件 | implemented（见差异 1） |
| `.keyboard.closeWindow(rev)` | `Promise<void>` | `desktop_shortcuts_close_window`（revision 不符则忽略） | implemented（关窗走 `window.close()`） |
| `.shortcuts.get(defs)` | `Promise<ShortcutConfigSnapshot>` | `desktop_shortcuts_get` | implemented（`~/data_dir/shortcuts.json`） |
| `.shortcuts.edit(edit,rev)` | `Promise<ShortcutSaveResult>` | `desktop_shortcuts_edit`（乐观并发，`stale`） | implemented |
| `.shortcuts.recording(active)` | `Promise<void>` | `desktop_shortcuts_recording` | implemented |
| `.shortcuts.subscribe(fn)` | `() => void` | `shortcuts-changed` 事件 | implemented |
| `.updates.status()` | `Promise<DesktopUpdatePresentation>` | `desktop_updates_status` | implemented（缓存态） |
| `.updates.open()` | `Promise<void>` | `desktop_updates_open`（`npm view` 探针 + 事件流） | implemented（只探测，不下载/安装） |
| `.updates.subscribe(fn)` | `() => void` | `updates-presentation` 事件 | implemented |
| `.fileDrag.prepare/cancel/start/release` | `DesktopFileDrag` | —（JS 内安全 stub） | **永久差异**：Tauri/wry 没有「从 webview 拖出文件」的 API；`prepare→undefined`、`start→false`、其余 no-op（本机 dsh 0.2.1-rc.2 无消费者） |
| `.browser.acquire/release/onOpenRequested` | `DesktopBrowserBridge` | `desktop_browser_acquire` / `desktop_browser_release` → `browser::{acquire,release}` | implemented，但默认**不暴露**：只有 `[dsh] browserGuests = true` 时 shell 才注入 caps 脚本（见差异 2） |
| `dshOnboarding.hasApiKey()` | `Promise<boolean>` | `desktop_onboarding_api_key` | stub（恒 `false`，P3 接 Host 凭据） |
| `dshOnboarding.setActive(a)` | `void`（原为 send） | `desktop_onboarding_active`（最小窗口 960×600） | implemented |
| `__DSH_DIRECTORY_PICKER__.pick()` | `Promise<string\|null>` | `desktop_directory_pick`（plugin-dialog） | implemented |
| `__DSH_HOST_PATHS__.pathFor(file)` | `string`（同步） | — 恒 `''` | stub（见差异 3） |
| `dshPlatform.open/setBounds/close` | `Promise<…>` | `desktop_platform_*` | 仍为「记录日志后 resolve」：官方语义是独立的 platform overlay 窗口，本壳没有对应窗口模型，因此只记录日志、不挂死调用方 |
| `__DSH_LOCALE__.read()` | `Promise<{languages,preference}>` | `desktop_locale_bootstrap`（sys-locale） | implemented |
| `__DSH_LOCALE__.onChange(l)` | `void`（原为 send） | `desktop_locale_changed`（落 `locale-preference.json`） | implemented |
| `html[data-platform]` | preload 同步打标 | UA 兜底 + `desktop_platform` 修正 | implemented |
| `html[data-fullscreen]` | `window-fullscreen` IPC | `window-fullscreen` 事件 | implemented（窗口侧已 emit） |
| native theme | 观察 `data-ds-theme-source` → `nativeTheme.themeSource` | `desktop_native_theme_set` → `Window::set_theme` | implemented |
| `enter-workspace` | 聚焦 body | `enter-workspace` 事件 | implemented |
| 动态流量改写 | Electron 用 `protocol.handle` 代理 | `fetch` / `XMLHttpRequest` / `EventSource` → 回环 app server | implemented（`preload.js` 管 fetch/XHR，`media-shim.js` 管 EventSource，见差异 4） |
| `navigator.mediaDevices.getUserMedia` | Chromium 原生实现 | `desktop_microphone_start`（`Channel` 推 PCM）+ 页面合成 MediaStream | implemented（`media-shim.js`；见「媒体与输入」一节） |
| `__DSH_MEDIA__`（只读诊断） | 无对应 | `desktop_microphone_status` + 内部计数器 | implemented（`{status(), permission(), underrun/dropped 计数}`） |
| `__DSH_BRIDGE_ESCAPE__.nativeFetch/nativeXhr/appBase` | 无（Electron preload 不暴露） | — | implemented（诊断 / 自救通道） |
| 页面诊断 | 无对应（Electron 有 inspector） | `console.warn/error`、`error`、`unhandledrejection` → `desktop_console` | implemented（release 无 inspector 时的排障通道） |

验证脚本（都不起 GUI）：

```bash
node --check src-tauri/bridge/preload.js
node --check src-tauri/bridge/webview-shim.js
node --check src-tauri/bridge/media-shim.js
node src-tauri/bridge/rewrite.test.cjs      # 纯函数 + shim 不变量
node src-tauri/bridge/shim.test.cjs         # fetch / XHR / 延迟基址 行为（DOM 桩）
node src-tauri/bridge/media-shim.test.cjs   # EventSource 重写 + 麦克风合成（vm 假环境）
cargo test -p dsh-desktop bridge::          # BootPayload / snapshot / 文档读写
cargo test -p dsh-desktop browser::         # lease / partition / policy / history
cargo test -p dsh-desktop media::           # 权限状态机 / 重采样 / 分帧 / 会话状态机
```

## 内嵌浏览器（`browserGuests`）

页面侧走官方 `@deepseek-ai/dsh-client-ui-sidebar-browser` 的 `<webview>` 分支：页面
`document.createElement('webview')`，由 `bridge/webview-shim.js` 的自定义元素接住，
再把元素矩形/可见性换算成原生子 webview 的 bounds。

| 命令（全部经 `browser::assert_sender`：label == `content` 且文档 origin 是 app 文档） | 作用 |
|---|---|
| `desktop_browser_acquire(workspace)` | 校验 workspace（非空、≤4096），按 workspace 取分区，发一个 `lease` |
| `desktop_browser_guest_attach(lease, url)` | 建/复用 guest（`async`：`Window::add_child` 要投递到主线程，命令本身不能占住主线程） |
| `desktop_browser_guest_bounds(lease, bounds)` | 同步几何与可见性（0.5 px 抖动阈值，越界/不可见 → `hide()`） |
| `desktop_browser_guest_navigate(lease, url)` / `_command(lease, command)` | 导航与 `goBack`/`goForward`/`reload`/`clearHistory` |
| `desktop_browser_guest_state(lease)` | 同步读快照（`getURL`/`getTitle`/`isLoading`/`canGoBack` 必须同步） |
| `desktop_browser_guest_close(lease)` / `desktop_browser_release(lease)` | 关 guest / 释放 lease（分区保留） |

能力开关：**`[dsh] browserGuests`（默认 `false`）**。为 `true` 时 `main.rs` 会按
`browser::with_caps` → `bridge_script` → `browser::with_shim` 的顺序注入三条脚本：
caps 必须早于 preload（preload 初始化时会同步读 `__DSH_BRIDGE_CAPS__`），
`<webview>` shim 必须晚于 preload（它要用桥的 `invoke`）。**两者必须同生同死**：
只开 caps 会让 Web UI 切到「没有实现的 `<webview>`」分支（白屏），只开 shim 则页面仍走
iframe 分支、白费。

## 媒体与输入

### 麦克风

WKWebView 没有 Media Capture（`navigator.mediaDevices === undefined`），所以
`media-shim.js` 覆盖 `navigator.mediaDevices`，把 `getUserMedia` 换成「让壳采集 +
页面合成 MediaStream」。前端（语音输入插件）零改动。

```text
desktop_microphone_start(channel, options)   # options 用 serde_json::Value 收，逐字段校验
  → channel.send(JSON 头 {type:"open",sampleRate,channelCount,framesPerFrame,format:"f32le",deviceId,groupId,label,…})
  → channel.send(ArrayBuffer: 20 ms 单声道 f32le，默认 3840 B/帧)
desktop_microphone_stop()                    # 页面侧 track.stop() 也会触发（shim 劫持）
desktop_microphone_status()                  # → { authorization, available, devices[] }
dsh-desktop://microphone-state               # { state: idle|requesting|starting|live|stopping|failed, reason? }
```

- 采样率跟页面 `AudioContext({sampleRate})` 走（默认 48 kHz），壳侧出 f32le 单声道，
  页面 `ScriptProcessorNode(1024)` 写入 `MediaStreamAudioDestinationNode`；`PcmRing`
  吸收「20 ms 帧」与「1024 帧块」的时钟差（欠载补静音、积压丢最旧，计数在 `__DSH_MEDIA__`）。
- 首帧头的 `deviceId`/`groupId`/`label` 会写到合成轨上，设备选择器据此回显。
- `track.stop()` 由 shim 劫持：原生轨不会派发 `ended`，shim 补发并顺带停掉原生采集。
- 任何一步失败都只 `console.warn` 并按 `NotAllowedError` 语义拒绝，**不打断页面启动**。

### EventSource

见差异 4：`media-shim.js` 把相对路径与 `dsh-app://app/...` 重写到回环 origin，
只做 URL 与生命周期代理，不接管数据流。

### 目录选择与文件拖放

`__DSH_DIRECTORY_PICKER__.pick()` 与官方同形（`Promise<string | null>`），并补齐
「先 restore + show + focus 再弹」与「同一窗口并发调用共用一次面板」。`fileDrag`
保持 stub（拖出为永久差异，见成员对照表）。

## 已知差异（接受并记录）

1. **键盘输入方向相反**：Electron 由主进程 `before-input-event` 过滤后推给页面；Tauri
   侧 bridge 在页面捕获阶段合成 `DesktopShortcutInput`（`kind:'keyboard'`，带当前
   `revision`）。`dsh-client-shortcuts` 在 desktop runtime 下不再自行 DOM dispatch，
   所以没有这一层快捷键会全哑。两键和弦（`secondCode`）、`iframe`/`webview` 归属判定
   仍按官方形状实现；Rust 侧菜单可直接发 `shortcuts-input` 事件。
2. **`.browser` 默认不暴露**：`dsh-client-ui-sidebar-browser` 只看
   `carrier.protocolVersion === 1 ? carrier.browser : undefined`，暴露一个未实现的
   `browser` 会让它放弃可用的 iframe 回退。P3 落地后用
   `globalThis.__DSH_BRIDGE_CAPS__ = { browserGuests: true }` 打开。
3. **`__DSH_HOST_PATHS__.pathFor` 恒返回 `''`**：Tauri 没有 `webUtils.getPathForFile`，
   拖入/粘贴的 `File` 在页面里没有宿主路径，因此走上传而不是 `@path` 引用。
4. **动态流量改写**：`dsh-app:` scheme handler 的响应会被 WebView 缓冲（spike 实测 SSE
   3s 后一次性到达），所以相对路径与 `dsh-app://app/...` 的 `fetch`/XHR 改写到回环 app
   server；绝对 `http(s)://`、`blob:`、`data:` 原样放行。三条硬限制（spike §5 实测）：
   - `Request` 的 body **不能**移交（`ReadableStream uploading is not supported`，且会让原
     Request 变 `bodyUsed`）→ 实现是 `clone()` + `arrayBuffer()` 重建，原始 Request 保持
     可用以便回退。**因此 `fetch(Request)` 的 body 会进内存；大上传请用 XHR（真流式）。**
   - 改写后变跨源，`mode` 强制为 `cors`（`same-origin`/`navigate` 会失败，`no-cors` 会让
     响应变 opaque），`only-if-cached` 丢弃；`credentials` 保持原值（cookie 由 Rust 侧注入）。
   - 装 shim 前先保存原生引用（`nativeFetch` / `nativeXhr`），并通过只读的
     `__DSH_BRIDGE_ESCAPE__` 暴露给页面自救/诊断，避免诊断请求被自己重写。
   - **WebSocket 不改写**（前端用 `streamBaseUrl`，自定义头由 Rust 代理在升级请求上加）；
   - **`EventSource` 由 `media-shim.js` 重写**（P1 曾判定「不包装」）：相对路径与
     `dsh-app://app/...` 与 fetch/XHR 用**同一套规则**改写到回环 origin；基址未就绪时
     排队、就绪后建连；只代理 `addEventListener`/`onopen`/`onerror`/`close`/`readyState`/`url`，
     不做流处理，重连语义仍归原生实现。
5. **同步能力**：Electron 有同步 getter，Tauri 只能 `invoke`；除 `pathFor` 与
   `data-platform` 兜底外，所有成员都是异步。
6. `desktop_shortcuts_edit` 不做服务端冲突/保留键校验（客户端注册表已算
   `conflicts`/`issue`），只做 JSON 形状校验、revision 乐观并发与原子写入。
