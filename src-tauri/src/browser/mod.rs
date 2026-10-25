//! 内嵌浏览器（Sidebar Browser）：Tauri 子 webview + `<webview>` 兼容层。
//!
//! 官方实现见 `apps/desktop/src/browser-guests.ts`（主进程侧的租约 / 隔离 / 导航策略）、
//! `preload-browser.ts`（页面侧 lease 级操作）与
//! `packages/client/ui-sidebar-browser`（页面自己 `document.createElement('webview')`）。
//! Tauri 没有 WebContentsView，等价物是窗口的**子 webview**（`Window::add_child`，
//! 需要 tauri 的 `unstable` feature，已在 `src-tauri/Cargo.toml` 打开）。
//!
//! ```text
//! 页面：dsh-app://app/ ── <webview data-sidebar-browser-frame> ──┐
//!   │  bridge/webview-shim.js（自定义元素 + DOM 观测）             │
//!   │      invoke: acquire / guest_attach / guest_bounds / …      │
//!   ▼                                                            ▼
//! Rust：browser::{lease, partition, policy, history, guest} ── 子 webview（独立持久分区）
//! ```
//!
//! # 组成
//!
//! | 模块 | 职责 | 官方对应 |
//! |---|---|---|
//! | [`lease`] | workspace 预约 → lease；lease 归属校验；整体回收 | `DesktopBrowserGuests` |
//! | [`partition`] | workspace key → 稳定持久分区（macOS data store / Win-Linux data 目录） | `session.fromPartition` |
//! | [`policy`] | 主 frame 导航白名单、新窗口白名单、受权 origin 拦截 | `allowedNavigation` / `setWindowOpenHandler` |
//! | [`history`] | 线性历史（`canGoBack` / `goBack` 的近似） | Chromium history |
//! | [`guest`] | 建子 webview、状态快照、几何同步、下载/权限/新窗口钩子 | `will-attach-webview` 等 |
//! | [`commands`] | `desktop_browser_*` 命令 | `DESKTOP_IPC.browser*` |
//!
//! # 接线清单（由 `main.rs` / `bridge.rs` 落地，本模块不自注册）
//!
//! ```ignore
//! // 1) main.rs：状态
//! .manage(browser::BrowserState::new(data_dir.clone()))
//!
//! // 2) main.rs：content webview 的 initialization_script，顺序不可换
//! //    with_caps 必须在 preload 之前，with_shim 跟在后面，两者共用 config.dsh.browser_guests
//! let builder = browser::with_caps(builder, config.dsh.browser_guests);
//! let builder = builder.initialization_script(bridge::bridge_script(Some(bridge::APP_SCHEME_ORIGIN)));
//! let builder = browser::with_shim(builder, config.dsh.browser_guests);
//!
//! // 3) main.rs：invoke_handler（bridge_handler! 之外追加）
//! //    路径必须是 `browser::commands::…`：`generate_handler!` 引用的是
//! //    `#[tauri::command]` 在同模块生成的隐藏项 `__cmd__<name>`，普通 `pub use`
//! //    不会把它带出来（所以 `commands` 是 `pub mod`）。
//! browser::commands::desktop_browser_guest_attach,
//! browser::commands::desktop_browser_guest_bounds,
//! browser::commands::desktop_browser_guest_navigate,
//! browser::commands::desktop_browser_guest_command,
//! browser::commands::desktop_browser_guest_state,
//! browser::commands::desktop_browser_guest_close
//!
//! // 4) bridge.rs：两条官方 IPC 改成一行转发
//! crate::browser::acquire(&webview, &state, &bridge.extra_origins(), &workspace)
//! crate::browser::release(&webview, &state, &bridge.extra_origins(), &lease)
//!
//! // 5) main.rs：窗口关闭 / 退出 / content 主 frame 导航时回收全部 guest（官方 releaseAll）
//! browser::shutdown(&app);
//! ```
//!
//! `browser_guests_enabled` 是**同一个开关**：只开 caps 会让 Web UI 切到「没有实现的
//! `<webview>`」分支（白屏），只开 shim 则白费（页面走 iframe 分支）。默认建议 `false`，
//! 真机验收通过后再打开。

// `generate_handler!` refers to the hidden item `__cmd__<name>` that
// `#[tauri::command]` emits next to the function, and a plain `pub use` does not
// re-export it — so the command module itself is public and `main.rs` registers
// `browser::commands::…`.
pub mod commands;
mod guest;
mod history;
mod lease;
mod partition;
mod policy;

// 只导出接线真正用到的四项：其余（guest 命令、事件常量、`GuestBounds`/`GuestSnapshot`、
// `GuestHistory`、`Reservation`、partition 与 policy 的函数）都只在 crate 内部使用，
// 由各自的模块自己持有，避免再制造一轮「导出即未使用」的告警。
pub use commands::{acquire, release};
pub use lease::BrowserState;

use tauri::webview::WebviewBuilder;
use tauri::{AppHandle, Manager, Runtime};

/// 能力开关脚本：告诉 preload 可以挂载 `dshDesktop.browser` 与 `<webview>` 兼容层。
///
/// 官方没有这一步（Electron 的 preload 只看 `protocolVersion`）；本仓库需要它，因为
/// Web UI 只要看到 `dshDesktop.browser` 就会**放弃**自己可用的 iframe 浏览器面板，
/// 而 `browser-guests` 的实现 / 真机验收还没完成时不能这么干。
pub const CAPS_JS: &str =
    "globalThis.__DSH_BRIDGE_CAPS__ = { browserGuests: true, protocolVersion: 1 };";

/// 页面侧 `<webview>` 兼容层（`src-tauri/bridge/webview-shim.js`，纯 JS，无构建）。
pub const SHIM_JS: &str = include_str!("../../bridge/webview-shim.js");

/// 能力开关脚本，`enabled` 为假时不注入。
pub fn caps_js(enabled: bool) -> Option<&'static str> {
    enabled.then_some(CAPS_JS)
}

/// `<webview>` 兼容层脚本，`enabled` 为假时不注入（与 [`caps_js`] 必须同生同死）。
pub fn shim_js(enabled: bool) -> Option<&'static str> {
    enabled.then_some(SHIM_JS)
}

/// 按开关给 builder 追加能力开关脚本；关闭时原样返回（接线处可以写成一行）。
pub fn with_caps<R: Runtime>(builder: WebviewBuilder<R>, enabled: bool) -> WebviewBuilder<R> {
    match caps_js(enabled) {
        Some(script) => builder.initialization_script(script),
        None => builder,
    }
}

/// 按开关给 builder 追加 `<webview>` 兼容层；关闭时原样返回。
pub fn with_shim<R: Runtime>(builder: WebviewBuilder<R>, enabled: bool) -> WebviewBuilder<R> {
    match shim_js(enabled) {
        Some(script) => builder.initialization_script(script),
        None => builder,
    }
}

/// 回收全部 guest：窗口关闭、退出、`content` 主 frame 换文档时调用。
///
/// 官方对应 `did-start-navigation` / `render-process-gone` / `destroyed` 的 `releaseAll`。
/// @returns 被关闭的 guest 数量。
pub fn shutdown(app: &AppHandle) -> usize {
    let Some(state) = app.try_state::<BrowserState>() else {
        return 0;
    };
    let guests = state.close_all();
    let count = guests.len();
    for guest in guests {
        guest.close();
    }
    count
}
