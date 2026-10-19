//! 音声キャプチャ (cpal) と固定長リングバッファ。
//!
//! 常時 16kHz / mono / f32 で書き込み、押下時のサンプル位置から
//! プリロール分だけ巻き戻して切り出す。デバイスが 16kHz を直接
//! 受け付けない場合のみ、既定設定で開いて線形リサンプルする。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::config::{AUDIO_SCRATCH_SAMPLES, AUDIO_STREAM_TIMEOUT_MS, Config, MIN_RING_SECONDS};

/// リサンプル時に余分に確保するサンプル数。
const RESAMPLER_RESERVE_SAMPLES: usize = 2;

// ---------------------------------------------------------------------------
// リングバッファ
// ---------------------------------------------------------------------------
/// 常時書き込みの固定長リングバッファ。
///
/// `position()` は累積サンプル数を返すため、押下位置からの巻き戻しに使える。
pub struct RingBuffer {
    buffer: Vec<f32>,
    total: u64,
}

impl RingBuffer {
    /// 指定サンプル数 (最低 1) のバッファを作る。
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![0.0; capacity.max(1)],
            total: 0,
        }
    }

    /// これまでに書き込んだ総サンプル数。
    pub fn position(&self) -> u64 {
        self.total
    }

    /// 末尾にサンプルを追加する。容量を超えた分は古いものから捨てる。
    pub fn push(&mut self, samples: &[f32]) {
        let incoming = samples.len();
        if incoming == 0 {
            return;
        }
        let capacity = self.buffer.len();
        // 溢れた分は捨て、最後の capacity サンプルだけを保持する。
        let kept = if incoming > capacity {
            &samples[incoming - capacity..]
        } else {
            samples
        };
        let n = kept.len();
        let start = ((self.total + incoming as u64 - n as u64) % capacity as u64) as usize;
        let first = n.min(capacity - start);
        self.buffer[start..start + first].copy_from_slice(&kept[..first]);
        if n > first {
            self.buffer[..n - first].copy_from_slice(&kept[first..]);
        }
        self.total += incoming as u64;
    }

    /// 絶対サンプル番号 `[start, end)` をコピーして返す。
    /// 保持範囲外は切り詰める (足りない場合は警告ログ)。
    pub fn read(&self, start_sample: u64, end_sample: u64) -> Vec<f32> {
        let capacity = self.buffer.len();
        let oldest = self.total.saturating_sub(capacity as u64);
        let start = start_sample.max(oldest);
        let end = end_sample.min(self.total);
        if end <= start {
            return Vec::new();
        }
        let truncated = start != start_sample || end != end_sample;
        let count = (end - start) as usize;
        let mut out = vec![0.0f32; count];
        let index = (start % capacity as u64) as usize;
        let first = count.min(capacity - index);
        out[..first].copy_from_slice(&self.buffer[index..index + first]);
        if count > first {
            out[first..].copy_from_slice(&self.buffer[..count - first]);
        }
        if truncated {
            crate::warn!(
                "音声バッファ範囲が不足 (要求 {}..{} / 利用可能 {}..{})",
                start_sample,
                end_sample,
                oldest,
                self.total
            );
        }
        out
    }
}

// ---------------------------------------------------------------------------
// リサンプラ (16kHz を直接開けなかった場合のみ使用)
// ---------------------------------------------------------------------------
/// 入力を出力レートへ線形補間するストリーミングリサンプラ。
struct LinearResampler {
    step: f64,
    position: f64,
    previous: f32,
    started: bool,
}

impl LinearResampler {
    fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            step: input_rate as f64 / output_rate as f64,
            position: 0.0,
            previous: 0.0,
            started: false,
        }
    }

    fn process_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        if input.is_empty() {
            return;
        }
        let length = input.len() as f64;
        out.reserve((input.len() as f64 / self.step) as usize + RESAMPLER_RESERVE_SAMPLES);
        while self.position < length {
            let index = self.position.floor() as usize;
            let fraction = (self.position - index as f64) as f32;
            let (a, b) = if index == 0 {
                let a = if self.started {
                    self.previous
                } else {
                    input[0]
                };
                (a, input[0])
            } else {
                (input[index - 1], input[index])
            };
            out.push(a + (b - a) * fraction);
            self.position += self.step;
        }
        self.position -= length;
        self.previous = *input.last().unwrap_or(&0.0);
        self.started = true;
    }
}

// ---------------------------------------------------------------------------
// キャプチャ本体
// ---------------------------------------------------------------------------
/// マイク入力ストリームと共有リングバッファ。
///
/// `cpal::Stream` は `Send` ではないため、この構造体はメインスレッドで保持する。
/// ワーカーは [`AudioCapture::ring`] の `Arc` 経由でサンプルを読む。
pub struct AudioCapture {
    ring: Arc<Mutex<RingBuffer>>,
    xruns: Arc<AtomicU64>,
    stream: cpal::Stream,
}

impl AudioCapture {
    /// 既定 (または `--device` 指定) の入力デバイスを開いて録音を開始する。
    pub fn start(config: &Config) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = select_device(&host, config.device.as_deref())?;
        let name = device.name().unwrap_or_else(|_| "?".to_string());

        let chosen = choose_config(&device, config.sample_rate)?;
        let input_rate = chosen.sample_rate().0;
        let channels = chosen.channels() as usize;
        let sample_format = chosen.sample_format();

        crate::info!(
            "マイク: [{}] {} ({} Hz, {} ch, {:?})",
            config.device.as_deref().unwrap_or("default"),
            name,
            input_rate,
            channels,
            sample_format
        );
        if input_rate != config.sample_rate {
            crate::warn!(
                "{} Hz を直接開けないため、{} Hz で取得して {} Hz へリサンプルします",
                config.sample_rate,
                input_rate,
                config.sample_rate
            );
        }

        let capacity =
            (config.sample_rate as f32 * config.ring_seconds).max(MIN_RING_SECONDS) as usize;
        let ring = Arc::new(Mutex::new(RingBuffer::new(capacity)));
        let xruns = Arc::new(AtomicU64::new(0));

        let spec = cpal::StreamConfig {
            channels: chosen.channels(),
            sample_rate: chosen.sample_rate(),
            buffer_size: cpal::BufferSize::Fixed(config.block_size),
        };

        let layout = StreamLayout {
            channels,
            input_rate,
            output_rate: config.sample_rate,
        };
        let stream = match build_for_format(&device, &spec, sample_format, layout, &ring, &xruns) {
            Ok(stream) => stream,
            Err(exc) => {
                crate::debug!(
                    "固定ブロックサイズ {} frames で開始できないため既定値を使います: {exc}",
                    config.block_size
                );
                let spec = cpal::StreamConfig {
                    buffer_size: cpal::BufferSize::Default,
                    ..spec
                };
                build_for_format(&device, &spec, sample_format, layout, &ring, &xruns).map_err(
                    |exc| {
                        format!(
                            "マイク入力ストリームを開始できません。\n\
                             \x20 16kHz / mono / float32 に対応したデバイスか確認してください。\n\
                             \x20 --list-devices で別デバイスを確認し、--device N を指定できます。\n\
                             \x20 詳細: {exc}"
                        )
                    },
                )?
            }
        };

        stream
            .play()
            .map_err(|exc| format!("録音ストリームを開始できません: {exc}"))?;
        crate::info!(
            "録音ストリーム開始 ({} Hz, mono, float32)",
            config.sample_rate
        );

        Ok(Self {
            ring,
            xruns,
            stream,
        })
    }

    /// 共有リングバッファ。ワーカースレッドへ渡す。
    pub fn ring(&self) -> Arc<Mutex<RingBuffer>> {
        Arc::clone(&self.ring)
    }

    /// ストリームエラー回数。デバッグログ用。
    pub fn xruns(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.xruns)
    }

    /// ストリームを停止する。
    pub fn stop(self) {
        drop(self.stream);
    }
}

fn choose_config(device: &cpal::Device, rate: u32) -> Result<cpal::SupportedStreamConfig, String> {
    if let Ok(ranges) = device.supported_input_configs() {
        for range in ranges {
            if range.channels() == 1
                && range.sample_format() == cpal::SampleFormat::F32
                && range.min_sample_rate().0 <= rate
                && rate <= range.max_sample_rate().0
            {
                return Ok(range.with_sample_rate(cpal::SampleRate(rate)));
            }
        }
    }
    device
        .default_input_config()
        .map_err(|exc| format!("入力デバイスの既定設定を取得できません: {exc}"))
}

fn select_device(host: &cpal::Host, spec: Option<&str>) -> Result<cpal::Device, String> {
    let Some(spec) = spec else {
        return host.default_input_device().ok_or_else(|| {
            "入力デバイスが見つかりません。--list-devices で確認してください。".to_string()
        });
    };
    let spec = spec.trim();

    if let Ok(index) = spec.parse::<usize>() {
        let devices: Vec<cpal::Device> = host
            .devices()
            .map_err(|exc| format!("入力デバイス一覧を取得できません: {exc}"))?
            .collect();
        return devices.into_iter().nth(index).ok_or_else(|| {
            format!("デバイス番号 {index} が見つかりません。--list-devices で確認してください。")
        });
    }

    let needle = spec.to_lowercase();
    let mut matches = Vec::new();
    let devices = host
        .input_devices()
        .map_err(|exc| format!("入力デバイス一覧を取得できません: {exc}"))?;
    for device in devices {
        let Ok(name) = device.name() else {
            continue;
        };
        if name.to_lowercase().contains(&needle) {
            matches.push((name, device));
        }
    }
    if matches.is_empty() {
        return Err(format!(
            "名前に '{spec}' を含む入力デバイスがありません。--list-devices で確認してください。"
        ));
    }
    if matches.len() > 1 {
        crate::warn!(
            "デバイス名が複数一致しました。最初のものを使います: {}",
            matches
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(matches.remove(0).1)
}

/// 入力ストリームのチャンネル数とサンプルレート。
#[derive(Clone, Copy)]
struct StreamLayout {
    channels: usize,
    input_rate: u32,
    output_rate: u32,
}

/// デバイスのサンプル形式に応じて入力ストリームを構築する。
fn build_for_format(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    layout: StreamLayout,
    ring: &Arc<Mutex<RingBuffer>>,
    xruns: &Arc<AtomicU64>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    match format {
        cpal::SampleFormat::F32 => build_typed::<f32>(device, config, layout, ring, xruns),
        cpal::SampleFormat::I16 => build_typed::<i16>(device, config, layout, ring, xruns),
        cpal::SampleFormat::U16 => build_typed::<u16>(device, config, layout, ring, xruns),
        cpal::SampleFormat::I32 => build_typed::<i32>(device, config, layout, ring, xruns),
        cpal::SampleFormat::F64 => build_typed::<f64>(device, config, layout, ring, xruns),
        _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
    }
}

/// サンプル型 `T` を f32 に変換し、必要なら出力レートへリサンプルしてリングへ書く。
fn build_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    layout: StreamLayout,
    ring: &Arc<Mutex<RingBuffer>>,
    xruns: &Arc<AtomicU64>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let ring = Arc::clone(ring);
    let error_xruns = Arc::clone(xruns);
    let channels = layout.channels.max(1);
    let mut resampler = (layout.input_rate != layout.output_rate)
        .then(|| LinearResampler::new(layout.input_rate, layout.output_rate));
    let mut mono: Vec<f32> = Vec::with_capacity(AUDIO_SCRATCH_SAMPLES);
    let mut resampled: Vec<f32> = Vec::with_capacity(AUDIO_SCRATCH_SAMPLES);

    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _info: &cpal::InputCallbackInfo| {
            mono.clear();
            mono.reserve(data.len() / channels + 1);
            let mut index = 0;
            while index < data.len() {
                mono.push(<f32 as cpal::Sample>::from_sample(data[index]));
                index += channels;
            }
            let mut ring = ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match &mut resampler {
                Some(resampler) => {
                    resampler.process_into(&mono, &mut resampled);
                    ring.push(&resampled);
                }
                None => ring.push(&mono),
            }
        },
        move |err| {
            error_xruns.fetch_add(1, Ordering::Relaxed);
            crate::debug!("オーディオストリームエラー: {err}");
        },
        Some(Duration::from_millis(AUDIO_STREAM_TIMEOUT_MS)),
    )
}

// ---------------------------------------------------------------------------
// 前処理
// ---------------------------------------------------------------------------
/// 2 次バターワースハイパス (scipy の `butter(2, cutoff, "highpass")` 相当)。
///
/// 発話ごとに状態をリセットして適用する (フィルタはステートレスに呼ぶ)。
pub fn highpass(samples: &mut [f32], sample_rate: f64, cutoff_hz: f64) {
    if samples.is_empty() {
        return;
    }
    let q = std::f64::consts::FRAC_1_SQRT_2;
    let w0 = 2.0 * std::f64::consts::PI * cutoff_hz / sample_rate;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * q);
    let a0 = 1.0 + alpha;
    let b0 = ((1.0 + cos) / 2.0) / a0;
    let b1 = (-(1.0 + cos)) / a0;
    let b2 = ((1.0 + cos) / 2.0) / a0;
    let a1 = (-2.0 * cos) / a0;
    let a2 = (1.0 - alpha) / a0;

    let mut z1 = 0.0f64;
    let mut z2 = 0.0f64;
    for sample in samples {
        let x = *sample as f64;
        let y = b0 * x + z1;
        z1 = b1 * x - a1 * y + z2;
        z2 = b2 * x - a2 * y;
        *sample = y as f32;
    }
}

// ---------------------------------------------------------------------------
// デバイス一覧
// ---------------------------------------------------------------------------
/// 入力可能なデバイスを列挙して標準出力へ表示する。
///
/// 戻り値はプロセス終了コード (1 台以上見つかれば 0)。
pub fn list_input_devices() -> i32 {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|device| device.name().ok());
    let devices: Vec<cpal::Device> = match host.devices() {
        Ok(devices) => devices.collect(),
        Err(exc) => {
            crate::error!("オーディオデバイスを取得できません: {exc}");
            return 1;
        }
    };

    println!("入力デバイス:");
    let mut found = false;
    for (index, device) in devices.iter().enumerate() {
        let Ok(ranges) = device.supported_input_configs() else {
            continue;
        };
        let ranges: Vec<_> = ranges.collect();
        if ranges.is_empty() {
            continue;
        }
        found = true;
        let name = device.name().unwrap_or_else(|_| "?".to_string());
        let channels = ranges
            .iter()
            .map(|range| range.channels())
            .max()
            .unwrap_or(0);
        let rate = device
            .default_input_config()
            .map(|config| config.sample_rate().0)
            .unwrap_or(0);
        let mark = if default_name.as_deref() == Some(name.as_str()) {
            "*"
        } else {
            " "
        };
        println!("{mark} {index:2}: {name} (input ch={channels}, default={rate} Hz)");
    }
    if !found {
        println!("  (入力デバイスが見つかりません)");
    }
    println!("  * = 既定の入力デバイス");
    if found { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_wraps_and_truncates() {
        let mut ring = RingBuffer::new(8);
        ring.push(&[1.0, 2.0, 3.0]);
        assert_eq!(ring.position(), 3);
        assert_eq!(ring.read(0, 3), vec![1.0, 2.0, 3.0]);

        ring.push(&[4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]);
        assert_eq!(ring.position(), 10);
        // 10 - 8 = 2 が最古
        assert_eq!(
            ring.read(0, 10),
            vec![3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]
        );
        assert_eq!(ring.read(8, 10), vec![9.0, 10.0]);

        ring.push(&(0..16).map(|value| value as f32).collect::<Vec<_>>());
        assert_eq!(ring.position(), 26);
        assert_eq!(
            ring.read(18, 26),
            vec![8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0]
        );
    }

    #[test]
    fn resampler_preserves_length_roughly() {
        let mut resampler = LinearResampler::new(48_000, 16_000);
        let input: Vec<f32> = (0..4800).map(|index| index as f32).collect();
        let mut out = Vec::new();
        resampler.process_into(&input, &mut out);
        assert!((out.len() as i64 - 1600).abs() <= 1);
    }

    #[test]
    fn highpass_removes_dc() {
        let mut samples = vec![0.5f32; 16_000];
        highpass(&mut samples, 16_000.0, 80.0);
        let tail = &samples[8_000..];
        let mean = tail.iter().sum::<f32>() / tail.len() as f32;
        assert!(mean.abs() < 1e-3, "mean={mean}");
    }
}
