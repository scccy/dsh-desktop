//! 非 macOS 平台的麦克风占位实现。
//!
//! 页面（`media-shim.js`）在 `getUserMedia` 里把 `unsupported` 映射成
//! `NotSupportedError`，语音输入插件据此显示「不可用」，而不是让命令抛「找不到」。
//! Windows / Linux 的原生采集是后续独立工作（分别对应 WASAPI 与 PipeWire/ALSA）。

use tauri::AppHandle;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::media::MicOptions;
use crate::media::permission::{Authorization, PermissionSnapshot};
use crate::media::session::Phase;

/// `desktop_microphone_status` 的实现。
/// @returns 「未知授权 + 无设备」的快照。
pub fn status() -> PermissionSnapshot {
    PermissionSnapshot::new(Authorization::Unknown, Vec::new())
}

/// `desktop_microphone_start` 的实现：本平台没有原生采集。
/// @returns 固定失败原因 `unsupported`。
pub fn start(
    _app: &AppHandle,
    _options: MicOptions,
    _channel: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    Err("unsupported".to_owned())
}

/// `desktop_microphone_stop` 的实现：没有会话可停。
pub fn stop(_app: &AppHandle) {}

/// 诊断：@returns 恒为 idle（本平台没有采集会话）。
pub fn phase() -> Phase {
    Phase::Idle
}

/// 诊断：@returns 恒为 `None`。
pub fn frame_count() -> Option<u64> {
    None
}
