//! Push-to-Talk のオーケストレーション。
//!
//! フックコールバック (メインスレッド) は状態遷移と録音範囲の決定だけを行い、
//! 推論と貼り付けは 1 本のワーカースレッドで直列化する。

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crate::audio::{AudioCapture, RingBuffer};
use crate::config::{self, Config};
use crate::engine::Engine;
use crate::history::{self, History, HistoryEntry};
use crate::models;
use crate::state::{
    AppState, HybridRelease, HybridState, PressOutcome, ReleaseOutcome, StateMachine, VisualState,
};
use crate::stats::{self, Stats};
use crate::text::{self, TextFormatter, WordEntry};
use crate::win;
use crate::{debug, error, info, sound, warn};

/// Mutex のポイズニングを無視して中身を取得する (録音は止めない)。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 確定した 1 発話のサンプル範囲。
struct Utterance {
    start_sample: u64,
    end_sample: u64,
    released_at: Instant,
    /// 録音終了時に前面だったウィンドウ。推論中に切り替わったら貼り付けない。
    target: usize,
}

/// 短い押下のあと、2回目の押下を待っている状態。
#[derive(Clone, Copy)]
struct PendingTap {
    deadline: Instant,
    released_at: Instant,
    position: u64,
}

/// ワーカースレッドへの指示。
enum Job {
    /// 推論と貼り付けを行う。
    Utterance(Utterance),
    /// 設定変更を反映してエンジンを再初期化する。
    Reconfigure,
    /// 辞書ファイルの更新を確認する。
    ///
    /// フックコールバックからファイル I/O を追い出すための非同期通知。
    CheckDictionaries,
    /// ワーカーを終了する。
    Shutdown,
}

/// ホットリロードで差し替える辞書。
#[derive(Default)]
struct Dicts {
    words: Vec<WordEntry>,
    external: Vec<(String, String)>,
}

/// ホットリロード判定用の更新時刻キャッシュ。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct FileMtimes {
    replacements: Option<SystemTime>,
    hotwords: Option<SystemTime>,
    words: Option<SystemTime>,
}

impl FileMtimes {
    fn read() -> Self {
        Self {
            replacements: text::file_mtime_ns(Path::new(text::REPLACEMENTS_PATH)),
            hotwords: text::file_mtime_ns(Path::new(text::HOTWORDS_PATH)),
            words: text::file_mtime_ns(Path::new(text::WORDS_PATH)),
        }
    }
}

/// フックコールバックとワーカーが共有する状態。
///
/// `Mutex` はどれも短時間だけ保持し、推論中にキー入力を止めない。
pub(crate) struct Shared {
    config: Mutex<Config>,
    state: Mutex<StateMachine>,
    formatter: Mutex<TextFormatter>,
    dicts: Mutex<Dicts>,
    mtimes: Mutex<FileMtimes>,
    ring: Arc<Mutex<RingBuffer>>,
    xruns: Arc<AtomicU64>,
    tx: Sender<Job>,
    hotwords_supported: AtomicBool,
    engine_ready: AtomicBool,
    engine_building: AtomicBool,
    engine_error: Mutex<Option<String>>,
    visual: Arc<AtomicU8>,
    hybrid: Mutex<HybridState>,
    pending_tap: Mutex<Option<PendingTap>>,
    /// 累積文字数・発話回数。本文は保持しない。
    stats: Mutex<Stats>,
    /// 直近の認識テキスト (新しい順)。
    history: Mutex<History>,
}

impl Shared {
    /// 辞書を読み込み、共有状態を初期化する。
    fn new(
        config: Config,
        ring: Arc<Mutex<RingBuffer>>,
        xruns: Arc<AtomicU64>,
        tx: Sender<Job>,
        hotwords_supported: bool,
        engine_ready: bool,
        engine_error: Option<String>,
    ) -> Arc<Self> {
        text::ensure_hotwords_file(Path::new(text::HOTWORDS_PATH));
        text::ensure_words_file(Path::new(text::WORDS_PATH));
        let external = text::load_external_replacements(Path::new(text::REPLACEMENTS_PATH));
        let words = text::load_words(Path::new(text::WORDS_PATH));
        let formatter = build_formatter(&config, &external, &words);
        let hotwords_count = text::count_hotwords(Path::new(text::HOTWORDS_PATH));

        if config.denoise {
            warn!(
                "ノイズリダクションを無効化して続行します: \
                 このビルドは RNNoise 非対応です"
            );
        }
        info!(
            "ハイパス: {} / ノイズリダクション: OFF",
            if config.highpass { "ON" } else { "OFF" }
        );
        info!("置換辞書: {}件読み込み", external.len());
        info!("単語辞書: {}件読み込み", words.len());

        let hotword_status = if hotwords_count == 0 {
            "OFF (0単語)".to_string()
        } else if hotwords_supported {
            format!("ON ({hotwords_count}単語)")
        } else {
            format!("OFF ({hotwords_count}単語, このエンジンは未対応)")
        };
        info!("ホットワード: {hotword_status} / ホットリロード: 有効");

        Arc::new(Self {
            config: Mutex::new(config),
            state: Mutex::new(StateMachine::new()),
            formatter: Mutex::new(formatter),
            dicts: Mutex::new(Dicts { words, external }),
            mtimes: Mutex::new(FileMtimes::read()),
            ring,
            xruns,
            tx,
            hotwords_supported: AtomicBool::new(hotwords_supported),
            engine_ready: AtomicBool::new(engine_ready),
            engine_building: AtomicBool::new(false),
            engine_error: Mutex::new(engine_error),
            visual: Arc::new(AtomicU8::new(VisualState::Idle as u8)),
            hybrid: Mutex::new(HybridState::default()),
            pending_tap: Mutex::new(None),
            stats: Mutex::new(Stats::load(Path::new(stats::STATS_PATH))),
            history: Mutex::new(History::load(Path::new(history::HISTORY_PATH))),
        })
    }

    /// 現在の設定のスナップショットを返す (UI・押下処理用)。
    pub(crate) fn snapshot(&self) -> Config {
        lock(&self.config).clone()
    }

    /// オーバーレイの表示状態を更新する。
    fn set_visual(&self, state: VisualState) {
        self.visual.store(state as u8, Ordering::SeqCst);
    }

    /// 設定画面からの変更を反映する。推論設定が変わった場合だけ
    /// ワーカーへエンジン再初期化を指示する。
    fn update_config(&self, config: Config) {
        let rebuild_engine = {
            let current = lock(&self.config);
            current.model != config.model
                || current.language != config.language
                || current.num_threads != config.num_threads
                || current.provider != config.provider
                || current.hotwords_score != config.hotwords_score
                || current.model_dir_override != config.model_dir_override
        };
        *lock(&self.config) = config;
        self.rebuild_formatter();
        if rebuild_engine {
            self.engine_ready.store(false, Ordering::SeqCst);
            let _ = self.tx.send(Job::Reconfigure);
        }
    }

    /// 辞書ファイルを即座に再読み込みする (設定画面の編集・トレイメニュー)。
    fn reload_dictionaries(&self) {
        let external = text::load_external_replacements(Path::new(text::REPLACEMENTS_PATH));
        let words = text::load_words(Path::new(text::WORDS_PATH));
        {
            let mut dicts = lock(&self.dicts);
            dicts.external = external;
            dicts.words = words;
        }
        *lock(&self.mtimes) = FileMtimes::read();
        self.rebuild_formatter();
        info!("辞書を再読み込みしました");
    }

    /// エンジン初期化の結果を共有状態へ反映する。
    fn set_engine_result(&self, result: Result<Engine, String>) -> Option<Engine> {
        self.engine_building.store(false, Ordering::SeqCst);
        match result {
            Ok(engine) => {
                self.hotwords_supported
                    .store(engine.supports_hotwords(), Ordering::SeqCst);
                self.engine_ready.store(true, Ordering::SeqCst);
                *lock(&self.engine_error) = None;
                info!("推論エンジンを初期化しました");
                Some(engine)
            }
            Err(exc) => {
                self.engine_ready.store(false, Ordering::SeqCst);
                warn!("推論エンジンを初期化できません: {exc}");
                *lock(&self.engine_error) = Some(exc);
                None
            }
        }
    }

    /// エンジンの (準備完了, 初期化中, エラー) を返す。
    pub(crate) fn engine_status(&self) -> (bool, bool, Option<String>) {
        (
            self.engine_ready.load(Ordering::SeqCst),
            self.engine_building.load(Ordering::SeqCst),
            lock(&self.engine_error).clone(),
        )
    }

    /// オーバーレイが参照する表示状態。
    pub(crate) fn visual_state(&self) -> &Arc<AtomicU8> {
        &self.visual
    }

    /// 設定画面へ渡す単語辞書 (正式表記, 読み)。
    pub(crate) fn word_entries(&self) -> Vec<(String, Option<String>)> {
        lock(&self.dicts)
            .words
            .iter()
            .map(|entry| {
                (
                    entry.target().to_string(),
                    entry.reading().map(ToString::to_string),
                )
            })
            .collect()
    }

    /// 設定画面へ渡す累積実績。
    ///
    /// 日付をまたいで最初に読まれた時点で当日分を 0 に戻す。保存は次の
    /// 貼り付け成功時に行うため、ここではファイル I/O をしない。
    pub(crate) fn stats(&self) -> Stats {
        let today = stats::local_date();
        let mut current = lock(&self.stats);
        if current.roll_day(&today) {
            debug!("統計の当日分をリセットしました: {today}");
        }
        current.clone()
    }

    /// 貼り付けに成功した 1 発話を加算し、`stats.json` へ保存する。
    ///
    /// 保存に失敗してもメモリ上の値は進め、次の成功時に再試行する。
    fn record_paste(&self, chars: u64) {
        if chars == 0 {
            return;
        }
        let today = stats::local_date();
        let snapshot = {
            let mut current = lock(&self.stats);
            current.record(chars, &today);
            current.clone()
        };
        if let Err(exc) = snapshot.save(Path::new(stats::STATS_PATH)) {
            warn!("累積実績を保存できません: {exc}");
        }
    }

    /// 設定画面へ渡す認識履歴 (新しい順)。
    pub(crate) fn history(&self) -> Vec<HistoryEntry> {
        lock(&self.history).entries.clone()
    }

    /// 認識テキストを履歴へ追加し、`history.json` を更新する。
    ///
    /// 保存に失敗してもメモリ上には残し、次の成功時に再試行する。
    fn record_history(&self, text: &str, model: &str) {
        if text.is_empty() {
            return;
        }
        let snapshot = {
            let mut current = lock(&self.history);
            current.record(HistoryEntry {
                time: history::local_datetime(),
                text: text.to_string(),
                model: model.to_string(),
            });
            current.clone()
        };
        if let Err(exc) = snapshot.save(Path::new(history::HISTORY_PATH)) {
            warn!("認識履歴を保存できません: {exc}");
        }
    }

    /// 履歴を全件削除し、`history.json` を更新する。
    pub(crate) fn clear_history(&self) -> Result<(), String> {
        let snapshot = {
            let mut current = lock(&self.history);
            current.clear();
            current.clone()
        };
        snapshot.save(Path::new(history::HISTORY_PATH))
    }

    /// 状態機械に合わせて表示状態を揃える。
    fn settle_visual(&self) {
        let state = lock(&self.state).state();
        self.set_visual(match state {
            AppState::Recording => VisualState::Recording,
            AppState::Processing => VisualState::Processing,
            AppState::Pasting => VisualState::Pasting,
            AppState::Idle => VisualState::Idle,
        });
    }

    /// ホットキー押下。辞書の更新をワーカーへ通知し、長押し/2回押しを
    /// 判定して録音を始める。
    fn on_press(&self) {
        // ファイル I/O はフックコールバック内で行わない。ここでは
        // ワーカーへ通知するだけで、読み込みは別スレッドが担う。
        self.request_dictionary_check();

        let now = Instant::now();
        self.poll_hybrid_at(now);
        let config = self.snapshot();
        let recording = lock(&self.state).state() == AppState::Recording;
        let pending_deadline = lock(&self.pending_tap).as_ref().map(|tap| tap.deadline);
        let outcome = lock(&self.hybrid).on_press(now, recording, pending_deadline);

        match outcome {
            PressOutcome::Start => {
                *lock(&self.pending_tap) = None;
                let position = lock(&self.ring).position();
                let pre_roll_samples =
                    (config.sample_rate as u64 * config.pre_roll_ms as u64) / config::MS_PER_SECOND;
                let start_sample = position.saturating_sub(pre_roll_samples);
                let repeat_guard = Duration::from_millis(config.repeat_guard_ms as u64);
                let started = lock(&self.state).begin_recording(now, repeat_guard, start_sample);
                if started {
                    info!("● 録音開始");
                    self.set_visual(VisualState::Recording);
                    if config.sound_start {
                        sound::play(config.sound_theme, sound::Cue::Start, config.sound_volume);
                    }
                } else {
                    debug!("キーリピート/直後の再押下を無視");
                }
            }
            PressOutcome::ContinueToggle => {
                *lock(&self.pending_tap) = None;
                info!("● 2回押し: トグル録音を継続");
                self.set_visual(VisualState::Recording);
            }
            PressOutcome::StopToggle => {
                *lock(&self.pending_tap) = None;
                let position = lock(&self.ring).position();
                info!("■ 2回押し: 録音終了");
                self.stop_recording_at(now, position, &config);
            }
            PressOutcome::Ignore => {}
        }
    }

    /// ホットキー離下。長押しなら録音を確定し、短押しなら2回押し待ちへ入る。
    fn on_release(&self) {
        let now = Instant::now();
        let config = self.snapshot();
        let position = lock(&self.ring).position();
        let recording = lock(&self.state).state() == AppState::Recording;
        let cancelled = lock(&self.state).is_cancelled();
        let outcome = lock(&self.hybrid).on_release(now, recording);

        match outcome {
            HybridRelease::Ignore => {}
            HybridRelease::Stop => {
                *lock(&self.pending_tap) = None;
                self.stop_recording_at(now, position, &config);
            }
            HybridRelease::Pending(_) if cancelled => {
                // Esc 済みの短押しは2回押し待ちに入らず、その場で破棄する。
                *lock(&self.pending_tap) = None;
                self.stop_recording_at(now, position, &config);
            }
            HybridRelease::Pending(deadline) => {
                *lock(&self.pending_tap) = Some(PendingTap {
                    deadline,
                    released_at: now,
                    position,
                });
                debug!("短押し: 2回押し待ち (300ms)");
            }
        }
    }

    /// 2回押し待ちの期限を確認する。UI の 50ms ティックから呼ぶ。
    pub(crate) fn poll_hybrid(&self) {
        self.poll_hybrid_at(Instant::now());
    }

    /// 2回押し待ちが期限切れなら短押しとして録音を確定する。
    fn poll_hybrid_at(&self, now: Instant) {
        let pending = {
            let mut guard = lock(&self.pending_tap);
            match *guard {
                Some(tap) if now >= tap.deadline => {
                    *guard = None;
                    Some(tap)
                }
                _ => None,
            }
        };
        let Some(pending) = pending else {
            return;
        };
        if lock(&self.hybrid).toggle_active() {
            return;
        }
        let config = self.snapshot();
        if lock(&self.state).state() == AppState::Recording {
            debug!("短押し: 2回押し待ちを確定");
            self.stop_recording_at(pending.released_at, pending.position, &config);
        }
    }

    /// 録音を終了し、状態機械の判定結果をログとワーカーへ渡す。
    fn stop_recording_at(&self, now: Instant, position: u64, config: &Config) {
        let tail_samples =
            (config.sample_rate as u64 * config.tail_grace_ms as u64) / config::MS_PER_SECOND;
        let min_duration = Duration::from_millis(config.min_duration_ms as u64);
        let outcome = lock(&self.state).release(now, tail_samples, position, min_duration);
        match outcome {
            ReleaseOutcome::Ignored => {}
            ReleaseOutcome::Cancelled(duration) => {
                self.set_visual(VisualState::Idle);
                info!(
                    "× 破棄: Esc によるキャンセル ({:.2} 秒)",
                    duration.as_secs_f64()
                );
            }
            ReleaseOutcome::TooShort(duration) => {
                self.set_visual(VisualState::Idle);
                info!(
                    "× 破棄: 録音が短すぎます ({:.0} ms < {} ms)",
                    duration.as_secs_f64() * config::MS_PER_SECOND_F64,
                    config.min_duration_ms
                );
            }
            ReleaseOutcome::Captured {
                start_sample,
                end_sample,
                duration,
            } => {
                self.set_visual(VisualState::Processing);
                info!("■ 録音終了: {:.2} 秒", duration.as_secs_f64());
                let job = Job::Utterance(Utterance {
                    start_sample,
                    end_sample,
                    released_at: now,
                    target: win::foreground_window(),
                });
                if self.tx.send(job).is_err() {
                    warn!("推論ワーカーが停止しています");
                    lock(&self.state).abort();
                    self.set_visual(VisualState::Idle);
                }
            }
        }
    }

    /// Esc。トグル録音は確定し、長押し中なら破棄する。
    fn on_cancel(&self) {
        let now = Instant::now();
        let config = self.snapshot();
        let toggle = lock(&self.hybrid).toggle_active();
        let pending = lock(&self.pending_tap).take();

        if toggle {
            lock(&self.hybrid).end_toggle();
            let position = lock(&self.ring).position();
            info!("Esc: トグル録音を終了してペースト");
            self.stop_recording_at(now, position, &config);
            return;
        }

        if pending.is_some() {
            lock(&self.hybrid).cancel_pending();
            if lock(&self.state).discard_recording() {
                self.set_visual(VisualState::Idle);
                info!("× 破棄: 2回押し待ちを Esc でキャンセル");
            }
            return;
        }

        if lock(&self.state).cancel() {
            self.set_visual(VisualState::Idle);
            info!("× キャンセル (Esc)");
        }
    }

    /// ワーカーへエンジン再初期化を依頼する。
    fn request_reconfigure(&self) {
        self.engine_ready.store(false, Ordering::SeqCst);
        self.engine_building.store(true, Ordering::SeqCst);
        if self.tx.send(Job::Reconfigure).is_err() {
            self.engine_building.store(false, Ordering::SeqCst);
        }
    }

    /// 辞書・ホットワードの更新時刻を見て、変わっていれば再読み込みする。
    fn reload_if_changed(&self) {
        let current = FileMtimes::read();
        let mut mtimes = lock(&self.mtimes);
        let mut dictionaries_changed = false;

        if current.replacements != mtimes.replacements {
            let external = text::load_external_replacements(Path::new(text::REPLACEMENTS_PATH));
            let count = external.len();
            lock(&self.dicts).external = external;
            mtimes.replacements = text::file_mtime_ns(Path::new(text::REPLACEMENTS_PATH));
            dictionaries_changed = true;
            info!("ホットリロード: 置換辞書を再読み込み ({count}件)");
        }

        if current.words != mtimes.words {
            let words = text::load_words(Path::new(text::WORDS_PATH));
            let count = words.len();
            lock(&self.dicts).words = words;
            mtimes.words = text::file_mtime_ns(Path::new(text::WORDS_PATH));
            dictionaries_changed = true;
            info!("ホットリロード: 単語辞書を再読み込み ({count}件)");
        }

        if dictionaries_changed {
            self.rebuild_formatter();
        }

        if current.hotwords != mtimes.hotwords {
            mtimes.hotwords = text::file_mtime_ns(Path::new(text::HOTWORDS_PATH));
            let count = text::count_hotwords(Path::new(text::HOTWORDS_PATH));
            info!(
                "ホットリロード: hotwords.txt を検出しましたが、\
                 現行エンジンは未対応のため反映しません ({count}単語)"
            );
        }
    }

    /// 辞書ファイルの更新確認をワーカーへ通知する。
    ///
    /// ホットキーのフックコールバックから呼ばれるため、ここでは送信だけを
    /// 行い、ファイルの読み込みはワーカーへ委ねる。
    fn request_dictionary_check(&self) {
        if self.tx.send(Job::CheckDictionaries).is_err() {
            debug!("辞書の更新確認を通知できません (ワーカー停止)");
        }
    }

    /// 辞書と設定から整形器を作り直す。
    fn rebuild_formatter(&self) {
        let config = self.snapshot();
        let formatter = {
            let dicts = lock(&self.dicts);
            build_formatter(&config, &dicts.external, &dicts.words)
        };
        *lock(&self.formatter) = formatter;
    }
}

/// 組み込み置換と外部辞書を合成して整形器を構築する。
fn build_formatter(
    config: &Config,
    external: &[(String, String)],
    words: &[WordEntry],
) -> TextFormatter {
    let replacements: Vec<(String, String)> = config
        .replacements
        .iter()
        .chain(external.iter())
        .cloned()
        .collect();
    TextFormatter::new(
        &replacements,
        words,
        config.fuzzy_threshold,
        config.fuzzy,
        config.remove_tags,
        config.remove_ja_spaces,
    )
}

/// Push-to-Talk アプリ本体。
///
/// 生成時にワーカースレッドを起動し、`run()` で設定画面とトレイを含む
/// イベントループへ入る。
pub(crate) struct App {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    ring: Arc<Mutex<RingBuffer>>,
    level: Arc<AtomicU32>,
    capture: Option<AudioCapture>,
    capture_error: Option<String>,
}

impl App {
    /// ワーカーを起動する。`engine` の所有権はワーカーへ移る。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: Config,
        engine: Option<Engine>,
        engine_error: Option<String>,
        ring: Arc<Mutex<RingBuffer>>,
        level: Arc<AtomicU32>,
        xruns: Arc<AtomicU64>,
        capture: Option<AudioCapture>,
        capture_error: Option<String>,
        hotwords_supported: bool,
    ) -> Result<Self, String> {
        let (tx, rx) = channel();
        let engine_ready = engine.is_some();
        let shared = Shared::new(
            config,
            Arc::clone(&ring),
            xruns,
            tx,
            hotwords_supported,
            engine_ready,
            engine_error,
        );
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("voice-input-worker".to_string())
            .spawn(move || worker_loop(worker_shared, engine, rx))
            .map_err(|exc| format!("推論ワーカーを開始できません: {exc}"))?;
        Ok(Self {
            shared,
            worker: Some(worker),
            ring,
            level,
            capture,
            capture_error,
        })
    }

    /// ワーカーと UI が共有する状態。
    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// マイク入力レベル (UI が毎ティック読む)。所有権は増やさない。
    pub(crate) fn level(&self) -> &Arc<AtomicU32> {
        &self.level
    }

    /// マイクを開けなかった理由。開けていれば `None`。
    pub(crate) fn capture_error(&self) -> Option<&str> {
        self.capture_error.as_deref()
    }

    /// マイクを開き直す。既存のリングバッファとレベルは再利用する。
    ///
    /// デバイス切替直後は旧ストリームの解放が完了しておらず、開き直しに
    /// 失敗することがあるため、短い間隔で数回再試行する。
    pub(crate) fn restart_capture(&mut self, config: &Config) -> Result<(), String> {
        self.capture = None;
        self.level.store(0.0f32.to_bits(), Ordering::Relaxed);
        let mut last_error = None;
        for attempt in 0..3 {
            match AudioCapture::start(config, Arc::clone(&self.ring), Arc::clone(&self.level)) {
                Ok(capture) => {
                    self.capture = Some(capture);
                    self.capture_error = None;
                    return Ok(());
                }
                Err(exc) => {
                    last_error = Some(exc);
                    if attempt < 2 {
                        thread::sleep(Duration::from_millis(140));
                    }
                }
            }
        }
        let exc = last_error.unwrap_or_else(|| "マイクを開けませんでした".to_string());
        self.capture_error = Some(exc.clone());
        Err(exc)
    }

    /// 設定画面の変更を反映する (必要ならエンジンを再初期化)。
    pub(crate) fn apply_config(&self, config: Config) {
        self.shared.update_config(config);
    }

    /// モデル変更後にエンジンを作り直す。
    pub(crate) fn reconfigure(&self) {
        self.shared.request_reconfigure();
    }

    /// 2回押し待ちの期限を確認する (UI ティックから呼ぶ)。
    pub(crate) fn poll_hybrid(&self) {
        self.shared.poll_hybrid();
    }

    /// 辞書ファイルを即座に再読み込みする。
    pub(crate) fn reload_dictionaries(&self) {
        self.shared.reload_dictionaries();
    }

    /// ワーカーとマイクを停止する。UI 終了時に一度だけ呼ぶ。
    pub(crate) fn shutdown(&mut self) {
        self.capture = None;
        let _ = self.shared.tx.send(Job::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        info!("終了します...");
        crate::logging::flush();
    }

    /// キーボードフックを設置し、設定画面のイベントループへ入る。
    pub(crate) fn run(mut self) -> ! {
        let config = self.shared.snapshot();
        let hotkey = match win::parse_hotkey(&config.hotkey) {
            Ok(hotkey) => hotkey,
            Err(exc) => {
                error!("{exc}");
                self.shutdown();
                std::process::exit(1);
            }
        };

        let press_shared = Arc::clone(&self.shared);
        let release_shared = Arc::clone(&self.shared);
        let cancel_shared = Arc::clone(&self.shared);
        let handler = win::HookHandler::new(
            hotkey,
            Box::new(move || press_shared.on_press()),
            Box::new(move || release_shared.on_release()),
            Box::new(move || cancel_shared.on_cancel()),
        );

        let _guard = match win::install(handler) {
            Ok(guard) => guard,
            Err(exc) => {
                error!("{exc}");
                self.shutdown();
                std::process::exit(1);
            }
        };

        info!(
            "準備完了: ホットキー={} / キャンセル=Esc / トレイから設定を開けます",
            config.hotkey
        );
        crate::ui::run(self)
    }
}

/// 推論・貼り付けを直列化するワーカーループ。
fn worker_loop(shared: Arc<Shared>, mut engine: Option<Engine>, rx: Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        match job {
            Job::Shutdown => break,
            Job::Reconfigure => {
                shared.engine_building.store(true, Ordering::SeqCst);
                let config = shared.snapshot();
                engine = shared.set_engine_result(Engine::build(&config));
            }
            Job::CheckDictionaries => shared.reload_if_changed(),
            Job::Utterance(item) => match engine.as_ref() {
                Some(engine) => process(engine, &shared, &item),
                None => {
                    warn!(
                        "モデルが未準備のため推論できません。設定画面からダウンロードしてください。"
                    );
                    lock(&shared.state).abort();
                    shared.settle_visual();
                }
            },
        }
    }
}

/// 1 発話を推論し、整形してアクティブウィンドウへ貼り付ける。
fn process(engine: &Engine, shared: &Shared, item: &Utterance) {
    let config = shared.snapshot();
    // デバイスバッファの遅れを考慮し、tail 分が届くまで少し待つ。
    let deadline = Instant::now() + Duration::from_millis(config::WORKER_WAIT_TIMEOUT_MS);
    loop {
        let position = lock(&shared.ring).position();
        if position >= item.end_sample || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(config::WORKER_POLL_INTERVAL_MS));
    }

    let mut audio = {
        let ring = lock(&shared.ring);
        ring.read(item.start_sample, item.end_sample)
    };
    if audio.is_empty() {
        warn!("音声データを取得できませんでした");
        lock(&shared.state).abort();
        shared.settle_visual();
        return;
    }
    let xruns = shared.xruns.load(Ordering::Relaxed);
    if xruns > 0 {
        debug!("オーディオ xruns: {xruns}");
    }

    if config.highpass {
        let started = Instant::now();
        crate::audio::highpass(
            &mut audio,
            config.sample_rate as f64,
            config::HIGHPASS_CUTOFF_HZ,
        );
        debug!(
            "前処理時間: {:.1} ms",
            started.elapsed().as_secs_f64() * config::MS_PER_SECOND_F64
        );
    }

    let peak = audio
        .iter()
        .fold(0.0f32, |acc, sample| acc.max(sample.abs()));
    if peak < config::SILENCE_PEAK_THRESHOLD {
        warn!("マイク入力がほぼ無音です (デバイス設定を確認してください)");
    }

    let started = Instant::now();
    let raw_text = engine.transcribe(&audio);
    let infer_ms = started.elapsed().as_secs_f64() * config::MS_PER_SECOND_F64;
    let rms = (audio.iter().map(|sample| sample * sample).sum::<f32>() / audio.len() as f32).sqrt();
    info!(
        "推論時間: {:.1} ms (音声 {:.2} 秒, RMS {:.4})",
        infer_ms,
        audio.len() as f64 / config.sample_rate as f64,
        rms
    );
    // 認識結果そのものはログへ残さない (音声内容の漏えいを防ぐ)。
    debug!("生の認識結果: {} 文字", raw_text.chars().count());

    let formatted = lock(&shared.formatter).format(&raw_text);
    if formatted.is_empty() {
        info!("認識結果: (空) — ペーストしません");
        lock(&shared.state).abort();
        shared.settle_visual();
        return;
    }
    let chars = stats::char_count(&formatted);
    info!("認識結果: {chars} 文字 (本文はログに残しません)");
    shared.record_history(&formatted, models::model_name(&config.model));

    lock(&shared.state).begin_pasting();
    shared.settle_visual();
    let pasted = paste(&config, &formatted, item.released_at, item.target);
    lock(&shared.state).finish_pasting();
    shared.settle_visual();
    if pasted {
        // 貼り付けが成立した発話だけを実績に数える。
        shared.record_paste(chars);
    }
    if pasted && config.sound_finish {
        sound::play(config.sound_theme, sound::Cue::Finish, config.sound_volume);
    }
}

/// クリップボードへ格納し、Ctrl+V を 1 回送る。
///
/// `restore_clipboard` が有効なら貼り付け後に元の内容へ戻す。
fn paste(config: &Config, text: &str, released_at: Instant, target: usize) -> bool {
    let original = if config.restore_clipboard {
        match win::clipboard_get() {
            Ok(original) => Some(original),
            Err(exc) => {
                warn!("クリップボードの退避をスキップ: {exc}");
                None
            }
        }
    } else {
        None
    };

    if let Err(exc) = win::clipboard_set(text) {
        error!("クリップボードへ書き込めません: {exc}");
        // 書き込みに失敗したときは、退避した元の内容を戻す。
        if let Some(original) = original.as_deref()
            && let Err(exc) = win::clipboard_set(original)
        {
            warn!("退避したクリップボードを復元できません: {exc}");
        }
        return false;
    }

    // 推論中にユーザーが別のウィンドウへ切り替えた場合、そのまま Ctrl+V を
    // 送ると意図しないアプリへ文章を注入してしまう。録音終了時の前面
    // ウィンドウが変わっていたら送信せず、クリップボードにだけ残す。
    thread::sleep(Duration::from_millis(config.paste_delay_ms as u64));
    if win::foreground_window() != target {
        warn!(
            "貼り付け先のウィンドウが変わったため Ctrl+V を送りません\
             （認識結果はクリップボードに残しました）"
        );
        return false;
    }
    let paste_ok = match win::send_ctrl_v() {
        Ok(()) => {
            info!(
                "ペースト完了 (キーを離してから {:.0} ms)",
                released_at.elapsed().as_secs_f64() * config::MS_PER_SECOND_F64
            );
            true
        }
        Err(exc) => {
            error!("SendInput による Ctrl+V 送信に失敗しました: {exc}");
            false
        }
    };

    if config.restore_clipboard {
        if !paste_ok {
            warn!("貼り付けに失敗したため、認識結果をクリップボードに残します");
        } else if let Some(original) = original {
            thread::sleep(Duration::from_millis(
                config.clipboard_restore_delay_ms as u64,
            ));
            match win::clipboard_set(&original) {
                Ok(()) => debug!("クリップボードを復元しました"),
                Err(exc) => warn!("クリップボードの復元に失敗: {exc}"),
            }
        } else {
            debug!("元クリップボードが無いため復元しません");
        }
    }
    paste_ok
}
