//! 采样格式归一化、流式重采样、定长分帧与 f32le 编码。
//!
//! AVCaptureSession 交给我们的是 `CMSampleBuffer`，里面可能是
//!   - 32-bit float 非交错（macOS 常见默认）或 16-bit 整数交错（部分 USB 声卡），
//!   - 采样率等于设备硬件率（44.1 kHz / 48 kHz / 96 kHz 都见过）。
//!
//! 页面 `AudioContext` 固定跑 48 kHz（`media-shim.js` 的 `sampleRate: 48000`），
//! 所以壳这边必须把任意输入转成「单声道 + 目标采样率 + 定长帧 + 小端 f32」。
//! 这些换算都是纯函数，能单测；真机采集（`macos.rs`）只负责把
//! `AudioBufferList` 里的字节摊平成 `&[f32]` 再交给这里。

/// 一帧有多少采样（四舍五入到最近的整数帧）。
///
/// @param sample_rate - 采样率（Hz）。
/// @param frame_millis - 帧长（毫秒）。
/// @returns 采样数；参数非法（0 或除不尽到 0）时返回 0。
pub fn frames_per_block(sample_rate: u32, frame_millis: u32) -> usize {
    if sample_rate == 0 || frame_millis == 0 {
        return 0;
    }
    // u64 中间量：192000 Hz × 100 ms 已经超过 u32 的一半，避免溢出。
    let frames = u64::from(sample_rate) * u64::from(frame_millis) / 1000;
    usize::try_from(frames).unwrap_or(0)
}

/// 把 16-bit 整数采样转成 f32（`i16::MAX` 归一化到 1.0）。
/// @param input - 交错或非交错都可以，这里只做数值换算。
/// @returns 等长的 f32 缓冲。
pub fn i16_to_f32(input: &[i16]) -> Vec<f32> {
    const SCALE: f32 = 1.0 / 32768.0;
    input
        .iter()
        .map(|sample| f32::from(*sample) * SCALE)
        .collect()
}

/// 交错多声道 → 单声道（对每个采样点取各声道平均）。
///
/// @param interleaved - 交错采样（长度必须是 `channels` 的整数倍）。
/// @param channels - 声道数；1 或 0 时原样复制。
/// @returns 单声道缓冲；长度不是整数倍时截掉尾巴。
pub fn interleaved_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / channels;
    let scale = 1.0 / channels as f32;
    let mut mono = Vec::with_capacity(frames);
    for frame in 0..frames {
        let base = frame * channels;
        let mut sum = 0.0;
        for channel in 0..channels {
            sum += interleaved[base + channel];
        }
        mono.push(sum * scale);
    }
    mono
}

/// 非交错多声道 → 单声道（每个声道一条切片）。
/// @param channels - 每条声道切片；空列表返回空。
/// @returns 单声道缓冲，长度为最短声道的长度。
pub fn planar_to_mono(channels: &[&[f32]]) -> Vec<f32> {
    if channels.is_empty() {
        return Vec::new();
    }
    if channels.len() == 1 {
        return channels[0].to_vec();
    }
    let frames = channels
        .iter()
        .map(|channel| channel.len())
        .min()
        .unwrap_or(0);
    let scale = 1.0 / channels.len() as f32;
    let mut mono = Vec::with_capacity(frames);
    for frame in 0..frames {
        let mut sum = 0.0;
        for channel in channels {
            sum += channel[frame];
        }
        mono.push(sum * scale);
    }
    mono
}

/// 流式线性插值重采样器。
///
/// 采集是连续流，不能对每一块独立重采样（块边界会引入咔哒声），所以要保留
/// 上一块最后一个采样与小数相位。线性插值对「语音 → 转写」足够；不需要
/// 窗函数 sinc（那是纯播放链路的讲究），代价是高频有轻微滚降，16 kHz 目标下无所谓。
#[derive(Clone, Debug)]
pub struct Resampler {
    from: u32,
    to: u32,
    /// 下一次输出在输入流中的位置（以输入采样为单位，相对本块起点）。
    position: f64,
    /// 上一块最后一个采样，供跨块插值使用。
    previous: f32,
    /// 是否已经收到过数据（决定 `previous` 是否有效）。
    primed: bool,
}

impl Resampler {
    /// 创建重采样器；`from == to` 时 [`Resampler::push`] 原样返回。
    /// @param from - 输入采样率。
    /// @param to - 输出采样率。
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            position: 0.0,
            previous: 0.0,
            primed: false,
        }
    }

    /// @returns 是否真的需要重采样。
    pub fn is_passthrough(&self) -> bool {
        self.from == self.to || self.from == 0 || self.to == 0
    }

    #[cfg(test)]
    /// @returns 当前小数相位（测试与诊断用）。
    pub fn position(&self) -> f64 {
        self.position
    }

    /// 喂一块输入，取出对应的输出采样。
    ///
    /// 跨块边界用「上一块末采样 + 本块首采样」插值，因此输出与输入之间存在
    /// 一个采样的固有延迟（20 ms 帧下不到 0.05 ms，可以忽略）。
    /// @param input - 单声道输入块。
    /// @returns 重采样后的输出块（可能为空）。
    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        if self.is_passthrough() {
            self.previous = *input.last().unwrap_or(&0.0);
            self.primed = true;
            return input.to_vec();
        }
        let ratio = f64::from(self.from) / f64::from(self.to);
        let mut output = Vec::with_capacity((input.len() as f64 / ratio).ceil() as usize + 1);
        // 把「上一块末采样」当作索引 -1 的虚拟点，本块首采样是索引 0。
        let sample_at = |index: i64| -> f32 {
            if index < 0 {
                if self.primed { self.previous } else { input[0] }
            } else {
                input.get(index as usize).copied().unwrap_or(0.0)
            }
        };
        loop {
            let index = self.position.floor();
            let fraction = self.position - index;
            let base = index as i64;
            let last_available = input.len() as i64 - 1;
            // 需要 base 与 base+1 两个点：越过本块末尾就停下，把相位留给下一块。
            if base + 1 > last_available {
                break;
            }
            let left = sample_at(base);
            let right = sample_at(base + 1);
            output.push(left + (right - left) * fraction as f32);
            self.position += ratio;
        }
        // 相位平移回本块的坐标系（-1 是上一块的末采样）。
        self.position -= input.len() as f64;
        self.previous = *input.last().unwrap_or(&0.0);
        self.primed = true;
        output
    }
}

/// 定长分帧器：把任意长度的采样流切成 `frame_size` 的整帧。
///
/// 最后不足一帧的尾巴留在内部，等下一次 `push` 拼上；`flush` 会把尾巴补零后吐出
/// （停止采集时用，保证页面侧不会因为半帧而卡住）。
#[derive(Clone, Debug)]
pub struct Framer {
    frame_size: usize,
    pending: Vec<f32>,
}

impl Framer {
    /// @param frame_size - 每帧采样数；0 视为不透传（`push` 直接返回空）。
    pub fn new(frame_size: usize) -> Self {
        Self {
            frame_size,
            pending: Vec::with_capacity(frame_size.max(1)),
        }
    }

    #[cfg(test)]
    /// @returns 每帧采样数。
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    #[cfg(test)]
    /// @returns 当前攒着的采样数。
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// 喂入一块采样，取出所有凑齐的整帧。
    /// @param input - 单声道采样。
    /// @returns 整帧列表（每帧长度都是 `frame_size`）。
    pub fn push(&mut self, input: &[f32]) -> Vec<Vec<f32>> {
        if self.frame_size == 0 {
            return Vec::new();
        }
        self.pending.extend_from_slice(input);
        let frames = self.pending.len() / self.frame_size;
        let mut output = Vec::with_capacity(frames);
        for index in 0..frames {
            let start = index * self.frame_size;
            output.push(self.pending[start..start + self.frame_size].to_vec());
        }
        self.pending.drain(..frames * self.frame_size);
        output
    }

    /// 把剩余的尾巴补零成一整帧（停止采集时调用）。
    /// @returns 至多一个整帧。
    pub fn flush(&mut self) -> Option<Vec<f32>> {
        if self.frame_size == 0 || self.pending.is_empty() {
            self.pending.clear();
            return None;
        }
        self.pending.resize(self.frame_size, 0.0);
        let frame = self.pending.split_off(0);
        Some(frame)
    }
}

/// 把 f32 采样编码成小端字节（`Channel` 推给页面的就是这段字节）。
/// @param samples - 单声道采样。
/// @returns `samples.len() * 4` 字节，布局与 `new Float32Array(buffer)` 一致。
pub fn encode_f32_le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// 解码小端 f32 字节（单测与诊断用；页面侧是 `new Float32Array(buffer)`）。
/// @param bytes - 4 的整数倍字节。
/// @returns 解码后的采样；长度不是 4 的倍数时忽略尾巴。
#[cfg(test)]
pub fn decode_f32_le(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成 `frames` 个采样、频率 `hz` 的正弦波（幅度 0.5）。
    fn sine(frames: usize, hz: f32, sample_rate: u32) -> Vec<f32> {
        (0..frames)
            .map(|index| {
                let phase = 2.0 * std::f32::consts::PI * hz * index as f32 / sample_rate as f32;
                0.5 * phase.sin()
            })
            .collect()
    }

    #[test]
    fn frame_sizes_match_the_page_contract() {
        assert_eq!(frames_per_block(48_000, 20), 960);
        assert_eq!(frames_per_block(48_000, 100), 4_800);
        assert_eq!(frames_per_block(16_000, 20), 320);
        assert_eq!(frames_per_block(192_000, 100), 19_200);
        assert_eq!(frames_per_block(0, 20), 0);
        assert_eq!(frames_per_block(48_000, 0), 0);
    }

    #[test]
    fn i16_samples_are_scaled_to_unit_range() {
        let input = [0, 16_384, -32_768, 32_767];
        let output = i16_to_f32(&input);
        assert_eq!(output.len(), 4);
        assert!((output[0] - 0.0).abs() < 1e-6);
        assert!((output[1] - 0.5).abs() < 1e-4);
        assert!((output[2] + 1.0).abs() < 1e-6);
        assert!(output[3] < 1.0 && output[3] > 0.999);
        assert!(i16_to_f32(&[]).is_empty());
    }

    #[test]
    fn interleaved_stereo_is_averaged() {
        let stereo = [0.5, -0.5, 0.25, 0.75, 1.0, 1.0];
        let mono = interleaved_to_mono(&stereo, 2);
        assert_eq!(mono.len(), 3);
        assert!((mono[0] - 0.0).abs() < 1e-6);
        assert!((mono[1] - 0.5).abs() < 1e-6);
        assert!((mono[2] - 1.0).abs() < 1e-6);
        // 声道数 1 / 0 原样返回。
        assert_eq!(interleaved_to_mono(&stereo, 1), stereo.to_vec());
        // 长度不是整数倍时截掉尾巴，不产生越界。
        assert_eq!(interleaved_to_mono(&[0.1, 0.2, 0.3], 2), vec![0.15]);
    }

    #[test]
    fn planar_channels_are_averaged() {
        let left = [0.4_f32, 0.2];
        let right = [0.2_f32, 0.0];
        let mono = planar_to_mono(&[&left, &right]);
        assert_eq!(mono.len(), 2);
        assert!((mono[0] - 0.3).abs() < 1e-6);
        assert!((mono[1] - 0.1).abs() < 1e-6);
        assert!(planar_to_mono(&[]).is_empty());
        assert_eq!(planar_to_mono(&[&left]).len(), 2);
    }

    #[test]
    fn passthrough_resampling_keeps_the_signal() {
        let mut resampler = Resampler::new(48_000, 48_000);
        assert!(resampler.is_passthrough());
        let input = sine(960, 440.0, 48_000);
        let output = resampler.push(&input);
        assert_eq!(output, input);
    }

    #[test]
    fn downsampling_keeps_the_length_roughly_proportional() {
        let mut resampler = Resampler::new(48_000, 16_000);
        assert!(!resampler.is_passthrough());
        let input = sine(48_000, 440.0, 48_000);
        let output = resampler.push(&input);
        // 3:1 抽取，允许因边界舍入差 1 个采样。
        assert!(
            output.len().abs_diff(16_000) <= 1,
            "expected ~16000 samples, got {}",
            output.len()
        );
        assert!(output.iter().all(|sample| sample.abs() <= 0.6));
    }

    #[test]
    fn streaming_resampling_matches_a_single_block() {
        // 分块喂与一次喂的结果必须一致（否则块边界会有咔哒声）。
        let input = sine(4_800, 440.0, 48_000);
        let mut whole = Resampler::new(48_000, 16_000);
        let expected = whole.push(&input);

        let mut chunked = Resampler::new(48_000, 16_000);
        let mut actual = Vec::new();
        for chunk in input.chunks(960) {
            actual.extend(chunked.push(chunk));
        }
        assert_eq!(actual.len(), expected.len());
        for (index, (left, right)) in actual.iter().zip(expected.iter()).enumerate() {
            assert!(
                (left - right).abs() < 1e-4,
                "sample {index} differs: {left} vs {right}"
            );
        }
    }

    #[test]
    fn upsampling_produces_more_samples() {
        let mut resampler = Resampler::new(16_000, 48_000);
        let input = sine(1_600, 440.0, 16_000);
        let output = resampler.push(&input);
        assert!(
            output.len().abs_diff(4_800) <= 2,
            "expected ~4800 samples, got {}",
            output.len()
        );
    }

    #[test]
    fn resampler_handles_empty_and_zero_rates() {
        let mut resampler = Resampler::new(48_000, 16_000);
        assert!(resampler.push(&[]).is_empty());
        assert_eq!(resampler.position(), 0.0);
        let mut broken = Resampler::new(0, 16_000);
        assert!(broken.is_passthrough());
        assert_eq!(broken.push(&[0.1, 0.2]), vec![0.1, 0.2]);
    }

    #[test]
    fn framer_emits_only_whole_frames() {
        let mut framer = Framer::new(4);
        assert!(framer.push(&[1.0, 2.0]).is_empty());
        assert_eq!(framer.pending(), 2);
        let frames = framer.push(&[3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(framer.pending(), 3);
        let tail = framer.flush().expect("tail frame");
        assert_eq!(tail, vec![5.0, 6.0, 7.0, 0.0]);
        assert!(framer.flush().is_none());
    }

    #[test]
    fn framer_with_zero_frame_size_is_inert() {
        let mut framer = Framer::new(0);
        assert!(framer.push(&[1.0, 2.0]).is_empty());
        assert_eq!(framer.frame_size(), 0);
        assert!(framer.flush().is_none());
    }

    #[test]
    fn f32_bytes_round_trip_on_little_endian_layout() {
        let samples = vec![0.0_f32, 0.5, -0.5, 1.0];
        let bytes = encode_f32_le(&samples);
        assert_eq!(bytes.len(), samples.len() * 4);
        // 0.5f32 的小端字节是 00 00 00 3F。
        assert_eq!(&bytes[4..8], &[0x00, 0x00, 0x00, 0x3f]);
        assert_eq!(decode_f32_le(&bytes), samples);
        assert!(encode_f32_le(&[]).is_empty());
        // 长度不是 4 的倍数时忽略尾巴，不 panic。
        assert_eq!(decode_f32_le(&[0, 0, 0, 0, 9]).len(), 1);
    }

    #[test]
    fn end_to_end_pipeline_produces_right_sized_frames() {
        // 44.1 kHz 非交错 → 单声道 → 48 kHz → 20 ms 帧（与真机默认情形最接近）。
        let mut resampler = Resampler::new(44_100, 48_000);
        let mut framer = Framer::new(frames_per_block(48_000, 20));
        let input = sine(44_100, 440.0, 44_100);
        let mut frames = Vec::new();
        for chunk in input.chunks(1_024) {
            frames.extend(framer.push(&resampler.push(chunk)));
        }
        if let Some(tail) = framer.flush() {
            frames.push(tail);
        }
        assert!(
            frames.len() >= 45,
            "expected ~46 frames, got {}",
            frames.len()
        );
        for frame in &frames {
            assert_eq!(frame.len(), 960);
            assert!(frame.iter().all(|sample| sample.abs() < 1.0));
        }
        // 帧时长总和应接近 1 秒。
        let total: usize = frames.iter().map(Vec::len).sum();
        assert!(
            total >= 48_000,
            "total {total} should cover one second at 48 kHz"
        );
    }
}
