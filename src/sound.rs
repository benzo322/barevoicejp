//! 通知音の合成と再生。
//!
//! 4 つのテーマを、外部ファイルなしのサイン波で合成する。
//! `soft` は環境音のような余韻、`pop` は明るいメリハリ、`chime` は
//! クリスタルなベル、`subtle` は短く控えめなタップ音。

use std::f32::consts::TAU;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::config::SoundTheme;

/// 再生する通知音。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cue {
    Start,
    Finish,
}

/// テーマと種類ごとの長さ。
fn duration(theme: SoundTheme, cue: Cue) -> f32 {
    match (theme, cue) {
        (SoundTheme::Soft, Cue::Start) => 0.45,
        (SoundTheme::Soft, Cue::Finish) => 0.95,
        (SoundTheme::Pop, Cue::Start) => 0.14,
        (SoundTheme::Pop, Cue::Finish) => 0.22,
        (SoundTheme::Chime, Cue::Start) => 0.45,
        (SoundTheme::Chime, Cue::Finish) => 0.80,
        (SoundTheme::Subtle, Cue::Start) => 0.05,
        (SoundTheme::Subtle, Cue::Finish) => 0.06,
    }
}

/// 0–1 を滑らかに補間する。
fn smoothstep(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    value * value * (3.0 - 2.0 * value)
}

/// ハン窓の前半分。両端で傾きが 0 になる、輪郭の立たないアタック。
fn hann_attack(t: f32, seconds: f32) -> f32 {
    let value = (t / seconds.max(0.001)).clamp(0.0, 1.0);
    0.5 - 0.5 * (std::f32::consts::PI * value).cos()
}

/// サンプル位置を 0–1 の進捗へ変換する。
fn progress(index: usize, count: usize) -> f32 {
    if count > 1 {
        index as f32 / (count - 1) as f32
    } else {
        0.0
    }
}

/// 末尾 `seconds` 秒で 0 へ向かうリリース係数。
fn release(progress: f32, duration: f32, seconds: f32) -> f32 {
    smoothstep(((1.0 - progress) * duration) / seconds)
}

/// 通知音を合成する。値域は -1.0..=1.0。
fn synthesize(theme: SoundTheme, cue: Cue, sample_rate: u32, volume: f32) -> Vec<f32> {
    let rate = sample_rate.max(8_000) as f32;
    let duration = duration(theme, cue);
    let count = (duration * rate) as usize;
    let volume = volume.clamp(0.0, 1.0);
    let mut samples = Vec::with_capacity(count);
    let mut phase = 0.0f32;
    let mut second = 0.0f32;
    let mut third = 0.0f32;

    for index in 0..count {
        let t = index as f32 / rate;
        let p = progress(index, count);

        let sample = match theme {
            SoundTheme::Soft => match cue {
                Cue::Start => {
                    // ピッチを動かさず、70ms のハン窓でふわっと立ち上げる。
                    phase += TAU * 264.0 / rate;
                    let attack = hann_attack(t, 0.070);
                    phase.sin() * attack * release(p, duration, 0.22) * volume * 0.30
                }
                Cue::Finish => {
                    phase += TAU * 247.0 / rate;
                    let attack = hann_attack(t, 0.075);
                    phase.sin() * attack * release(p, duration, 0.62) * volume * 0.28
                }
            },
            SoundTheme::Pop => match cue {
                Cue::Start => {
                    let frequency = 880.0 + 520.0 * p;
                    phase += TAU * frequency / rate;
                    let attack = smoothstep(t / 0.004);
                    let tone = phase.sin() + (phase * 2.0).sin() * 0.18;
                    tone * attack * release(p, duration, 0.045) * (-7.0 * p).exp() * volume * 0.30
                }
                Cue::Finish => {
                    let frequency = 620.0 - 180.0 * p;
                    phase += TAU * frequency / rate;
                    let attack = smoothstep(t / 0.005);
                    let tone = phase.sin() + (phase * 2.0).sin() * 0.20;
                    tone * attack * release(p, duration, 0.060) * (-8.0 * p).exp() * volume * 0.34
                }
            },
            SoundTheme::Chime => match cue {
                Cue::Start => {
                    phase += TAU * 1046.5 / rate;
                    second += TAU * 1568.0 / rate;
                    let attack = smoothstep(t / 0.003);
                    let bell =
                        phase.sin() * 0.60 + second.sin() * 0.30 + (phase * 2.0).sin() * 0.10;
                    bell * attack * release(p, duration, 0.18) * (-4.5 * p).exp() * volume * 0.30
                }
                Cue::Finish => {
                    phase += TAU * 1318.5 / rate;
                    second += TAU * 1975.5 / rate;
                    third += TAU * 1661.2 / rate;
                    let attack = smoothstep(t / 0.005);
                    let bell = phase.sin() * 0.45 + second.sin() * 0.30 + third.sin() * 0.25;
                    bell * attack * release(p, duration, 0.35) * (-3.2 * p).exp() * volume * 0.26
                }
            },
            SoundTheme::Subtle => match cue {
                Cue::Start => {
                    phase += TAU * 220.0 / rate;
                    let attack = smoothstep(t / 0.002);
                    phase.sin()
                        * attack
                        * release(p, duration, 0.025)
                        * (-12.0 * p).exp()
                        * volume
                        * 0.12
                }
                Cue::Finish => {
                    phase += TAU * 320.0 / rate;
                    let attack = smoothstep(t / 0.002);
                    phase.sin()
                        * attack
                        * release(p, duration, 0.030)
                        * (-11.0 * p).exp()
                        * volume
                        * 0.10
                }
            },
        };
        samples.push(sample);
    }
    samples
}

/// 通知音を非同期に再生する。音声デバイスが無い場合は静かに諦める。
pub(crate) fn play(theme: SoundTheme, cue: Cue, volume: f32) {
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
            let samples = synthesize(theme, cue, sample_rate, volume);

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
            std::thread::sleep(Duration::from_secs_f32(duration(theme, cue) + 0.08));
        })
        .ok();
}

/// サンプル列をデバイスの形式へ変換して再生するストリームを作る。
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

    const THEMES: [SoundTheme; 4] = [
        SoundTheme::Soft,
        SoundTheme::Pop,
        SoundTheme::Chime,
        SoundTheme::Subtle,
    ];

    fn peak(samples: &[f32]) -> f32 {
        samples
            .iter()
            .fold(0.0f32, |max, value| max.max(value.abs()))
    }

    #[test]
    fn all_themes_fade_in_and_out() {
        for theme in THEMES {
            for cue in [Cue::Start, Cue::Finish] {
                let samples = synthesize(theme, cue, 44_100, 0.8);
                assert!(!samples.is_empty(), "{theme:?} {cue:?}");
                assert!(peak(&samples) <= 0.4, "{theme:?} {cue:?}");
                assert!(samples[0].abs() < 1e-6, "{theme:?} {cue:?}");
                assert!(
                    samples.last().copied().unwrap_or(1.0).abs() < 1e-6,
                    "{theme:?} {cue:?}"
                );
            }
        }
    }

    #[test]
    fn subtle_is_quieter_than_pop() {
        let subtle = synthesize(SoundTheme::Subtle, Cue::Start, 44_100, 1.0);
        let pop = synthesize(SoundTheme::Pop, Cue::Start, 44_100, 1.0);
        assert!(peak(&subtle) < peak(&pop));
    }

    #[test]
    fn soft_start_has_a_gentle_attack() {
        let rate = 44_100u32;
        let samples = synthesize(SoundTheme::Soft, Cue::Start, rate, 1.0);
        let at = |millis: f32| samples[(rate as f32 * millis / 1000.0) as usize].abs();
        assert!(
            at(30.0) > at(5.0) * 2.0,
            "attack should ramp smoothly: 5ms={}, 30ms={}",
            at(5.0),
            at(30.0)
        );
        assert!((hann_attack(0.035, 0.070) - 0.5).abs() < 0.01);
        assert!(hann_attack(0.069, 0.070) > 0.99);
    }

    #[test]
    fn soft_finish_is_a_steady_low_tone() {
        let rate = 44_100u32;
        let samples = synthesize(SoundTheme::Soft, Cue::Finish, rate, 1.0);
        let crossings = |start_ms: u32, end_ms: u32| {
            let start = (rate * start_ms / 1000) as usize;
            let end = (rate * end_ms / 1000) as usize;
            samples[start..end]
                .windows(2)
                .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
                .count()
        };
        let first = crossings(100, 200);
        let second = crossings(200, 300);
        assert!((22..=27).contains(&first), "first window={first}");
        assert!((22..=27).contains(&second), "second window={second}");
        assert!(
            first.abs_diff(second) <= 1,
            "pitch should not slide: {first} vs {second}"
        );
    }

    #[test]
    fn zero_volume_is_silent() {
        let samples = synthesize(SoundTheme::Soft, Cue::Finish, 16_000, 0.0);
        assert!(samples.iter().all(|value| *value == 0.0));
    }
}
