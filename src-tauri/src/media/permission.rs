//! TCC 授权状态与启动决策。
//!
//! macOS 的麦克风权限由 TCC 管：`AVCaptureDevice.authorizationStatus(for: .audio)`
//! 返回 notDetermined / restricted / denied / authorized，`requestAccess(for:.audio)`
//! 才会弹系统授权窗（每个 bundle 只弹一次）。
//!
//! 这里把「当前授权 + 有没有设备」映射成明确的启动决策，并给出**与页面一致的失败原因串**
//! （页面 `media-shim.js` 的 `failureFor()` 按这些串造 `DOMException`，两边必须逐字相同，
//! 见本文件的 `failure_reasons_match_the_page_shim` 测试）。
//!
//! 注意一个 macOS 特有的事实：**授权之前系统不列出任何音频设备**
//! （`AVCaptureDevice.devicesWithMediaType:` 返回空）。所以「没设备」在
//! notDetermined 阶段是正常的，不能被判定成「机器没有麦克风」。

use serde::Serialize;

/// `AVAuthorizationStatus` 的 Rust 表示。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authorization {
    /// 还没问过用户：可以弹窗。
    NotDetermined,
    /// 被家长控制 / MDM 限制：弹窗也不会出现。
    Restricted,
    /// 用户明确拒绝：只能去系统设置里改。
    Denied,
    /// 已授权。
    Granted,
    /// 系统返回了未知值（新系统加了枚举）。
    Unknown,
}

impl Authorization {
    /// 从 `AVAuthorizationStatus` 的原始值转换（0..=3，与 AVFoundation 一致）。
    /// @param raw - 系统返回值。
    /// @returns 对应状态；超范围时为 [`Authorization::Unknown`]。
    pub fn from_raw(raw: isize) -> Self {
        match raw {
            0 => Authorization::NotDetermined,
            1 => Authorization::Restricted,
            2 => Authorization::Denied,
            3 => Authorization::Granted,
            _ => Authorization::Unknown,
        }
    }

    /// @returns 给页面的授权状态串。
    pub fn as_str(self) -> &'static str {
        match self {
            Authorization::NotDetermined => "not-determined",
            Authorization::Restricted => "restricted",
            Authorization::Denied => "denied",
            Authorization::Granted => "granted",
            Authorization::Unknown => "unknown",
        }
    }

    /// @returns 是否必须走一次系统授权请求。
    pub fn needs_request(self) -> bool {
        matches!(self, Authorization::NotDetermined)
    }
}

/// 一个音频输入设备（`AVCaptureDevice` 的子集）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    /// `uniqueID`，跨启动稳定，页面用它做 `deviceId`。
    pub id: String,
    /// 用户可见名（如「MacBook Pro 麦克风」）。
    pub label: String,
    /// `AVCaptureDevice.groupID` 的字符串形式；同一物理设备的输入/输出共享它。
    pub group_id: String,
    /// 是否是系统默认输入。
    pub is_default: bool,
}

/// 启动采集前的决策。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartDecision {
    /// 有授权、有设备：直接开采集。
    Start,
    /// 还没问过：先 `requestAccess`，成功再开。
    RequestAccess,
    /// 不能开，附带给页面的原因串。
    Refuse(&'static str),
}

/// 失败 / 拒绝原因串（与页面 `failureFor()` 的 `switch` 一一对应）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureReason {
    /// 用户或系统策略拒绝授权。
    Denied,
    /// 没有可用的输入设备。
    DeviceMissing,
    /// 指定的 `deviceId` 不存在（页面的 `{deviceId:{exact}}`）。
    BadDevice,
    /// 设备被别的进程独占。
    Busy,
    /// 平台不支持（非 macOS 或 WebView 侧不支持）。
    Unsupported,
}

impl FailureReason {
    /// @returns 给页面的原因串。
    pub fn as_str(self) -> &'static str {
        match self {
            FailureReason::Denied => "denied",
            FailureReason::DeviceMissing => "device-missing",
            FailureReason::BadDevice => "bad-device",
            FailureReason::Busy => "busy",
            FailureReason::Unsupported => "unsupported",
        }
    }
}

/// 依据授权状态与设备可用性给出启动决策。
///
/// 规则（顺序有意义）：
///
/// - notDetermined 一律先要授权，**不管当前看不看得到设备**（授权前系统不列设备）；
/// - restricted / denied / unknown 一律拒绝 —— 弹窗也不会成功；
/// - 已授权但没有设备 → `device-missing`。
///
/// @param authorization - 当前授权状态。
/// @param has_device - 是否至少有一个音频输入设备。
/// @returns 启动决策。
pub fn start_decision(authorization: Authorization, has_device: bool) -> StartDecision {
    if authorization.needs_request() {
        return StartDecision::RequestAccess;
    }
    match authorization {
        // 上面已经拦掉 notDetermined，这里再列一次是为了让 match 保持穷尽。
        Authorization::NotDetermined => StartDecision::RequestAccess,
        Authorization::Granted => {
            if has_device {
                StartDecision::Start
            } else {
                StartDecision::Refuse(FailureReason::DeviceMissing.as_str())
            }
        }
        Authorization::Restricted | Authorization::Denied => {
            StartDecision::Refuse(FailureReason::Denied.as_str())
        }
        Authorization::Unknown => StartDecision::Refuse(FailureReason::Unsupported.as_str()),
    }
}

/// 在设备清单里挑出本次要用的设备。
///
/// `requested` 为空表示「系统默认」；否则要求精确命中 `uniqueID`
/// （页面传的是 `{ deviceId: { exact } }`，命中不了就该报 `bad-device`，
/// 而不是悄悄换成默认设备录错人）。
/// @param devices - 当前设备清单。
/// @param requested - 页面要的 `uniqueID`，空串表示默认。
/// @returns 选中的设备；找不到时为 `None`。
pub fn select_device<'a>(devices: &'a [DeviceInfo], requested: &str) -> Option<&'a DeviceInfo> {
    if requested.is_empty() {
        if let Some(found) = devices.iter().find(|device| device.is_default) {
            return Some(found);
        }
        return devices.first();
    }
    devices.iter().find(|device| device.id == requested)
}

/// `desktop_microphone_status` 的返回体。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionSnapshot {
    /// 授权状态串。
    pub authorization: &'static str,
    /// 是否至少有一个输入设备。
    pub available: bool,
    /// 设备清单（授权前为空）。
    pub devices: Vec<DeviceInfo>,
}

impl PermissionSnapshot {
    /// 组装一份快照。
    /// @param authorization - 当前授权。
    /// @param devices - 当前设备清单。
    pub fn new(authorization: Authorization, devices: Vec<DeviceInfo>) -> Self {
        Self {
            authorization: authorization.as_str(),
            available: !devices.is_empty(),
            devices,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, is_default: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.to_owned(),
            label: format!("mic {id}"),
            group_id: "group".to_owned(),
            is_default,
        }
    }

    #[test]
    fn raw_status_maps_to_the_four_system_values() {
        assert_eq!(Authorization::from_raw(0), Authorization::NotDetermined);
        assert_eq!(Authorization::from_raw(1), Authorization::Restricted);
        assert_eq!(Authorization::from_raw(2), Authorization::Denied);
        assert_eq!(Authorization::from_raw(3), Authorization::Granted);
        assert_eq!(Authorization::from_raw(9), Authorization::Unknown);
        assert_eq!(Authorization::from_raw(-1), Authorization::Unknown);
    }

    #[test]
    fn undetermined_always_requests_access_even_without_devices() {
        // 授权前 macOS 不列设备，这是正常状态，不能判成「没有麦克风」。
        assert_eq!(
            start_decision(Authorization::NotDetermined, false),
            StartDecision::RequestAccess
        );
        assert_eq!(
            start_decision(Authorization::NotDetermined, true),
            StartDecision::RequestAccess
        );
        assert!(Authorization::NotDetermined.needs_request());
        assert!(!Authorization::Granted.needs_request());
    }

    #[test]
    fn granted_without_a_device_is_device_missing() {
        assert_eq!(
            start_decision(Authorization::Granted, true),
            StartDecision::Start
        );
        assert_eq!(
            start_decision(Authorization::Granted, false),
            StartDecision::Refuse("device-missing")
        );
    }

    #[test]
    fn denied_and_restricted_are_both_denied() {
        assert_eq!(
            start_decision(Authorization::Denied, true),
            StartDecision::Refuse("denied")
        );
        assert_eq!(
            start_decision(Authorization::Restricted, true),
            StartDecision::Refuse("denied")
        );
        assert_eq!(
            start_decision(Authorization::Unknown, true),
            StartDecision::Refuse("unsupported")
        );
    }

    #[test]
    fn default_selection_prefers_the_system_default() {
        let devices = vec![device("a", false), device("b", true)];
        assert_eq!(
            select_device(&devices, "").map(|d| d.id.as_str()),
            Some("b")
        );
        assert_eq!(
            select_device(&devices, "a").map(|d| d.id.as_str()),
            Some("a")
        );
        // 精确匹配失败就是失败：不能悄悄回落到默认设备。
        assert!(select_device(&devices, "zzz").is_none());
    }

    #[test]
    fn selection_falls_back_to_the_first_device_without_a_default_flag() {
        let devices = vec![device("a", false), device("b", false)];
        assert_eq!(
            select_device(&devices, "").map(|d| d.id.as_str()),
            Some("a")
        );
        assert!(select_device(&[], "").is_none());
    }

    #[test]
    fn failure_reasons_match_the_page_shim() {
        // 这几条串是 media-shim.js `failureFor()` 的 switch 分支，逐字相同。
        assert_eq!(FailureReason::Denied.as_str(), "denied");
        assert_eq!(FailureReason::DeviceMissing.as_str(), "device-missing");
        assert_eq!(FailureReason::BadDevice.as_str(), "bad-device");
        assert_eq!(FailureReason::Busy.as_str(), "busy");
        assert_eq!(FailureReason::Unsupported.as_str(), "unsupported");
    }

    #[test]
    fn snapshot_serialises_for_the_page() {
        let snapshot = PermissionSnapshot::new(Authorization::Granted, vec![device("a", true)]);
        let json = serde_json::to_value(&snapshot).expect("serialisable");
        assert_eq!(json["authorization"], "granted");
        assert_eq!(json["available"], true);
        assert_eq!(json["devices"][0]["id"], "a");
        assert_eq!(json["devices"][0]["groupId"], "group");
        assert_eq!(json["devices"][0]["isDefault"], true);

        let empty = PermissionSnapshot::new(Authorization::Denied, Vec::new());
        let json = serde_json::to_value(&empty).expect("serialisable");
        assert_eq!(json["authorization"], "denied");
        assert_eq!(json["available"], false);
        assert_eq!(json["devices"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn authorization_strings_are_stable() {
        assert_eq!(Authorization::NotDetermined.as_str(), "not-determined");
        assert_eq!(Authorization::Restricted.as_str(), "restricted");
        assert_eq!(Authorization::Denied.as_str(), "denied");
        assert_eq!(Authorization::Granted.as_str(), "granted");
        assert_eq!(Authorization::Unknown.as_str(), "unknown");
    }
}
