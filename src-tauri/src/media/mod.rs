//! 麦克风与输入能力（P3）。
//!
//! macOS 的 WKWebView 没有实现 Media Capture（`navigator.mediaDevices` 不存在，
//! 见 `docs/spikes/media-input.md`），所以采集必须由壳自己做：
//!
//! ```text
//! AVCaptureSession（media/macos.rs）
//!   → 20 ms 单声道 f32 PCM（本模块：格式转换 / 重采样 / 分帧）
//!   → Tauri `Channel<InvokeResponseBody>` 二进制帧
//!   → 页面 `media-shim.js`：PcmRing → ScriptProcessorNode
//!   → MediaStreamAudioDestinationNode.stream → 冒充 getUserMedia() 的返回值
//! ```
//!
//! 这个模块只放**可单测的纯逻辑**（不 `use tauri::*`、不碰窗口、不碰 IPC 类型），
//! 因此可以在无显示器、无麦克风、无授权的 CI 上跑：
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`permission`] | TCC 授权状态、设备可用性、启动决策，以及给页面的失败原因串 |
//! | [`pcm`] | 采样格式归一化（f32/i16、交错/非交错、下混）、流式重采样、定长分帧、f32le 编码 |
//! | [`session`] | 采集会话状态机（idle/requesting/starting/live/stopping/failed）与副作用的对外动作 |
//!
//! 平台实现 `macos.rs` 在依赖与真机验收就绪后由 `#[cfg(target_os = "macos")] mod macos;`
//! 挂上（接线见 `docs/bridge-contract.md` 的媒体一节与最终回报的接线清单）。

pub mod pcm;
pub mod permission;
pub mod session;

/// 原生采集实现（AVCaptureSession）。非 macOS 平台不需要它：命令层由
/// [`unsupported`] 提供同名函数，三平台共用一份注册列表。
#[cfg(target_os = "macos")]
pub mod macos;

/// 非 macOS 的占位实现：命令存在、语义明确（`unsupported`），页面按
/// `NotSupportedError` 处理，而不是「命令找不到」。
#[cfg(not(target_os = "macos"))]
pub mod unsupported;

// 平台选择只发生在这里：bridge.rs 一律写 `media::platform::status/start/stop`，
// 不出现 `#[cfg(target_os = ...)]`（与 `src-tauri/src/platform/` 的分层约定一致）。
#[cfg(target_os = "macos")]
pub use self::macos as platform;
#[cfg(not(target_os = "macos"))]
pub use self::unsupported as platform;

/// 采集输出采样率：与页面 `AudioContext({ sampleRate: 48000 })` 对齐。
pub const CAPTURE_SAMPLE_RATE: u32 = 48_000;
/// 单帧时长（毫秒）。20 ms ≈ 960 帧 ≈ 3840 字节 f32，一帧一次 IPC。
pub const FRAME_MILLIS: u32 = 20;
/// 采集声道数：页面侧只用一个 `AudioWorklet`/ScriptProcessor 输出通道，单声道足够。
pub const CAPTURE_CHANNELS: u16 = 1;
/// 通道首帧协议版本（页面 `MIC_PROTOCOL`，改字段时必须一起加）。
pub const MIC_PROTOCOL: u8 = 1;

// 命令名不由常量表达：Tauri 的 `#[tauri::command]` 用函数名当命令名，
// 常量会与 `bridge.rs` 里的函数名各自漂移，所以只保留事件名（页面与壳唯一的
// 字符串约定，且被 `macos.rs` 的 emit 直接引用）。

/// 麦克风状态事件：`{ state, reason? }`，由 Rust 主动 emit 给页面。
pub const EVENT_MICROPHONE_STATE: &str = "dsh-desktop://microphone-state";

use serde::Serialize;

/// 页面传来的采集选项（`desktop_microphone_start` 的 `options`）。
///
/// 命令签名里收 `serde_json::Value` 再经 [`MicOptions::parse`] 校验：Tauri 对嵌套
/// 结构体的 camelCase 反序列化容易踩坑，而这里需要的是「非法值一律拒绝」而不是
/// 静默取默认值——静默默认会让页面以为按 48 kHz 录，实际却是别的采样率。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicOptions {
    /// 页面 `AudioContext.sampleRate`；壳按它出帧，避免页面侧再重采样。
    pub sample_rate: u32,
    /// 只有单声道被接受（多声道没有消费方，白白多一倍带宽）。
    pub channel_count: u16,
    /// 单帧毫秒数，必须能整除出整数帧。
    pub frame_millis: u32,
    /// 设备 `uniqueID`；空串表示系统默认设备。
    pub device_id: String,
    /// 系统回声消除（在 macOS 上要选带 voice processing 的设备，未实现时仅为记录）。
    pub echo_cancellation: bool,
    /// 降噪，同上。
    pub noise_suppression: bool,
    /// 自动增益，同上。
    pub auto_gain_control: bool,
}

/// 选项校验失败的具体原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionError {
    /// 不是 JSON 对象。
    NotAnObject,
    /// 协议版本不匹配（页面与壳不同步）。
    Protocol,
    /// 采样率不在允许区间。
    SampleRate,
    /// 声道数只支持 1。
    ChannelCount,
    /// 帧长不是 5/10/20/40/100 之一，或采样率除不尽。
    FrameMillis,
}

impl OptionError {
    /// @returns 稳定的错误串（同时作为日志与页面失败原因）。
    pub fn as_str(self) -> &'static str {
        match self {
            OptionError::NotAnObject => "the microphone options are not an object",
            OptionError::Protocol => "the microphone protocol version is unsupported",
            OptionError::SampleRate => "the requested microphone sample rate is unsupported",
            OptionError::ChannelCount => "only one microphone channel is supported",
            OptionError::FrameMillis => "the requested microphone frame length is unsupported",
        }
    }
}

impl MicOptions {
    /// 默认选项：48 kHz / 单声道 / 20 ms / 系统默认设备。
    pub fn defaults() -> Self {
        Self {
            sample_rate: CAPTURE_SAMPLE_RATE,
            channel_count: CAPTURE_CHANNELS,
            frame_millis: FRAME_MILLIS,
            device_id: String::new(),
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        }
    }

    /// 解析并校验页面传来的选项。
    ///
    /// 缺省字段取默认值（页面只传它关心的），但**出现就必须合法**：例如
    /// `sampleRate: 0` 或 `channelCount: 2` 直接拒绝，而不是回落到默认值。
    /// @param value - `desktop_microphone_start` 的 `options` 参数。
    /// @returns 校验通过的选项或具体错误。
    pub fn parse(value: &serde_json::Value) -> Result<Self, OptionError> {
        let object = value.as_object().ok_or(OptionError::NotAnObject)?;
        let mut options = Self::defaults();
        if let Some(protocol) = object.get("protocol") {
            let protocol = protocol.as_u64().ok_or(OptionError::Protocol)?;
            if protocol != u64::from(MIC_PROTOCOL) {
                return Err(OptionError::Protocol);
            }
        }
        if let Some(sample_rate) = object.get("sampleRate") {
            let rate = sample_rate.as_u64().ok_or(OptionError::SampleRate)?;
            let rate = u32::try_from(rate).map_err(|_| OptionError::SampleRate)?;
            // 8 kHz（电话/转写）到 192 kHz（专业声卡）之外没有合理的采集用途。
            if !(8_000..=192_000).contains(&rate) || rate % 100 != 0 {
                return Err(OptionError::SampleRate);
            }
            options.sample_rate = rate;
        }
        if let Some(channel_count) = object.get("channelCount") {
            let count = channel_count.as_u64().ok_or(OptionError::ChannelCount)?;
            if count != u64::from(CAPTURE_CHANNELS) {
                return Err(OptionError::ChannelCount);
            }
            options.channel_count = CAPTURE_CHANNELS;
        }
        if let Some(frame_millis) = object.get("frameMillis") {
            let millis = frame_millis.as_u64().ok_or(OptionError::FrameMillis)?;
            let millis = u32::try_from(millis).map_err(|_| OptionError::FrameMillis)?;
            if !matches!(millis, 5 | 10 | 20 | 40 | 100) {
                return Err(OptionError::FrameMillis);
            }
            if pcm::frames_per_block(options.sample_rate, millis) == 0 {
                return Err(OptionError::FrameMillis);
            }
            options.frame_millis = millis;
        } else if pcm::frames_per_block(options.sample_rate, options.frame_millis) == 0 {
            // 采样率变了但帧长没跟着变时，仍要保证能整除。
            return Err(OptionError::FrameMillis);
        }
        if let Some(device_id) = object.get("deviceId") {
            // 空串是「系统默认」，不是非法值。
            options.device_id = device_id
                .as_str()
                .ok_or(OptionError::FrameMillis)?
                .to_owned();
        }
        options.echo_cancellation = flag(object, "echoCancellation");
        options.noise_suppression = flag(object, "noiseSuppression");
        options.auto_gain_control = flag(object, "autoGainControl");
        Ok(options)
    }

    /// @returns 每帧的采样数。
    pub fn frames_per_frame(&self) -> usize {
        pcm::frames_per_block(self.sample_rate, self.frame_millis)
    }
}

/// 读一个布尔开关；非布尔值按「没开」处理（这类字段只影响音质，不值得拒绝整次启动）。
fn flag(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
    object
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// 通道首帧（JSON 头）：页面据此设置 `sampleRate` 与合成轨的设备身份。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicStreamHeader {
    /// 固定 `"open"`。
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// 壳实际输出的采样率。
    pub sample_rate: u32,
    /// 壳实际输出的声道数（恒 1）。
    pub channel_count: u16,
    /// 每帧采样数。
    pub frames_per_frame: usize,
    /// 采样格式，固定 `"f32le"`。
    pub format: &'static str,
    /// 设备 `uniqueID`。
    pub device_id: String,
    /// 设备所属 group（页面设备选择器用它把 label 与默认项对上）。
    pub group_id: String,
    /// 设备可读名。
    pub label: String,
    /// 系统是否启用了回声消除处理。
    pub echo_cancellation: bool,
    /// 系统是否启用了降噪处理。
    pub noise_suppression: bool,
    /// 系统是否启用了自动增益。
    pub auto_gain_control: bool,
}

impl MicStreamHeader {
    /// 为一次成功启动构造首帧。
    /// @param options - 已生效的采集选项。
    /// @param device - 实际选中的设备。
    pub fn open(options: &MicOptions, device: &permission::DeviceInfo) -> Self {
        Self {
            kind: "open",
            sample_rate: options.sample_rate,
            channel_count: options.channel_count,
            frames_per_frame: options.frames_per_frame(),
            format: "f32le",
            device_id: device.id.clone(),
            group_id: device.group_id.clone(),
            label: device.label.clone(),
            echo_cancellation: options.echo_cancellation,
            noise_suppression: options.noise_suppression,
            auto_gain_control: options.auto_gain_control,
        }
    }
}

/// 状态事件负载（`EVENT_MICROPHONE_STATE`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MicStateEvent {
    /// `idle` / `requesting` / `starting` / `live` / `stopping` / `failed`。
    pub state: &'static str,
    /// 失败或停止原因（页面用于提示与 DOMException 映射）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

impl MicStateEvent {
    /// @param state - 状态名，来自 [`session::Phase::as_str`]。
    /// @param reason - 可选原因串。
    pub fn new(state: &'static str, reason: Option<&'static str>) -> Self {
        Self { state, reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).expect("valid json")
    }

    #[test]
    fn defaults_match_the_page_contract() {
        let options = MicOptions::defaults();
        assert_eq!(options.sample_rate, 48_000);
        assert_eq!(options.channel_count, 1);
        assert_eq!(options.frame_millis, 20);
        assert_eq!(options.frames_per_frame(), 960);
        assert!(options.device_id.is_empty());
    }

    #[test]
    fn missing_options_take_defaults() {
        let options = MicOptions::parse(&value(r#"{}"#)).expect("defaults");
        assert_eq!(options, MicOptions::defaults());
    }

    #[test]
    fn page_options_are_accepted() {
        let options = MicOptions::parse(&value(
            r#"{"protocol":1,"sampleRate":48000,"channelCount":1,"frameMillis":20,
                "deviceId":"BuiltInMicrophoneDevice","echoCancellation":true,
                "noiseSuppression":true,"autoGainControl":false}"#,
        ))
        .expect("valid options");
        assert_eq!(options.sample_rate, 48_000);
        assert_eq!(options.frame_millis, 20);
        assert_eq!(options.device_id, "BuiltInMicrophoneDevice");
        assert!(options.echo_cancellation);
        assert!(options.noise_suppression);
        assert!(!options.auto_gain_control);
    }

    #[test]
    fn sixteen_kilohertz_is_allowed_for_transcription() {
        let options = MicOptions::parse(&value(r#"{"sampleRate":16000}"#)).expect("16 kHz");
        assert_eq!(options.sample_rate, 16_000);
        assert_eq!(options.frames_per_frame(), 320);
    }

    #[test]
    fn invalid_options_are_rejected_not_defaulted() {
        assert_eq!(
            MicOptions::parse(&value("[]")),
            Err(OptionError::NotAnObject)
        );
        assert_eq!(
            MicOptions::parse(&value(r#"{"protocol":2}"#)),
            Err(OptionError::Protocol)
        );
        assert_eq!(
            MicOptions::parse(&value(r#"{"sampleRate":0}"#)),
            Err(OptionError::SampleRate)
        );
        assert_eq!(
            MicOptions::parse(&value(r#"{"sampleRate":"48000"}"#)),
            Err(OptionError::SampleRate)
        );
        assert_eq!(
            MicOptions::parse(&value(r#"{"sampleRate":44110}"#)),
            Err(OptionError::SampleRate)
        );
        assert_eq!(
            MicOptions::parse(&value(r#"{"channelCount":2}"#)),
            Err(OptionError::ChannelCount)
        );
        assert_eq!(
            MicOptions::parse(&value(r#"{"frameMillis":33}"#)),
            Err(OptionError::FrameMillis)
        );
        // 48 kHz 下 5 ms 只有 240 帧，仍然能整除；但 8 kHz 下 5 ms 是 40 帧，也可以。
        assert!(MicOptions::parse(&value(r#"{"sampleRate":8000,"frameMillis":5}"#)).is_ok());
    }

    #[test]
    fn sample_rate_without_frame_length_still_divides() {
        // 100 ms @ 8000 Hz = 800 帧，默认 20 ms 仍然整除。
        assert!(MicOptions::parse(&value(r#"{"sampleRate":8000}"#)).is_ok());
        // 11025 Hz 不是 100 的倍数，直接拒绝（避免出半帧）。
        assert_eq!(
            MicOptions::parse(&value(r#"{"sampleRate":11025}"#)),
            Err(OptionError::SampleRate)
        );
    }

    #[test]
    fn header_describes_the_actual_stream() {
        let options = MicOptions::defaults();
        let device = permission::DeviceInfo {
            id: "mic-1".to_owned(),
            label: "MacBook Pro 麦克风".to_owned(),
            group_id: "group-1".to_owned(),
            is_default: true,
        };
        let header = MicStreamHeader::open(&options, &device);
        let json = serde_json::to_value(&header).expect("serialisable");
        assert_eq!(json["type"], "open");
        assert_eq!(json["sampleRate"], 48_000);
        assert_eq!(json["channelCount"], 1);
        assert_eq!(json["framesPerFrame"], 960);
        assert_eq!(json["format"], "f32le");
        assert_eq!(json["deviceId"], "mic-1");
        assert_eq!(json["groupId"], "group-1");
        assert_eq!(json["label"], "MacBook Pro 麦克风");
    }

    #[test]
    fn state_events_omit_the_reason_when_there_is_none() {
        let live = serde_json::to_value(MicStateEvent::new("live", None)).expect("serialisable");
        assert_eq!(live["state"], "live");
        assert!(live.get("reason").is_none());
        let failed = serde_json::to_value(MicStateEvent::new("failed", Some("denied")))
            .expect("serialisable");
        assert_eq!(failed["reason"], "denied");
    }

    #[test]
    fn the_state_event_name_is_stable() {
        // 事件名是页面（media-shim.js / bridge-contract.md）与壳的唯一约定。
        // 命令名直接就是 bridge.rs 里的函数名（`desktop_microphone_*`），没有额外常量。
        assert_eq!(EVENT_MICROPHONE_STATE, "dsh-desktop://microphone-state");
    }
}
