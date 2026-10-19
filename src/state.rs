//! 明示的な状態機械。
//!
//! `Idle -> Recording -> Processing -> Pasting -> Idle` を基本とし、
//! 推論中でも次の発話を録音できる (リファレンスと同じく処理は直列化される)。
//! キーリピートは `Recording` 中の押下と `repeat_guard` で二重発火を防ぐ。

use std::time::{Duration, Instant};

/// アプリの明示的な状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppState {
    /// 待機中。ホットキー押下を受け付ける。
    Idle,
    /// 録音中。プリロールを含む範囲を切り出す。
    Recording,
    /// 推論待ち。ワーカーが処理する。
    Processing,
    /// クリップボード格納と Ctrl+V 送信中。
    Pasting,
}

/// ホットキー離下時の結果。
#[derive(Debug)]
pub enum ReleaseOutcome {
    /// 録音中ではなかった。
    Ignored,
    /// Esc でキャンセルされた。
    Cancelled(Duration),
    /// 最小押下時間未満だった。
    TooShort(Duration),
    /// 推論対象として確定した。
    Captured {
        start_sample: u64,
        end_sample: u64,
        duration: Duration,
    },
}

/// Push-to-Talk の状態機械。
///
/// キーリピートによる二重発火を防ぎ、推論中でも次の発話を録音できる。
#[derive(Debug)]
pub struct StateMachine {
    state: AppState,
    cancelled: bool,
    pressed_at: Option<Instant>,
    start_sample: u64,
    last_event_at: Option<Instant>,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    /// `Idle` 状態で初期化する。
    pub fn new() -> Self {
        Self {
            state: AppState::Idle,
            cancelled: false,
            pressed_at: None,
            start_sample: 0,
            last_event_at: None,
        }
    }

    /// 現在の状態。
    pub fn state(&self) -> AppState {
        self.state
    }

    /// 録音開始。キーリピート・直後の再押下は `false`。
    pub fn begin_recording(
        &mut self,
        now: Instant,
        repeat_guard: Duration,
        start_sample: u64,
    ) -> bool {
        if self.state == AppState::Recording {
            return false;
        }
        if let Some(last) = self.last_event_at
            && now.saturating_duration_since(last) < repeat_guard
        {
            // 無視した押下でタイマーを更新し、リピートが続く間は開始しない。
            self.last_event_at = Some(now);
            return false;
        }
        self.state = AppState::Recording;
        self.cancelled = false;
        self.pressed_at = Some(now);
        self.start_sample = start_sample;
        self.last_event_at = Some(now);
        true
    }

    /// 録音中の Esc。既にキャンセル済みなら `false`。
    pub fn cancel(&mut self) -> bool {
        if self.state != AppState::Recording || self.cancelled {
            return false;
        }
        self.cancelled = true;
        true
    }

    /// ホットキー離下を処理する。
    ///
    /// 録音中なら `tail_samples` を加えた範囲を返し、状態を `Processing` へ進める。
    pub fn release(
        &mut self,
        now: Instant,
        tail_samples: u64,
        position: u64,
        min_duration: Duration,
    ) -> ReleaseOutcome {
        self.last_event_at = Some(now);
        if self.state != AppState::Recording {
            return ReleaseOutcome::Ignored;
        }
        let duration = now.saturating_duration_since(self.pressed_at.unwrap_or(now));
        self.state = AppState::Idle;
        if self.cancelled {
            self.cancelled = false;
            return ReleaseOutcome::Cancelled(duration);
        }
        if duration < min_duration {
            return ReleaseOutcome::TooShort(duration);
        }
        let start_sample = self.start_sample;
        let end_sample = position.saturating_add(tail_samples);
        self.state = AppState::Processing;
        ReleaseOutcome::Captured {
            start_sample,
            end_sample,
            duration,
        }
    }

    /// Processing からのみ Pasting へ遷移する。
    /// 推論完了後、貼り付け開始時に呼ぶ。
    pub fn begin_pasting(&mut self) {
        if self.state == AppState::Processing {
            self.state = AppState::Pasting;
        }
    }

    /// Pasting からのみ Idle へ戻す。録音が始まっていたら触らない。
    pub fn finish_pasting(&mut self) {
        if self.state == AppState::Pasting {
            self.state = AppState::Idle;
        }
    }

    /// 推論・貼り付け失敗時に Processing/Pasting を Idle へ戻す。
    pub fn abort(&mut self) {
        if matches!(self.state, AppState::Processing | AppState::Pasting) {
            self.state = AppState::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn key_repeat_is_suppressed() {
        let mut machine = StateMachine::new();
        let t0 = Instant::now();
        assert!(machine.begin_recording(t0, ms(250), 0));
        // 録音中の押下 (リピート) は状態を変えない
        assert!(!machine.begin_recording(t0 + ms(30), ms(250), 0));
        assert_eq!(machine.state(), AppState::Recording);

        let released = machine.release(t0 + ms(400), 0, 6400, ms(200));
        assert!(matches!(released, ReleaseOutcome::Captured { .. }));
        assert_eq!(machine.state(), AppState::Processing);

        // 離した直後の再押下は repeat_guard で無視される
        assert!(!machine.begin_recording(t0 + ms(500), ms(250), 0));
        machine.abort();
        assert_eq!(machine.state(), AppState::Idle);
    }

    #[test]
    fn cancel_and_short_press_discard() {
        let mut machine = StateMachine::new();
        let t0 = Instant::now();
        assert!(machine.begin_recording(t0, ms(250), 100));
        assert!(machine.cancel());
        assert!(!machine.cancel());
        assert!(matches!(
            machine.release(t0 + ms(400), 0, 8000, ms(200)),
            ReleaseOutcome::Cancelled(_)
        ));
        assert_eq!(machine.state(), AppState::Idle);

        assert!(machine.begin_recording(t0 + ms(1000), ms(250), 0));
        assert!(matches!(
            machine.release(t0 + ms(1100), 0, 18000, ms(200)),
            ReleaseOutcome::TooShort(_)
        ));
        assert_eq!(machine.state(), AppState::Idle);
    }

    #[test]
    fn pasting_does_not_clobber_recording() {
        let mut machine = StateMachine::new();
        machine.state = AppState::Processing;
        machine.begin_pasting();
        assert_eq!(machine.state(), AppState::Pasting);
        machine.finish_pasting();
        assert_eq!(machine.state(), AppState::Idle);
    }
}
