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

/// フローティングバーが参照する表示状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum VisualState {
    Idle = 0,
    Recording = 1,
    Processing = 2,
    Pasting = 3,
}

impl VisualState {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Recording,
            2 => Self::Processing,
            3 => Self::Pasting,
            _ => Self::Idle,
        }
    }
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

    /// Esc キャンセル待ちかどうか。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
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

    /// キーが既に離されている録音を、その場で破棄する。
    pub fn discard_recording(&mut self) -> bool {
        if self.state == AppState::Recording {
            self.state = AppState::Idle;
            self.cancelled = false;
            self.pressed_at = None;
            true
        } else {
            false
        }
    }
}

/// 2回押しと判定する時間窓。
pub const DOUBLE_PRESS_WINDOW: Duration = Duration::from_millis(300);

/// ホットキー押下時のハイブリッド判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressOutcome {
    /// 新規録音を開始する。
    Start,
    /// 2回目の押下。録音を継続し、トグル状態へ入る。
    ContinueToggle,
    /// トグル録音中の押下。録音を終了してペーストする。
    StopToggle,
    /// キーリピートなどで無視する。
    Ignore,
}

/// ホットキー離下時のハイブリッド判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HybridRelease {
    /// 何もしない。
    Ignore,
    /// 長押しの終了。即座に録音を確定する。
    Stop,
    /// 短い押下。2回目の押下を待つ。
    Pending(Instant),
}

/// 長押しと2回押しを同時に受け付ける判定器。
///
/// 1回目の押下で録音を始め、300ms 未満で離した場合は 2回目の押下を
/// 待つ。2回目が窓内に来ればトグル録音へ移行し、来なければ短い
/// Push-to-Talk として確定する。
#[derive(Clone, Copy, Debug, Default)]
pub struct HybridState {
    key_down: bool,
    toggle_active: bool,
    press_started: Option<Instant>,
    suppress_release: bool,
}

impl HybridState {
    pub fn on_press(
        &mut self,
        now: Instant,
        recording: bool,
        pending_deadline: Option<Instant>,
    ) -> PressOutcome {
        if self.key_down {
            return PressOutcome::Ignore;
        }
        self.key_down = true;
        self.suppress_release = false;

        if self.toggle_active {
            self.toggle_active = false;
            self.press_started = Some(now);
            self.suppress_release = true;
            return PressOutcome::StopToggle;
        }

        if recording {
            if pending_deadline.is_some_and(|deadline| now <= deadline) {
                self.toggle_active = true;
                self.press_started = Some(now);
                return PressOutcome::ContinueToggle;
            }
            self.press_started = Some(now);
            return PressOutcome::Ignore;
        }

        self.press_started = Some(now);
        PressOutcome::Start
    }

    pub fn on_release(&mut self, now: Instant, recording: bool) -> HybridRelease {
        if !self.key_down {
            return HybridRelease::Ignore;
        }
        self.key_down = false;
        if self.suppress_release {
            self.suppress_release = false;
            self.press_started = None;
            return HybridRelease::Ignore;
        }
        let Some(started) = self.press_started.take() else {
            return HybridRelease::Ignore;
        };
        if !recording || self.toggle_active {
            return HybridRelease::Ignore;
        }
        if now.saturating_duration_since(started) >= DOUBLE_PRESS_WINDOW {
            HybridRelease::Stop
        } else {
            HybridRelease::Pending(started + DOUBLE_PRESS_WINDOW)
        }
    }

    pub fn toggle_active(&self) -> bool {
        self.toggle_active
    }

    /// Esc でトグル録音を終了する。
    pub fn end_toggle(&mut self) {
        self.toggle_active = false;
        self.press_started = None;
        self.suppress_release = false;
    }

    /// 2回押し待ちを破棄して初期状態へ戻す。
    pub fn cancel_pending(&mut self) {
        *self = Self::default();
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

    #[test]
    fn hybrid_hold_stops_on_release() {
        let mut hybrid = HybridState::default();
        let t0 = Instant::now();
        assert_eq!(hybrid.on_press(t0, false, None), PressOutcome::Start);
        assert_eq!(hybrid.on_release(t0 + ms(500), true), HybridRelease::Stop);
        assert!(!hybrid.toggle_active());
    }

    #[test]
    fn hybrid_short_tap_waits_and_double_press_toggles() {
        let mut hybrid = HybridState::default();
        let t0 = Instant::now();
        assert_eq!(hybrid.on_press(t0, false, None), PressOutcome::Start);
        let deadline = t0 + DOUBLE_PRESS_WINDOW;
        assert_eq!(
            hybrid.on_release(t0 + ms(80), true),
            HybridRelease::Pending(deadline)
        );

        // 300ms 以内の2回目でトグル録音へ移行する。
        assert_eq!(
            hybrid.on_press(t0 + ms(200), true, Some(deadline)),
            PressOutcome::ContinueToggle
        );
        assert!(hybrid.toggle_active());
        assert_eq!(hybrid.on_release(t0 + ms(260), true), HybridRelease::Ignore);
        assert!(hybrid.toggle_active());

        // 3回目の押下でトグル録音を終了する。
        assert_eq!(
            hybrid.on_press(t0 + ms(1500), true, None),
            PressOutcome::StopToggle
        );
        assert_eq!(
            hybrid.on_release(t0 + ms(1520), true),
            HybridRelease::Ignore
        );
        assert!(!hybrid.toggle_active());
    }

    #[test]
    fn hybrid_second_press_after_deadline_is_a_new_start() {
        let mut hybrid = HybridState::default();
        let t0 = Instant::now();
        assert_eq!(hybrid.on_press(t0, false, None), PressOutcome::Start);
        let deadline = t0 + DOUBLE_PRESS_WINDOW;
        assert_eq!(
            hybrid.on_release(t0 + ms(80), true),
            HybridRelease::Pending(deadline)
        );
        // 期限切れの待機は Shared 側で確定される。ここでは新しい押下として扱う。
        assert_eq!(
            hybrid.on_press(t0 + ms(400), false, None),
            PressOutcome::Start
        );
    }

    #[test]
    fn cancelled_recording_can_be_discarded_without_release() {
        let mut machine = StateMachine::new();
        let t0 = Instant::now();
        assert!(machine.begin_recording(t0, ms(250), 0));
        assert!(machine.cancel());
        assert!(machine.discard_recording());
        assert_eq!(machine.state(), AppState::Idle);
        assert!(!machine.discard_recording());
    }
}
