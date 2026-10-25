//! macOS 原生采集：`AVCaptureSession` + `AVCaptureAudioDataOutput`。
//!
//! 这条链路存在的唯一理由：WKWebView 不实现 Media Capture。页面侧由
//! `bridge/media-shim.js` 把这里推出去的 PCM 合成成 `MediaStream`；换算与状态机在
//! [`crate::media::pcm`] / [`crate::media::session`]，本文件只负责「跟系统打交道」：
//!
//! ```text
//! AVCaptureDevice（TCC 授权）
//!   → AVCaptureDeviceInput → AVCaptureSession
//!   → AVCaptureAudioDataOutput（sample buffer 委托，跑在我们自己的串行队列上）
//!   → CMSampleBuffer → AudioBufferList（f32/i16、交错/非交错都可能）
//!   → 下混单声道 → 重采样到页面采样率 → 20 ms 定长帧 → Channel 二进制帧
//! ```
//!
//! 四个必须记住的约束（前三条来自真机 spike，见 `docs/spikes/media-input.md`）：
//!
//! 1. **授权前系统不列任何音频设备**：所以「设备清单为空」不能当成「机器没有麦克风」，
//!    必须先把 TCC 问完再判断（[`crate::media::permission::start_decision`] 处理这件事）。
//! 2. TCC 授权窗按 bundle 只弹一次；`Info.plist` 缺 `NSMicrophoneUsageDescription` 时
//!    弹窗根本不会出现，采集会一直出静音。
//! 3. 委托回调在**非主线程**的 dispatch 队列上，回调里只做「取字节 + 推帧」，
//!    不做任何窗口/UI 调用。
//! 4. `AVCaptureOutput` **不持有**委托、`setSampleBufferDelegate:queue:` 也不持有队列：
//!    两者都必须由我们保活（见 [`LiveCapture`]）。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVCaptureAudioDataOutput, AVCaptureAudioDataOutputSampleBufferDelegate, AVCaptureConnection,
    AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession, AVMediaType,
    AVMediaTypeAudio,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM,
};
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};

use crate::media::pcm::{Framer, Resampler, encode_f32_le, interleaved_to_mono, planar_to_mono};
use crate::media::permission::{
    Authorization, DeviceInfo, FailureReason, PermissionSnapshot, StartDecision, select_device,
    start_decision,
};
use crate::media::session::{Action, Machine, Signal};
use crate::media::{EVENT_MICROPHONE_STATE, MicOptions, MicStateEvent, MicStreamHeader};

/// 等用户回答 TCC 授权窗的最长时间。
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(120);
/// 委托队列名字（Instruments 里可见，便于排查丢帧/卡顿）。
const QUEUE_LABEL: &str = "dev.dsh.desktop.microphone";
/// 通道断开的轮询间隔（只在采集期间跑，开销可忽略）。
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);

/// 一次进行中的采集。
///
/// `Retained` 里的 Objective-C 对象都不是 `Send`，这里手工 `unsafe impl Send`：这些对象
/// 本身是线程安全的（引用计数原子；`AVCaptureSession` 的配置与启停可跨线程调用），而本模块
/// 只通过全局 `Mutex` 串行访问它们，所以把所有权移出/移入全局槽是安全的。
struct LiveCapture {
    session: Retained<AVCaptureSession>,
    /// 必须一起保活：`AVCaptureOutput` **不持有**委托（Apple 的文档要求调用方保活）。
    output: Retained<AVCaptureAudioDataOutput>,
    delegate: Retained<SampleDelegate>,
    /// 会话也持有输入，这里留一份是为了与委托/队列同生命周期、顺序释放。
    _input: Retained<AVCaptureDeviceInput>,
    /// `setSampleBufferDelegate:queue:` 同样不持有队列，必须自己保活。
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
}

// SAFETY: 见 `LiveCapture` 的文档注释；所有访问都在 `ACTIVE` 的互斥锁之后。
unsafe impl Send for LiveCapture {}

/// 全局唯一的采集会话（桌面端同时只允许一路采集：页面只有一个语音按钮）。
static ACTIVE: OnceLock<Mutex<Option<LiveCapture>>> = OnceLock::new();

/// 进程级会话状态机。
static MACHINE: OnceLock<Mutex<Machine>> = OnceLock::new();

/// @returns 采集槽。
fn active() -> &'static Mutex<Option<LiveCapture>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

/// @returns 状态机。
fn machine() -> &'static Mutex<Machine> {
    MACHINE.get_or_init(|| Mutex::new(Machine::new()))
}

/// 加锁但容忍毒化（采集失败不该让整条链后续全废）。
///
/// @param mutex - 目标互斥量。
/// @returns 守卫。
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// @returns `AVMediaTypeAudio` 常量（extern static，读取必须 unsafe）。
fn media_type_audio() -> &'static AVMediaType {
    // SAFETY: Foundation 在整个进程生命周期内都导出该常量。
    unsafe { AVMediaTypeAudio }.expect("AVMediaTypeAudio is always present on macOS")
}

/// 读当前 TCC 授权状态。
/// @returns 归一化后的授权状态。
pub fn authorization() -> Authorization {
    // SAFETY: 纯只读查询，任意线程可调用。
    let raw = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type_audio()) };
    Authorization::from_raw(raw.0)
}

/// 枚举音频输入设备（授权前为空，这是系统行为）。
///
/// 设备按 `uniqueID` 选，不构造 `NSString`：直接比较系统给的字符串。
///
/// 关于 `allow(deprecated)`：Apple 把这个类方法换成了 `AVCaptureDeviceDiscoverySession`，
/// 而 objc2 0.3.2 的发现会话要求调用方自己构造 `NSArray<AVCaptureDeviceType>`（`&'static`
/// 常量无法直接进数组）。这里只有一处枚举、语义完全一致，等 objc2 提供便利构造再换。
/// @returns 设备清单，默认设备带 `is_default`。
#[allow(deprecated)]
pub fn devices() -> Vec<DeviceInfo> {
    // SAFETY: 只读查询。
    let list = unsafe { AVCaptureDevice::devicesWithMediaType(media_type_audio()) };
    // SAFETY: 默认设备查询，可能返回 nil。
    let default_id = unsafe { AVCaptureDevice::defaultDeviceWithMediaType(media_type_audio()) }
        .map(|device| unsafe { device.uniqueID() }.to_string())
        .unwrap_or_default();
    let mut devices = Vec::new();
    for index in 0..list.count() {
        let device = list.objectAtIndex(index);
        let id = unsafe { device.uniqueID() }.to_string();
        devices.push(DeviceInfo {
            is_default: !default_id.is_empty() && id == default_id,
            label: unsafe { device.localizedName() }.to_string(),
            // `AVCaptureDevice.groupID` 在本版 objc2 绑定里没有导出（Apple 只在部分平台
            // 声明它）。页面只用 group 把「当前设备」与 `enumerateDevices()` 的条目对上，
            // 用 `uniqueID` 当 group 在两侧一致，因此对设备选择器无影响。
            group_id: id.clone(),
            id,
        });
    }
    devices
}

/// `desktop_microphone_status` 的实现。
/// @returns 授权状态与设备清单。
pub fn status() -> PermissionSnapshot {
    PermissionSnapshot::new(authorization(), devices())
}

/// 请求 TCC 授权并同步等待用户回答。
///
/// 只在 `notDetermined` 时调用；已有结论时系统会立刻回同一个答案。
/// @returns 是否允许采集。
fn request_access() -> bool {
    let (sender, receiver) = std::sync::mpsc::channel::<bool>();
    let block = block2::RcBlock::new(move |granted: Bool| {
        // 接收端可能已超时放弃，发送失败是正常情况。
        let _ = sender.send(granted.as_bool());
    });
    // SAFETY: 回调块在本次调用返回前一直存活；系统只会在任意队列上调用它一次。
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type_audio(), &block)
    };
    receiver.recv_timeout(PERMISSION_TIMEOUT).unwrap_or(false)
}

/// 推进状态机并发出其中的状态事件。
/// @param app - 应用句柄。
/// @param signal - 输入信号。
/// @returns 状态机给出的动作（调用方据此决定下一步）。
fn advance(app: &AppHandle, signal: Signal) -> Vec<Action> {
    let actions = machine()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .apply(signal);
    for action in &actions {
        if let Action::EmitState { state, reason } = action {
            let payload = MicStateEvent::new(state, *reason);
            if let Err(error) = app.emit(EVENT_MICROPHONE_STATE, &payload) {
                eprintln!("dsh desktop: could not publish the microphone state: {error}");
            }
        }
    }
    actions
}

/// 按 `uniqueID` 找设备。
///
/// 不用 `deviceWithUniqueID:` 是为了免掉 `objc2-foundation` 依赖（那需要现造一个
/// `NSString`）；音频输入设备最多几只，枚举匹配的开销可以忽略。
/// @param id - `AVCaptureDevice.uniqueID` 的字符串形式。
/// @returns 命中的设备；找不到时 `None`（调用方报 `bad-device`）。
#[allow(deprecated)] // 同 `devices()`：`devicesWithMediaType:` 被标废弃但语义等价
fn device_with_id(id: &str) -> Option<Retained<AVCaptureDevice>> {
    // SAFETY: 只读查询。
    let list = unsafe { AVCaptureDevice::devicesWithMediaType(media_type_audio()) };
    for index in 0..list.count() {
        let device = list.objectAtIndex(index);
        if unsafe { device.uniqueID() }.to_string() == id {
            return Some(device);
        }
    }
    None
}

/// `desktop_microphone_start` 的实现：启动采集并通过 `channel` 推 PCM。
///
/// 返回 `Ok` 时页面即可建立 `MediaStream`（JSON 头随后就到）。
/// @param app - 应用句柄。
/// @param options - 已校验的采集选项。
/// @param channel - 页面给的通道：JSON 头 + 二进制 f32 帧都从它出去。
/// @returns 失败原因串（与页面 `failureFor()` 对齐）。
pub fn start(
    app: &AppHandle,
    options: MicOptions,
    channel: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    // 先停掉可能存在的旧会话，避免两路采集抢同一只麦克风。
    stop(app);

    let decision = start_decision(authorization(), !devices().is_empty());
    advance(app, Signal::Start(decision));
    match decision {
        StartDecision::Refuse(reason) => return Err(reason.to_owned()),
        StartDecision::RequestAccess => {
            if !request_access() {
                advance(
                    app,
                    Signal::PermissionRefused(FailureReason::Denied.as_str()),
                );
                return Err(FailureReason::Denied.as_str().to_owned());
            }
            advance(app, Signal::PermissionGranted);
        }
        StartDecision::Start => {}
    }

    // 授权到位后重新枚举：notDetermined 阶段系统不报设备。
    let available = devices();
    let Some(device) = select_device(&available, &options.device_id).cloned() else {
        let reason = if options.device_id.is_empty() {
            FailureReason::DeviceMissing
        } else {
            FailureReason::BadDevice
        };
        advance(app, Signal::Failure(reason.as_str()));
        return Err(reason.as_str().to_owned());
    };

    match open_capture(&options, &device, channel) {
        Ok(live) => {
            *lock(active()) = Some(live);
            advance(app, Signal::Opened);
            watch_for_a_closed_channel(app.clone());
            Ok(())
        }
        Err(reason) => {
            advance(app, Signal::Failure(reason.as_str()));
            Err(reason.as_str().to_owned())
        }
    }
}

/// 通道断开的看门狗。
///
/// 页面重载或关窗时 JS 上下文被销毁，`track.stop()` 不会再被调用；如果壳这边不处理，
/// `AVCaptureSession` 会一直被占用（系统录音指示一直亮）。这里每 500 ms 看一次标记，
/// 发现通道已断就自己收摊；会话结束后自动退出。
/// @param app - 应用句柄（`stop` 需要它来 emit 状态事件）。
fn watch_for_a_closed_channel(app: AppHandle) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(WATCHDOG_INTERVAL);
            match channel_broken() {
                // 会话已经结束：看门狗退休。
                None => return,
                Some(true) => {
                    stop(&app);
                    return;
                }
                Some(false) => {}
            }
        }
    });
}

/// @returns 当前会话的通道是否已断；没有会话时为 `None`。
fn channel_broken() -> Option<bool> {
    let guard = lock(active());
    guard
        .as_ref()
        .map(|live| lock(&live.delegate.ivars().pipeline).broken)
}

/// 建立并启动 `AVCaptureSession`。
/// @param options - 页面选项。
/// @param device - 选中的设备。
/// @param channel - 帧通道。
/// @returns 采集句柄或失败原因。
fn open_capture(
    options: &MicOptions,
    device: &DeviceInfo,
    channel: Channel<InvokeResponseBody>,
) -> Result<LiveCapture, FailureReason> {
    // 页面用 `{ deviceId: { exact } }` 指定设备时必须真的用它：选错了不会报错，
    // 只会静默录另一只麦克风，是最难发现的一类缺陷。
    let capture_device = if device.is_default || device.id.is_empty() {
        // SAFETY: 只读查询。
        unsafe { AVCaptureDevice::defaultDeviceWithMediaType(media_type_audio()) }
            .ok_or(FailureReason::DeviceMissing)?
    } else {
        device_with_id(&device.id).ok_or(FailureReason::BadDevice)?
    };

    // SAFETY: 下面都是 AVFoundation 的配置调用；错误路径只返回 NSError。
    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&capture_device) }
        .map_err(|_| FailureReason::Busy)?;
    // SAFETY: 新建对象。
    let output = unsafe { AVCaptureAudioDataOutput::new() };
    // SAFETY: 新建对象。
    let session = unsafe { AVCaptureSession::new() };

    let delegate = SampleDelegate::new(Pipeline::new(options, device, channel));
    let queue = DispatchQueue::new(QUEUE_LABEL, None);

    // SAFETY: 会话配置必须在 begin/commit 之间；先 can_add_* 判定，避免抛 Objective-C 异常。
    unsafe {
        output.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        );
        session.beginConfiguration();
        if !session.canAddInput(&input) {
            session.commitConfiguration();
            return Err(FailureReason::Busy);
        }
        session.addInput(&input);
        if !session.canAddOutput(&output) {
            session.commitConfiguration();
            return Err(FailureReason::Unsupported);
        }
        session.addOutput(&output);
        session.commitConfiguration();
        session.startRunning();
    }

    Ok(LiveCapture {
        session,
        output,
        _input: input,
        delegate,
        _queue: queue,
    })
}

/// `desktop_microphone_stop` 的实现：停采集、摘委托、回到 idle。
/// @param app - 应用句柄。
pub fn stop(app: &AppHandle) {
    let taken = lock(active()).take();
    if let Some(live) = taken {
        // 先把不足 20 ms 的尾巴补零吐出去（否则每次停止都会丢最后几十毫秒），
        // 再摘委托、停会话，避免回调打到正在释放的队列/管线上。
        lock(&live.delegate.ivars().pipeline).flush_tail();
        // SAFETY: 摘委托必须在释放委托对象之前；`stopRunning` 可与配置并行。
        unsafe {
            live.output.setSampleBufferDelegate_queue(None, None);
            live.session.stopRunning();
        }
        drop(live);
    }
    advance(app, Signal::StopRequested);
    advance(app, Signal::Stopped);
}

/// 一次采集的帧管线：格式记忆 + 重采样 + 分帧 + 推送。
struct Pipeline {
    options: MicOptions,
    device: DeviceInfo,
    channel: Channel<InvokeResponseBody>,
    resampler: Resampler,
    framer: Framer,
    /// 首帧前先发 JSON 头（页面据此设置采样率与设备身份）。
    header_sent: bool,
    /// 已确认的源采样率；变化时重建重采样器。
    source_rate: u32,
    /// 已推出的帧数。
    frames: u64,
    /// 采样格式问题只报一次，否则每 20 ms 刷一行日志。
    complained: bool,
    /// 通道已断（页面重建 / 关窗）：后续帧直接丢弃，并通知看门狗收摊。
    broken: bool,
}

impl Pipeline {
    /// @param options - 页面选项。
    /// @param device - 选中的设备。
    /// @param channel - 帧通道。
    fn new(
        options: &MicOptions,
        device: &DeviceInfo,
        channel: Channel<InvokeResponseBody>,
    ) -> Self {
        Self {
            options: options.clone(),
            device: device.clone(),
            channel,
            resampler: Resampler::new(options.sample_rate, options.sample_rate),
            framer: Framer::new(options.frames_per_frame()),
            header_sent: false,
            source_rate: options.sample_rate,
            frames: 0,
            complained: false,
            broken: false,
        }
    }

    /// 处理一个 sample buffer：读格式、取 PCM、下混、重采样、分帧、推出去。
    /// @param sample_buffer - 委托回调收到的缓冲区。
    fn on_sample(&mut self, sample_buffer: &CMSampleBuffer) {
        let Some(description) = (unsafe { sample_buffer.format_description() }) else {
            return;
        };
        // SAFETY: 音频格式描述就是这个 typedef；非音频描述返回空指针。
        let asbd_ptr = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description) };
        if asbd_ptr.is_null() {
            return;
        }
        // SAFETY: 指针指向格式描述内部的只读结构，拷贝出来用。
        let asbd: AudioStreamBasicDescription = unsafe { *asbd_ptr };
        if asbd.mFormatID != kAudioFormatLinearPCM {
            self.complain("the capture device delivers a non-PCM format");
            return;
        }
        let channels = usize::try_from(asbd.mChannelsPerFrame).unwrap_or(0).max(1);
        let is_float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
        let is_int16 =
            asbd.mFormatFlags & kAudioFormatFlagIsSignedInteger != 0 && asbd.mBitsPerChannel == 16;
        let non_interleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
        if !is_float && !is_int16 {
            self.complain("the capture device delivers an unsupported sample format");
            return;
        }
        let source_rate = asbd.mSampleRate.max(1.0) as u32;
        if source_rate != self.source_rate {
            self.source_rate = source_rate;
            self.resampler = Resampler::new(source_rate, self.options.sample_rate);
        }

        // SAFETY: 回调期间 sample buffer 一直有效，拿到的指针只在本次回调内使用。
        let Some(buffers) = (unsafe { audio_buffers(sample_buffer, channels) }) else {
            return;
        };
        let mut planar: Vec<Vec<f32>> = Vec::with_capacity(buffers.len());
        for buffer in &buffers {
            // SAFETY: CoreMedia 保证缓冲里是 `mDataByteSize` 字节的该格式采样。
            match unsafe { samples_of(buffer, is_float) } {
                Some(samples) => planar.push(samples),
                None => {
                    self.complain("a sample buffer had an unusable length");
                    return;
                }
            }
        }
        let mono = if non_interleaved && planar.len() > 1 {
            let views: Vec<&[f32]> = planar.iter().map(Vec::as_slice).collect();
            planar_to_mono(&views)
        } else {
            interleaved_to_mono(planar.first().map(Vec::as_slice).unwrap_or(&[]), channels)
        };
        if mono.is_empty() {
            return;
        }
        let resampled = self.resampler.push(&mono);
        for frame in self.framer.push(&resampled) {
            self.send_frame(&frame);
        }
    }

    /// 推一帧给页面（首帧前先发 JSON 头）。
    /// @param frame - 单声道 f32 采样，长度等于 `frames_per_frame`。
    fn send_frame(&mut self, frame: &[f32]) {
        // 会话可能刚好在停：迟到的 sample buffer 不再推给页面。
        if !lock(machine()).accepts_frames() {
            return;
        }
        if !self.header_sent {
            self.header_sent = true;
            let header = MicStreamHeader::open(&self.options, &self.device);
            match serde_json::to_string(&header) {
                Ok(json) => {
                    if let Err(error) = self.channel.send(InvokeResponseBody::Json(json)) {
                        eprintln!("dsh desktop: microphone header not delivered: {error}");
                        return;
                    }
                }
                Err(error) => {
                    eprintln!("dsh desktop: microphone header encoding failed: {error}");
                    return;
                }
            }
        }
        if let Err(error) = self
            .channel
            .send(InvokeResponseBody::Raw(encode_f32_le(frame)))
        {
            // 通道断了（页面重建或关窗）：置位后由看门狗收回会话，这里只报一次。
            if !self.broken {
                self.broken = true;
                eprintln!("dsh desktop: microphone channel closed: {error}");
            }
            return;
        }
        self.frames += 1;
    }

    /// 把不足一帧的尾巴补零吐出（停止采集时调用）。
    fn flush_tail(&mut self) {
        if let Some(frame) = self.framer.flush() {
            self.send_frame(&frame);
        }
    }

    /// 只报一次问题。
    /// @param message - 说明。
    fn complain(&mut self, message: &str) {
        if self.complained {
            return;
        }
        self.complained = true;
        eprintln!("dsh desktop: {message}");
    }
}

/// 一个 `AudioBuffer` 的视图（借用 sample buffer 的内存，仅在回调内有效）。
struct AudioBufferView {
    data: *mut u8,
    length: usize,
    channels: usize,
}

/// 从 sample buffer 里取出 `AudioBufferList`。
///
/// `AudioBufferList` 是柔性数组（`mBuffers` 声明成 1 个元素），缓冲数量解压前未知，
/// 所以先问一次需要的字节数，再按 16 字节对齐分配（CoreAudio 要求 `mData` 16 字节对齐）。
/// 这里刻意让 `block_buffer_out` 为 null：数据在本次回调期间由 sample buffer 保活，
/// 我们立刻拷成 `Vec<f32>`，不需要额外的 `CMBlockBuffer` 引用。
/// @param sample_buffer - 委托回调的缓冲区。
/// @param channels - 声道数（作为尺寸上界）。
/// @returns 缓冲视图；失败时 `None`。
unsafe fn audio_buffers(
    sample_buffer: &CMSampleBuffer,
    channels: usize,
) -> Option<Vec<AudioBufferView>> {
    let mut size_needed: usize = 0;
    // SAFETY: 第一次只问大小（不传输出缓冲）。
    let status = unsafe {
        sample_buffer.audio_buffer_list_with_retained_block_buffer(
            &mut size_needed,
            std::ptr::null_mut(),
            0,
            None,
            None,
            0,
            std::ptr::null_mut(),
        )
    };
    let minimum = std::mem::size_of::<AudioBufferList>()
        + channels.saturating_sub(1) * std::mem::size_of::<AudioBuffer>();
    if status != 0 || size_needed < minimum {
        size_needed = minimum;
    }
    // u128 的对齐是 16 字节，正好满足 CoreAudio 的要求。
    let words = size_needed.div_ceil(16).max(1);
    let mut storage = vec![0u128; words];
    let list_ptr = storage.as_mut_ptr().cast::<AudioBufferList>();
    // SAFETY: 缓冲区按 16 字节对齐，长度足够容纳 `size_needed` 字节。
    let status = unsafe {
        sample_buffer.audio_buffer_list_with_retained_block_buffer(
            &mut size_needed,
            list_ptr,
            words * 16,
            None,
            None,
            0,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: 调用成功后 `mNumberBuffers` 已写入，`mBuffers` 紧跟其后。
    let count = unsafe { (*list_ptr).mNumberBuffers } as usize;
    let mut buffers = Vec::with_capacity(count);
    for index in 0..count {
        // SAFETY: index < mNumberBuffers，且缓冲区按柔性数组的布局分配。
        let buffer = unsafe { &*((*list_ptr).mBuffers.as_ptr().add(index)) };
        buffers.push(AudioBufferView {
            data: buffer.mData.cast::<u8>(),
            length: buffer.mDataByteSize as usize,
            channels: buffer.mNumberChannels as usize,
        });
    }
    Some(buffers)
}

/// 把一个缓冲转成 f32 采样。
/// @param buffer - 缓冲视图。
/// @param is_float - 是否 32-bit float（否则按 16-bit 有符号整数解释）。
/// @returns 采样；长度不是采样宽度的整数倍时 `None`。
unsafe fn samples_of(buffer: &AudioBufferView, is_float: bool) -> Option<Vec<f32>> {
    if buffer.data.is_null() || buffer.length == 0 {
        return Some(Vec::new());
    }
    // 声道数在交错/非交错分支里由外层决定，这里只记录（诊断用）。
    debug_assert!(buffer.channels >= 1);
    if is_float {
        if !buffer.length.is_multiple_of(4) {
            return None;
        }
        let count = buffer.length / 4;
        // SAFETY: CoreMedia 保证这块内存是 count 个 f32。
        let slice = unsafe { std::slice::from_raw_parts(buffer.data.cast::<f32>(), count) };
        Some(slice.to_vec())
    } else {
        if !buffer.length.is_multiple_of(2) {
            return None;
        }
        let count = buffer.length / 2;
        // SAFETY: CoreMedia 保证这块内存是 count 个 i16。
        let slice = unsafe { std::slice::from_raw_parts(buffer.data.cast::<i16>(), count) };
        Some(crate::media::pcm::i16_to_f32(slice))
    }
}

/// 委托的附属数据。
pub struct SampleDelegateIvars {
    pipeline: Mutex<Pipeline>,
}

define_class!(
    // SAFETY: NSObject 是唯一合法的父类；本类不实现其它特殊协议。
    #[unsafe(super(NSObject))]
    #[name = "DshMicrophoneSampleDelegate"]
    #[ivars = SampleDelegateIvars]
    pub struct SampleDelegate;

    unsafe impl NSObjectProtocol for SampleDelegate {}

    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for SampleDelegate {
        // SAFETY: 选择器与 AVFoundation 声明一致；系统只在我们给的串行队列上调用。
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn capture_output(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            lock(&self.ivars().pipeline).on_sample(sample_buffer);
        }
    }
);

impl SampleDelegate {
    /// @param pipeline - 本次采集的帧管线。
    /// @returns 需要调用方保活的委托对象。
    fn new(pipeline: Pipeline) -> Retained<Self> {
        let delegate = Self::alloc().set_ivars(SampleDelegateIvars {
            pipeline: Mutex::new(pipeline),
        });
        // SAFETY: `init` 是本类（NSObject 子类）的唯一指定初始化器。
        unsafe { msg_send![super(delegate), init] }
    }
}
