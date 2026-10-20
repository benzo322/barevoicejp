//! 通知音の合成と再生。
//!
//! 外部ファイルを使わず、柔らかいサイン波を合成する。開始音は低めの
//! 「フワン…」、完了音は 440Hz を中心とした和音が余韻を残して消える。

use std::f32::consts::TAU;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// 再生する通知音。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    Start,
    Finish,
}

impl Cue {
    fn duration(self) -> f32 {
        match self {
            Self::Start => 0.12,
            Self::Finish => 0.55,
        }
    }
}

fn smoothstep(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    value * value * (3.0 - 2.0 * value)
}

/// 通知音を合成する。値域は -1.0..=1.0。
pub fn synthesize(cue: Cue, sample_rate: u32, volume: f32) -> Vec<f32> {
    let rate = sample_rate.max(8_000) as f32;
    let duration = cue.duration();
    let count = (duration * rate) as usize;
    let volume = volume.clamp(0.0, 1.0);
    let mut samples = Vec::with_capacity(count);
    let mut phase = 0.0f32;
    let mut second = 0.0f32;
    let mut third = 0.0f32;

    for index in 0..count {
        let t = index as f32 / rate;
        let progress = if count > 1 {
            index as f32 / (count - 1) as f32
        } else {
            0.0
        };

        let sample = match cue {
            Cue::Start => {
                // 390Hz から 355Hz へわずかに下がる、温かいサイン波。
                let frequency = 390.0 - 35.0 * progress;
                phase += TAU * frequency / rate;
                let attack = smoothstep(t / 0.030);
                let release = smoothstep(((1.0 - progress) * duration) / 0.070);
                let decay = (-1.6 * progress).exp();
                phase.sin() * attack * release * decay * volume * 0.34
            }
            Cue::Finish => {
                // A4 / C#5 / E5 の和音。少しずつ下降させ、空間に溶かす。
                phase += TAU * (440.0 - 2.0 * progress) / rate;
                second += TAU * (554.365 - 2.0 * progress) / rate;
                third += TAU * (659.255 - 3.0 * progress) / rate;
                let attack = smoothstep(t / 0.025);
                let release = smoothstep(((1.0 - progress) * duration) / 0.50);
                let decay = (-2.4 * progress).exp();
                let chord = phase.sin() * 0.55 + second.sin() * 0.27 + third.sin() * 0.18;
                chord * attack * release * decay * volume * 0.34
            }
        };
        samples.push(sample);
    }
    samples
}

/// 通知音を非同期に再生する。音声デバイスが無い場合は静かに諦める。
pub fn play(cue: Cue, volume: f32) {
    if volume <= 0.0 {
        return;
    }
    std::thread::Builder::new()
        .name("mkvoicejp-sound".to_string())
        .spawn(move || {
            let host = cpal::default_host();
            let Some(device) = host.default_output_device() else {
                return;
            };
            let Ok(config) = device.default_output_config() else {
                return;
            };
            let sample_rate = config.sample_rate().0;
            let channels = config.channels() as usize;
            let samples = synthesize(cue, sample_rate, volume);

            let stream = match config.sample_format() {
                cpal::SampleFormat::F32 => build(&device, &config, samples, channels, |v| v),
                cpal::SampleFormat::I16 => build(&device, &config, samples, channels, |v| {
                    (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                }),
                cpal::SampleFormat::U16 => build(&device, &config, samples, channels, |v| {
                    ((v.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16
                }),
                _ => None,
            };
            let Some(stream) = stream else {
                return;
            };
            if stream.play().is_err() {
                return;
            }
            std::thread::sleep(Duration::from_secs_f32(cue.duration() + 0.08));
        })
        .ok();
}

fn build<T, F>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    samples: Vec<f32>,
    channels: usize,
    convert: F,
) -> Option<cpal::Stream>
where
    T: cpal::SizedSample,
    F: Fn(f32) -> T + Send + 'static,
{
    let channels = channels.max(1);
    let mut cursor = 0usize;
    let stream_config = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    device
        .build_output_stream::<T, _, _>(
            &stream_config,
            move |output: &mut [T], _info: &cpal::OutputCallbackInfo| {
                for frame in output.chunks_mut(channels) {
                    let value = samples.get(cursor).copied().unwrap_or(0.0);
                    cursor += 1;
                    for slot in frame {
                        *slot = convert(value);
                    }
                }
            },
            |_error| {},
            None,
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_cues_are_soft_and_fade_out() {
        for cue in [Cue::Start, Cue::Finish] {
            let samples = synthesize(cue, 44_100, 0.8);
            assert!(!samples.is_empty());
            assert!(samples.iter().all(|value| value.abs() <= 0.4));
            assert!(samples[0].abs() < 1e-6);
            assert!(samples.last().copied().unwrap_or(1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn start_cue_has_a_gentle_attack() {
        let rate = 44_100u32;
        let samples = synthesize(Cue::Start, rate, 1.0);
        let at = |millis: f32| samples[(rate as f32 * millis / 1000.0) as usize].abs();
        assert!(
            at(30.0) > at(5.0) * 2.0,
            "attack should ramp smoothly: 5ms={}, 30ms={}",
            at(5.0),
            at(30.0)
        );
    }

    #[test]
    fn zero_volume_is_silent() {
        let samples = synthesize(Cue::Finish, 16_000, 0.0);
        assert!(samples.iter().all(|value| *value == 0.0));
    }
}
