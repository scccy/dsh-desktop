//! 子 webview（guest）的创建、状态跟踪与几何同步。
//!
//! 每一个 Sidebar 浏览器的 guest 是窗口里的一个**独立子 webview**（`Window::add_child`，
//! 需要 tauri 的 `unstable` feature），挂在 `content` webview 之后，因此天然叠在渲染层之上。
//! 页面里的 `<webview>` 元素只负责占位与观测，真实视口由这里按它的
//! `getBoundingClientRect()` 摆位 —— 兼容层在 `src-tauri/bridge/webview-shim.js`。
//!
//! 与官方 `apps/desktop/src/browser-guests.ts` 的映射：
//!
//! | 官方（Electron） | 这里（Tauri / wry） |
//! |---|---|
//! | `session.fromPartition(partition)` 隔离存储 | `WebviewBuilder::data_store_identifier`（macOS ≥ 14）/ `data_directory`（Win/Linux） |
//! | 固定 `webPreferences`（无 node、sandbox、无插件） | 子 webview 本来就是纯系统 WebView，没有 node 桥 |
//! | `will-attach-webview` 校验 lease + partition | `desktop_browser_guest_attach` 的 lease/partition 校验 |
//! | `will-frame-navigate` / `will-redirect` | `WebviewBuilder::on_navigation`（见 policy.rs） |
//! | `setWindowOpenHandler` → deny + 上报 | `on_new_window` → `NewWindowResponse::Deny` + `browser-open-requested` 事件 |
//! | `will-download` → preventDefault | `on_download` 返回 `false` + `browser-download-blocked` 事件 |
//! | `setPermissionRequestHandler(false)` | `on_permission_request` → `PermissionResponse::Deny` |
//! | `did-navigate` / `did-stop-loading` / `page-title-updated` | `on_page_load` / `on_document_title_changed` → 状态事件，由 shim 派发同名 DOM 事件 |
//! | `canGoBack` / `goBack` | 线性历史模型（history.rs，非原生 history 恢复） |
//! | `did-fail-load` | **无等价物**：加载失败只表现为 loading 结束、URL 不变 |
//! | `guest.on('login')` → cancel | 无等价物：guest 弹不出原生认证框 |

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::webview::{
    DownloadEvent, NewWindowResponse, PageLoadEvent, PermissionResponse, WebviewBuilder,
};
use tauri::{
    AppHandle, Emitter, EventTarget, LogicalPosition, LogicalSize, Manager, PhysicalPosition,
    PhysicalSize, Rect, Webview, WebviewUrl, Window,
};
use url::Url;

use crate::bridge::EVENT_BROWSER_OPEN_REQUESTED;
use crate::window::CONTENT_WEBVIEW;

use super::history::GuestHistory;
use super::lease::{BrowserState, Reservation};
use super::partition;
use super::policy::{self, BlockedOrigins};

/// guest 的初始文档：真正的目标 URL 由页面 `loadURL()` 发起，对齐官方
/// 「`about:blank#<lease>` 承载 lease、再导航」的两段式。
pub(crate) const ABOUT_BLANK: &str = "about:blank";

/// 事件：guest 状态快照变化。payload 是 [`GuestSnapshot`]，发给 `content` webview，
/// 由 shim 找到对应元素后派发 `did-navigate` / `did-stop-loading` 等同名 DOM 事件。
pub const EVENT_GUEST_STATE: &str = "dsh-desktop://browser-guest-state";

/// 事件：guest 发起了一次被拦下的下载（官方 `will-download` 静默拦截，这里额外上报）。
pub const EVENT_GUEST_DOWNLOAD: &str = "dsh-desktop://browser-download-blocked";

/// 页面看到的 guest 状态快照，同时是 `desktop_browser_guest_*` 的返回值与事件 payload。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuestSnapshot {
    /// 该 guest 的 lease（页面据此把状态对回 `<webview>` 元素）。
    pub lease: String,
    /// 主 frame 当前已提交的 URL。
    pub url: String,
    /// 文档标题（`on_document_title_changed`）。
    pub title: String,
    /// 主 frame 是否在加载。
    pub loading: bool,
    /// 线性历史里的可后退状态（近似 `canGoBack`）。
    pub can_go_back: bool,
    /// 线性历史里的可前进状态（近似 `canGoForward`）。
    pub can_go_forward: bool,
    /// 子 webview 当前是否可见。
    pub visible: bool,
}

/// 页面里 `<webview>` 元素的位置：CSS px，相对 `content` webview 视口左上角。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuestBounds {
    /// 左边界（CSS px）。
    pub x: f64,
    /// 上边界（CSS px）。
    pub y: f64,
    /// 宽度（CSS px）。
    pub width: f64,
    /// 高度（CSS px）。
    pub height: f64,
    /// 元素是否真的该显示（页面一侧已按可见性/裁剪判定）。
    pub visible: bool,
}

/// 一个已挂载的 guest。
pub struct Guest {
    /// 子 webview 句柄。
    pub(crate) webview: Webview,
    /// 线性历史。
    pub(crate) history: GuestHistory,
    /// 页面读到的状态快照。
    pub(crate) snapshot: GuestSnapshot,
}

impl Guest {
    /// 包装刚创建的子 webview。
    pub(crate) fn new(webview: Webview, lease: &str) -> Self {
        Self {
            webview,
            history: GuestHistory::new(),
            snapshot: GuestSnapshot {
                lease: lease.to_owned(),
                url: ABOUT_BLANK.to_owned(),
                ..GuestSnapshot::default()
            },
        }
    }

    /// `PageLoadEvent::Started`：进入加载态。@returns 快照是否有变化。
    pub(crate) fn begin_loading(&mut self) -> bool {
        if self.snapshot.loading {
            return false;
        }
        self.snapshot.loading = true;
        true
    }

    /// `PageLoadEvent::Finished`：提交主 frame 文档。@returns 快照是否有变化。
    pub(crate) fn commit(&mut self, url: &str) -> bool {
        let mut changed = false;
        self.history.commit(url);
        if !url.is_empty() && self.snapshot.url != url {
            self.snapshot.url = url.to_owned();
            changed = true;
        }
        if self.snapshot.loading {
            self.snapshot.loading = false;
            changed = true;
        }
        changed | self.refresh_history_flags()
    }

    /// 清空历史但保留当前文档（Electron 的 `clearHistory`）。@returns 快照是否有变化。
    pub(crate) fn clear_history(&mut self) -> bool {
        // 以历史里的当前位置为准；历史还没提交过文档时回落到快照里的 URL。
        let current = self
            .history
            .current()
            .unwrap_or(&self.snapshot.url)
            .to_owned();
        self.history.clear(&current);
        self.refresh_history_flags()
    }

    /// 把线性历史的位置同步进快照。@returns 快照是否有变化。
    fn refresh_history_flags(&mut self) -> bool {
        let mut changed = false;
        let can_go_back = self.history.can_go_back();
        if self.snapshot.can_go_back != can_go_back {
            self.snapshot.can_go_back = can_go_back;
            changed = true;
        }
        let can_go_forward = self.history.can_go_forward();
        if self.snapshot.can_go_forward != can_go_forward {
            self.snapshot.can_go_forward = can_go_forward;
            changed = true;
        }
        changed
    }

    /// 更新标题。@returns 快照是否有变化。
    pub(crate) fn set_title(&mut self, title: &str) -> bool {
        if self.snapshot.title == title {
            return false;
        }
        self.snapshot.title = title.to_owned();
        true
    }

    /// 记录可见性。@returns 是否发生变化。
    pub(crate) fn set_visible(&mut self, visible: bool) -> bool {
        if self.snapshot.visible == visible {
            return false;
        }
        self.snapshot.visible = visible;
        true
    }

    /// 关闭子 webview（幂等）。
    pub(crate) fn close(&self) {
        if let Err(error) = self.webview.close() {
            log_warn(
                self.webview.app_handle(),
                &self.snapshot.lease,
                &format!("close failed: {error}"),
            );
        }
    }
}

/// 创建一个 guest 子 webview，初始 `1x1` 隐藏，等页面报上 bounds 再显示。
pub(crate) fn create(
    app: &AppHandle,
    window: &Window,
    state: &BrowserState,
    reservation: &Reservation,
    blocked: BlockedOrigins,
) -> Result<Webview, String> {
    let label = state.next_guest_label();
    let initial = WebviewUrl::External(
        ABOUT_BLANK
            .parse()
            .map_err(|error| format!("dsh desktop: invalid guest document: {error}"))?,
    );

    let load_app = app.clone();
    let load_lease = reservation.lease.clone();
    let title_app = app.clone();
    let title_lease = reservation.lease.clone();
    let open_app = app.clone();
    let open_lease = reservation.lease.clone();
    let download_app = app.clone();
    let download_lease = reservation.lease.clone();
    let navigation_blocked = blocked.clone();
    let open_blocked = blocked.clone();

    let builder = WebviewBuilder::new(&label, initial)
        // 持久分区：macOS ≥ 14 用 data store identifier，Windows / Linux 用独立 data 目录。
        .data_store_identifier(partition::data_store_identifier(&reservation.partition))
        .data_directory(partition::data_directory(
            state.root(),
            &reservation.partition,
        ))
        // 不让 guest 抢焦点：焦点只在用户真的点进页面时由系统给。
        .focused(false)
        // 主 frame 导航白名单（对齐 will-frame-navigate / will-redirect），
        // 额外放行引导文档 about:blank。
        .on_navigation(move |url| policy::allows_guest_navigation(url, &navigation_blocked))
        // 原生新窗口一律拒绝；白名单内的 URL 上报给页面自己开标签。
        .on_new_window(move |url, _features| {
            report_open_request(&open_app, &open_lease, &url, &open_blocked);
            NewWindowResponse::Deny
        })
        // 下载一律拦截（对齐 will-download → preventDefault），只上报 URL。
        .on_download(move |_webview, event| {
            if let DownloadEvent::Requested { url, .. } = event {
                report_download(&download_app, &download_lease, &url);
            }
            false
        })
        // 权限一律拒绝（对齐 setPermissionRequestHandler(false)）。
        .on_permission_request(|_webview, _kind| PermissionResponse::Deny)
        .on_document_title_changed(move |_webview, title| {
            report_title(&title_app, &title_lease, &title);
        })
        .on_page_load(move |_webview, payload| {
            report_page_load(&load_app, &load_lease, payload.url(), payload.event());
        });

    let webview = window
        .add_child(
            builder,
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(1.0, 1.0),
        )
        .map_err(|error| format!("dsh desktop: could not create the browser guest: {error}"))?;
    // 先隐藏：页面把 `<webview>` 元素的位置报上来之后再显示，避免闪现到窗口左上角。
    if let Err(error) = webview.hide() {
        log_warn(app, &reservation.lease, &format!("hide failed: {error}"));
    }
    Ok(webview)
}

/// 把页面里的元素矩形映射到子 webview。@returns 映射后是否可见。
pub(crate) fn apply_bounds(
    caller: &Webview,
    guest: &Webview,
    input: GuestBounds,
) -> Result<bool, String> {
    if !input.visible || input.width <= 0.5 || input.height <= 0.5 {
        guest
            .hide()
            .map_err(|error| format!("dsh desktop: could not hide the browser guest: {error}"))?;
        return Ok(false);
    }
    let scale = caller
        .window()
        .scale_factor()
        .map_err(|error| format!("dsh desktop: the window scale factor is unavailable: {error}"))?;
    let origin = caller
        .position()
        .map_err(|error| format!("dsh desktop: the content position is unavailable: {error}"))?;
    let area = caller
        .size()
        .map_err(|error| format!("dsh desktop: the content size is unavailable: {error}"))?;

    // CSS px → 物理 px，再裁剪到 content webview 的可见区域。
    let left = (input.x * scale).round();
    let top = (input.y * scale).round();
    let max_width = f64::from(area.width);
    let max_height = f64::from(area.height);
    let x = left.clamp(0.0, max_width);
    let y = top.clamp(0.0, max_height);
    let right = (left + (input.width * scale).round()).clamp(0.0, max_width);
    let bottom = (top + (input.height * scale).round()).clamp(0.0, max_height);
    let width = (right - x).max(0.0);
    let height = (bottom - y).max(0.0);
    if width < 1.0 || height < 1.0 {
        // 元素被完全滚出视口或被压扁：隐藏而不是给 0 尺寸（部分平台会因此崩）。
        guest
            .hide()
            .map_err(|error| format!("dsh desktop: could not hide the browser guest: {error}"))?;
        return Ok(false);
    }
    let bounds = Rect {
        position: PhysicalPosition::new(origin.x + x as i32, origin.y + y as i32).into(),
        size: PhysicalSize::new(width as u32, height as u32).into(),
    };
    guest
        .set_bounds(bounds)
        .map_err(|error| format!("dsh desktop: could not place the browser guest: {error}"))?;
    guest
        .show()
        .map_err(|error| format!("dsh desktop: could not show the browser guest: {error}"))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// 状态上报
// ---------------------------------------------------------------------------

/// 把最新快照发给 `content` webview。
pub(crate) fn emit_state(app: &AppHandle, snapshot: &GuestSnapshot) {
    if let Err(error) = app.emit_to(
        EventTarget::webview(CONTENT_WEBVIEW),
        EVENT_GUEST_STATE,
        snapshot,
    ) {
        log_warn(app, &snapshot.lease, &format!("state emit failed: {error}"));
    }
}

/// 上报一次被拒绝的新窗口请求（payload 与官方 IPC 一致：`{ lease, url }`）。
fn report_open_request(app: &AppHandle, lease: &str, url: &Url, blocked: &BlockedOrigins) {
    if !policy::allows_new_window(url, blocked) {
        log_warn(app, lease, &format!("refused to open {}", safe(url)));
        return;
    }
    let payload = json!({ "lease": lease, "url": url.as_str() });
    if let Err(error) = app.emit_to(
        EventTarget::webview(CONTENT_WEBVIEW),
        EVENT_BROWSER_OPEN_REQUESTED,
        payload,
    ) {
        log_warn(app, lease, &format!("open request emit failed: {error}"));
    }
}

/// 上报一次被拦下的下载。
fn report_download(app: &AppHandle, lease: &str, url: &Url) {
    let payload = json!({ "lease": lease, "url": url.as_str() });
    if let Err(error) = app.emit_to(
        EventTarget::webview(CONTENT_WEBVIEW),
        EVENT_GUEST_DOWNLOAD,
        payload,
    ) {
        log_warn(app, lease, &format!("download emit failed: {error}"));
    }
}

/// `PageLoadEvent::{Started,Finished}` → 快照更新 + 事件。
fn report_page_load(app: &AppHandle, lease: &str, url: &Url, event: PageLoadEvent) {
    let Some(state) = app.try_state::<BrowserState>() else {
        return;
    };
    let result = state.with_guest(lease, |guest| {
        let changed = match event {
            PageLoadEvent::Started => guest.begin_loading(),
            PageLoadEvent::Finished => guest.commit(url.as_str()),
        };
        (changed, guest.snapshot.clone())
    });
    if let Some((true, snapshot)) = result {
        emit_state(app, &snapshot);
    }
}

/// 文档标题变化 → 快照更新 + 事件。
fn report_title(app: &AppHandle, lease: &str, title: &str) {
    let Some(state) = app.try_state::<BrowserState>() else {
        return;
    };
    let result = state.with_guest(lease, |guest| {
        let changed = guest.set_title(title);
        (changed, guest.snapshot.clone())
    });
    if let Some((true, snapshot)) = result {
        emit_state(app, &snapshot);
    }
}

// ---------------------------------------------------------------------------
// 日志与脱敏
// ---------------------------------------------------------------------------

/// 写一行 info 日志（壳的滚动日志）。
pub(crate) fn log_info(app: &AppHandle, lease: &str, message: &str) {
    if let Some(state) = app.try_state::<crate::state::AppState>() {
        state
            .logger
            .info(&format!("browser guest {lease}: {message}"));
    }
}

/// 写一行 warn 日志（壳的滚动日志）。
pub(crate) fn log_warn(app: &AppHandle, lease: &str, message: &str) {
    if let Some(state) = app.try_state::<crate::state::AppState>() {
        state
            .logger
            .warn(&format!("browser guest {lease}: {message}"));
    }
}

/// 去掉 query 与 fragment 的 URL：日志里只留 origin + path（query 可能带搜索词）。
fn safe(url: &Url) -> String {
    let mut trimmed = url.clone();
    trimmed.set_query(None);
    trimmed.set_fragment(None);
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::{ABOUT_BLANK, GuestBounds, GuestSnapshot};
    use serde_json::json;

    #[test]
    fn snapshot_uses_camel_case_keys() {
        let snapshot = GuestSnapshot {
            lease: "lease-1".to_owned(),
            url: "https://example.com/".to_owned(),
            title: "Example".to_owned(),
            loading: true,
            can_go_back: true,
            can_go_forward: false,
            visible: false,
        };
        let value = serde_json::to_value(&snapshot).expect("snapshot serializes");
        assert_eq!(value["lease"], json!("lease-1"));
        assert_eq!(value["canGoBack"], json!(true));
        assert_eq!(value["canGoForward"], json!(false));
        assert_eq!(value["visible"], json!(false));
        assert!(value.get("can_go_back").is_none());
        // 反序列化同样认 camelCase（事件 payload 与命令返回值共用一个形状）
        let parsed: GuestSnapshot = serde_json::from_value(value).expect("snapshot deserializes");
        assert_eq!(parsed, snapshot);
    }

    #[test]
    fn bounds_accept_camel_case_payloads() {
        let bounds: GuestBounds = serde_json::from_value(
            json!({ "x": 1, "y": 2, "width": 30, "height": 40, "visible": true }),
        )
        .expect("bounds deserialize");
        assert_eq!(bounds.x, 1.0);
        assert_eq!(bounds.height, 40.0);
        assert!(bounds.visible);
    }

    #[test]
    fn about_blank_is_the_initial_document() {
        assert_eq!(ABOUT_BLANK, "about:blank");
    }
}
