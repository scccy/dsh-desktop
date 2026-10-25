# Preload 桥契约（`window.dshDesktop`）

Tauri 版的 Electron preload 等价物。基线是官方 `apps/desktop/src/preload-app.ts`
（+ `preload-platform.ts` / `preload-theme.ts` / `ipc.ts`）。

- 脚本：`src-tauri/bridge/preload.js`（纯 JS，无构建，`node --check` 通过）
- Rust：`src-tauri/src/bridge.rs`（命令 / 事件 / `BridgeState`）
- 注入：content webview 的 `initialization_script`（主 frame，页面前执行）
- **状态（2026-10-25）**：已接线并在 macOS 真机验收（13 injections、`data-platform=darwin`、无 `Desktop keyboard bridge unavailable`、对话流式回复）。

## 接线现状（已落地）

```rust
// main.rs：state、命令、注入脚本
.manage(bridge::BridgeState::new(data_dir.clone()))
// invoke_handler 里逐条注册（与 shell 命令合并；也可用 bridge_handler!()）
bridge::desktop_boot, bridge::desktop_console, …, bridge::desktop_browser_release

// main.rs 建 content webview 时：
.initialization_script(bridge::bridge_script(Some(bridge::APP_SCHEME_ORIGIN)))

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
`desktop_platform_close`, `desktop_browser_acquire`, `desktop_browser_release`。

事件（Rust `emit` → preload `listen`，名字里的 `:` `/` 是 Tauri 允许字符）：

| 事件 | payload | 触发方 |
|---|---|---|
| `dsh-desktop://shortcuts-input` | `DesktopShortcutInput` | 菜单/键盘持有者（`bridge::notify_shortcut_input`；P3 接原生输入） |
| `dsh-desktop://shortcuts-changed` | `ShortcutConfigSnapshot` | `desktop_shortcuts_edit` |
| `dsh-desktop://updates-presentation` | `DesktopUpdatePresentation` | `desktop_updates_open` |
| `dsh-desktop://window-fullscreen` | `bool` | `main.rs` 在窗口尺寸/全屏变化时 emit（已接线） |
| `dsh-desktop://enter-workspace` | `null` | `main.rs` 在 `dsh-app` 文档加载完成后 emit（已接线） |
| `dsh-desktop://browser-open-requested` | `{ lease, url }` | P3 |
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
| `.fileDrag.prepare/cancel/start/release` | `DesktopFileDrag` | —（JS 内安全 stub） | **P3**：`prepare→undefined`、`start→false`、其余 no-op |
| `.browser.acquire/release/onOpenRequested` | `DesktopBrowserBridge` | `desktop_browser_*` | **P3**，且默认**不暴露**（见差异 2） |
| `dshOnboarding.hasApiKey()` | `Promise<boolean>` | `desktop_onboarding_api_key` | stub（恒 `false`，P3 接 Host 凭据） |
| `dshOnboarding.setActive(a)` | `void`（原为 send） | `desktop_onboarding_active`（最小窗口 960×600） | implemented |
| `__DSH_DIRECTORY_PICKER__.pick()` | `Promise<string\|null>` | `desktop_directory_pick`（plugin-dialog） | implemented |
| `__DSH_HOST_PATHS__.pathFor(file)` | `string`（同步） | — 恒 `''` | stub（见差异 3） |
| `dshPlatform.open/setBounds/close` | `Promise<…>` | `desktop_platform_*` | **P3**（记录日志后 resolve，不挂死 overlay） |
| `__DSH_LOCALE__.read()` | `Promise<{languages,preference}>` | `desktop_locale_bootstrap`（sys-locale） | implemented |
| `__DSH_LOCALE__.onChange(l)` | `void`（原为 send） | `desktop_locale_changed`（落 `locale-preference.json`） | implemented |
| `html[data-platform]` | preload 同步打标 | UA 兜底 + `desktop_platform` 修正 | implemented |
| `html[data-fullscreen]` | `window-fullscreen` IPC | `window-fullscreen` 事件 | implemented（窗口侧已 emit） |
| native theme | 观察 `data-ds-theme-source` → `nativeTheme.themeSource` | `desktop_native_theme_set` → `Window::set_theme` | implemented |
| `enter-workspace` | 聚焦 body | `enter-workspace` 事件 | implemented |
| 动态流量改写 | Electron 用 `protocol.handle` 代理 | `fetch` / `XMLHttpRequest` → `desktop_app_base` | implemented（见差异 4） |
| `__DSH_BRIDGE_ESCAPE__.nativeFetch/nativeXhr/appBase` | 无（Electron preload 不暴露） | — | implemented（诊断 / 自救通道） |
| 页面诊断 | 无对应（Electron 有 inspector） | `console.warn/error`、`error`、`unhandledrejection` → `desktop_console` | implemented（release 无 inspector 时的排障通道） |

验证脚本（都不起 GUI）：

```bash
node --check src-tauri/bridge/preload.js
node src-tauri/bridge/rewrite.test.cjs   # 纯函数 + shim 不变量
node src-tauri/bridge/shim.test.cjs     # fetch / XHR / 延迟基址 行为（DOM 桩）
cargo test -p dsh-desktop bridge::       # BootPayload / snapshot / 文档读写
```

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
   - **`EventSource` 不包装**：相对 URL 不会被重写，只有 hmr / documentpreview 用它
     （开发期），需要时等 P3 补绝对回环 URL 或补包装。
5. **同步能力**：Electron 有同步 getter，Tauri 只能 `invoke`；除 `pathFor` 与
   `data-platform` 兜底外，所有成员都是异步。
6. `desktop_shortcuts_edit` 不做服务端冲突/保留键校验（客户端注册表已算
   `conflicts`/`issue`），只做 JSON 形状校验、revision 乐观并发与原子写入。
