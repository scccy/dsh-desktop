//! `desktop_browser_*` 命令：页面 `<webview>` 兼容层与桥之间的边界。
//!
//! 每条命令都先过 sender 校验（只接受 `content` webview + 应用文档 origin，与
//! `bridge::assert_sender` 同一套规则），再做 lease 归属校验，最后才碰子 webview。
//! 命令名的 wire 契约：
//!
//! | 命令 | 调用方 | 语义 |
//! |---|---|---|
//! | `desktop_browser_acquire` | `dshDesktop.browser.acquire(workspace)` | 发一个 lease + 已批准的分区 |
//! | `desktop_browser_release` | `dshDesktop.browser.release(lease)` | 销毁该 lease 的 guest，分区保留 |
//! | `desktop_browser_guest_attach` | `<webview>` 元素 connectedCallback | 建子 webview |
//! | `desktop_browser_guest_bounds` | 元素几何 / 可见性变化 | 摆位、裁剪、显隐 |
//! | `desktop_browser_guest_navigate` | `element.loadURL()` | 白名单校验后导航 |
//! | `desktop_browser_guest_command` | `goBack` / `goForward` / `reload` / `clearHistory` / focus | 导航命令 |
//! | `desktop_browser_guest_state` | 元素重连时重新对齐 | 取当前快照 |
//! | `desktop_browser_guest_close` | 元素 disconnectedCallback | 销毁子 webview |
//!
//! 前两条与官方 IPC 同名，由 `bridge.rs` 的同名命令转发到 [`acquire`] / [`release`]；
//! 其余六条是本仓库新增的（Electron 里由 `<webview>` 标签自身承担，不需要 IPC）。

use std::sync::Arc;

use serde_json::{Value, json};
use tauri::{AppHandle, Manager, State, Webview};

use crate::bridge::BridgeState;
use crate::state::AppState;
use crate::window::CONTENT_WEBVIEW;

use super::guest::{self, GuestBounds, GuestSnapshot};
use super::lease::{BrowserState, Reservation};
use super::policy::{self, BlockedOrigins};

/// 与 `bridge::assert_sender` 同一套规则：只接受 `content` webview 的应用文档。
///
/// 官方对应 `assertDesktopSender`；guest 自己是远程 origin，Tauri 的 ACL 也会先拒掉它的
/// invoke（capabilities 里没有 `remote` 上下文），这里是第二道防线。
/// 只在 crate 内部使用，不作为对外 API 导出（`browser/mod.rs` 的 `pub use` 只留接线项）。
pub(crate) fn assert_sender(caller: &Webview, extra_origins: &[String]) -> Result<(), String> {
    if caller.label() != CONTENT_WEBVIEW {
        return Err("dsh desktop: rejected IPC from an unowned webview".to_owned());
    }
    let url = caller
        .url()
        .map_err(|error| format!("dsh desktop: the caller URL is unavailable: {error}"))?;
    if !crate::bridge::url_is_app_document(&url, extra_origins) {
        return Err("dsh desktop: rejected IPC from an unowned renderer".to_owned());
    }
    Ok(())
}

/// guest 绝不允许到达的 origin：受权栈里的**每一层**。
///
/// 官方对应 `new DesktopBrowserGuests(() => hostUrl)` 的 `hostUrl()`：`isApplicationHost`
/// 是在**导航策略这一层**把应用的受权 origin 整组挡掉，而不是靠 cookie 认证兜底。
/// 本仓库的受权表面有三处，全部登记：
///
/// 1. 回环 app server（`BridgeState::app_base()`）——动态流量的代理，持有 Host cookie；
/// 2. Desktop Host 自己的 origin（`AppRuntime::host_origin`，即 `dsh web` 的监听地址）——
///    它要求 cookie 认证，但「认证兜底」不是策略，guest 根本不该发出这类请求；
/// 3. 旧两层路径（`--url` / `--port` / `webFallback`）下的 `ConnectionInfo::web_url`——
///    那条路径没有 app server，guest 同样不该碰它。
///
/// 一个都没登记时返回空集合，调用方据此**拒绝开 guest**（见
/// [`desktop_browser_guest_attach`]）：没有受权 origin 意味着栈还没起到三层，
/// guest 的分区与代理行为在这个状态下都没有定义。
fn blocked_origins(app: &AppHandle, bridge: &BridgeState) -> BlockedOrigins {
    let mut blocked = BlockedOrigins::new();
    if let Some(base) = bridge.app_base() {
        blocked.insert(&base);
    }
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let runtime = state.runtime();
        if let Some(runtime) = runtime.app.as_ref() {
            blocked.insert(runtime.host_origin.as_str());
        }
        // 旧两层路径（webFallback / attach）：没有 app server，只有连接目标本身。
        if let Some(connection) = runtime.connection.as_ref() {
            blocked.insert(connection.web_url.as_str());
        }
    }
    blocked
}

/// 把「变了就推状态」的收尾逻辑收在一处。
fn publish(
    app: &AppHandle,
    result: Option<(bool, GuestSnapshot)>,
) -> Result<GuestSnapshot, String> {
    match result {
        Some((changed, snapshot)) => {
            if changed {
                guest::emit_state(app, &snapshot);
            }
            Ok(snapshot)
        }
        None => Err("dsh desktop: unknown browser guest lease".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// 官方 IPC：acquire / release（bridge.rs 的同名命令转发到这里）
// ---------------------------------------------------------------------------

/// `dshDesktop.browser.acquire(workspace)`：官方 `browser-guests.ts::acquire`。
///
/// @returns `{ lease, partition }`，与 `DesktopBrowserReservation` 逐字段一致。
pub fn acquire(
    caller: &Webview,
    state: &BrowserState,
    extra_origins: &[String],
    workspace: &Value,
) -> Result<Value, String> {
    assert_sender(caller, extra_origins)?;
    let reservation = state.reserve(caller.label(), workspace)?;
    guest::log_info(
        caller.app_handle(),
        &reservation.lease,
        &format!("lease reserved ({} active)", state.lease_count()),
    );
    Ok(json!({
        "lease": reservation.lease,
        "partition": reservation.partition,
    }))
}

/// `dshDesktop.browser.release(lease)`：官方 `browser-guests.ts::release`。
///
/// 只销毁该 lease 名下的 guest；分区（workspace 存储）保留。
pub fn release(
    caller: &Webview,
    state: &BrowserState,
    extra_origins: &[String],
    lease: &str,
) -> Result<(), String> {
    assert_sender(caller, extra_origins)?;
    if let Some(guest) = state.release(caller.label(), lease)? {
        guest.close();
        guest::log_info(caller.app_handle(), lease, "guest released");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// guest 生命周期（本仓库新增）
// ---------------------------------------------------------------------------

/// 建一个子 webview。页面在 `<webview>` 进 DOM 时调用。
///
/// 这个命令是 `async` 的：`Window::add_child` 会把建视图的任务投给主线程并等它完成
/// （`tauri-runtime-wry::send_user_message`），同步命令本来就在主线程上跑，
/// 放在异步命令里可以让主线程先空出来跑完投递，也不在 WebKit 的 IPC 回调里重入建视图。
#[tauri::command]
pub async fn desktop_browser_guest_attach(
    caller: Webview,
    app: AppHandle,
    bridge: State<'_, BridgeState>,
    state: State<'_, BrowserState>,
    lease: String,
    partition: String,
) -> Result<GuestSnapshot, String> {
    assert_sender(&caller, &bridge.extra_origins())?;
    state.check_attachment(caller.label(), &lease, &partition)?;
    let window = caller.window();
    let reservation = Reservation {
        lease: lease.clone(),
        partition: partition.clone(),
    };
    let blocked = blocked_origins(&app, &bridge);
    if blocked.is_empty() {
        // 三层栈还没起到「有受权 origin」的状态（Host / app server / attach 目标都没有）：
        // 此时 guest 的分区、代理与导航策略都没有定义，宁可不开浏览器，
        // 也不要在「一个受权 origin 都拦不住」的状态下把子 webview 建出来。
        return Err(
            "dsh desktop: the authenticated stack is not ready; refusing to open a browser guest"
                .to_owned(),
        );
    }
    let webview = guest::create(&app, &window, &state, &reservation, blocked)?;
    let snapshot = state.attach(caller.label(), &lease, &partition, webview)?;
    guest::log_info(&app, &lease, "guest attached");
    Ok(snapshot)
}

/// 同步 `<webview>` 元素的几何与可见性。
#[tauri::command]
pub fn desktop_browser_guest_bounds(
    caller: Webview,
    app: AppHandle,
    bridge: State<'_, BridgeState>,
    state: State<'_, BrowserState>,
    lease: String,
    bounds: GuestBounds,
) -> Result<GuestSnapshot, String> {
    assert_sender(&caller, &bridge.extra_origins())?;
    let webview = state.guest_webview(caller.label(), &lease)?;
    let visible = guest::apply_bounds(&caller, &webview, bounds)?;
    let result = state.with_guest(&lease, |guest| {
        let changed = guest.set_visible(visible);
        (changed, guest.snapshot.clone())
    });
    publish(&app, result)
}

/// 销毁子 webview（元素离开 DOM）。lease 一并回收，`release` 随后幂等成功。
#[tauri::command]
pub fn desktop_browser_guest_close(
    caller: Webview,
    bridge: State<'_, BridgeState>,
    state: State<'_, BrowserState>,
    lease: String,
) -> Result<(), String> {
    assert_sender(&caller, &bridge.extra_origins())?;
    if let Some(guest) = state.release(caller.label(), &lease)? {
        guest.close();
        guest::log_info(caller.app_handle(), &lease, "guest closed");
    }
    Ok(())
}

/// 取当前快照（元素重连、或页面丢过一次事件后重新对齐）。
#[tauri::command]
pub fn desktop_browser_guest_state(
    caller: Webview,
    bridge: State<'_, BridgeState>,
    state: State<'_, BrowserState>,
    lease: String,
) -> Result<GuestSnapshot, String> {
    assert_sender(&caller, &bridge.extra_origins())?;
    state.snapshot(caller.label(), &lease)
}

// ---------------------------------------------------------------------------
// 导航
// ---------------------------------------------------------------------------

/// `element.loadURL(url)`：白名单校验后导航到目标。
#[tauri::command]
pub fn desktop_browser_guest_navigate(
    caller: Webview,
    app: AppHandle,
    bridge: State<'_, BridgeState>,
    state: State<'_, BrowserState>,
    lease: String,
    url: String,
) -> Result<GuestSnapshot, String> {
    assert_sender(&caller, &bridge.extra_origins())?;
    let target = policy::parse_guest_target(&url, &blocked_origins(&app, &bridge))?;
    let webview = state.guest_webview(caller.label(), &lease)?;
    let result = state.with_guest(&lease, |guest| {
        let changed = guest.begin_loading();
        (changed, guest.snapshot.clone())
    });
    let snapshot = publish(&app, result)?;
    webview
        .navigate(target)
        .map_err(|error| format!("dsh desktop: could not navigate the browser guest: {error}"))?;
    Ok(snapshot)
}

/// `goBack` / `goForward` / `reload` / `clearHistory` / `focus`。
#[tauri::command]
pub fn desktop_browser_guest_command(
    caller: Webview,
    app: AppHandle,
    bridge: State<'_, BridgeState>,
    state: State<'_, BrowserState>,
    lease: String,
    command: String,
) -> Result<GuestSnapshot, String> {
    assert_sender(&caller, &bridge.extra_origins())?;
    let webview = state.guest_webview(caller.label(), &lease)?;
    match command.as_str() {
        "back" | "forward" => {
            let result = state.with_guest(&lease, |guest| {
                let target = if command == "back" {
                    guest.history.back()
                } else {
                    guest.history.forward()
                };
                if target.is_some() {
                    guest.begin_loading();
                }
                (target, guest.snapshot.clone())
            });
            let Some((target, snapshot)) = result else {
                return Err("dsh desktop: unknown browser guest lease".to_owned());
            };
            match target {
                Some(value) => {
                    let target =
                        policy::parse_guest_target(&value, &blocked_origins(&app, &bridge))?;
                    guest::emit_state(&app, &snapshot);
                    webview.navigate(target).map_err(|error| {
                        format!("dsh desktop: could not navigate the browser guest: {error}")
                    })?;
                    Ok(snapshot)
                }
                None => Ok(snapshot),
            }
        }
        "reload" => {
            let result = state.with_guest(&lease, |guest| {
                let changed = guest.begin_loading();
                (changed, guest.snapshot.clone())
            });
            let snapshot = publish(&app, result)?;
            webview.reload().map_err(|error| {
                format!("dsh desktop: could not reload the browser guest: {error}")
            })?;
            Ok(snapshot)
        }
        "clear-history" => {
            let result = state.with_guest(&lease, |guest| {
                let changed = guest.clear_history();
                (changed, guest.snapshot.clone())
            });
            publish(&app, result)
        }
        "focus" => {
            webview.set_focus().map_err(|error| {
                format!("dsh desktop: could not focus the browser guest: {error}")
            })?;
            state.snapshot(caller.label(), &lease)
        }
        other => Err(format!(
            "dsh desktop: unsupported browser guest command: {other}"
        )),
    }
}
