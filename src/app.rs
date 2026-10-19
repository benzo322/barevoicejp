//! Push-to-Talk のオーケストレーション。
//!
//! フックコールバック (メインスレッド) は状態遷移と録音範囲の決定だけを行い、
//! 推論と貼り付けは 1 本のワーカースレッドで直列化する。

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crate::audio::RingBuffer;
use crate::config::{self, Config};
use crate::engine::Engine;
use crate::state::{AppState, ReleaseOutcome, StateMachine};
use crate::text::{self, TextFormatter, WordEntry};
use crate::win;
use crate::{debug, error, info, warn};

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
}

/// ワーカースレッドへの指示。
enum Job {
    /// 推論と貼り付けを行う。
    Utterance(Utterance),
    /// hotwords.txt の変更を反映してエンジンを再初期化する。
    ReloadHotwords,
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
struct Shared {
    config: Config,
    state: Mutex<StateMachine>,
    formatter: Mutex<TextFormatter>,
    dicts: Mutex<Dicts>,
    mtimes: Mutex<FileMtimes>,
    ring: Arc<Mutex<RingBuffer>>,
    xruns: Arc<AtomicU64>,
    tx: Sender<Job>,
    hotwords_supported: AtomicBool,
    sample_rate: u32,
    pre_roll_samples: u64,
    tail_samples: u64,
    min_duration: Duration,
    repeat_guard: Duration,
    paste_delay: Duration,
    restore_delay: Duration,
    restore_clipboard: bool,
    highpass: bool,
}

impl Shared {
    /// 辞書を読み込み、時間・サンプル単位の値を事前計算する。
    fn new(
        config: Config,
        ring: Arc<Mutex<RingBuffer>>,
        xruns: Arc<AtomicU64>,
        tx: Sender<Job>,
        hotwords_supported: bool,
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

        let sample_rate = config.sample_rate;
        let pre_roll_samples =
            (sample_rate as u64 * config.pre_roll_ms as u64) / config::MS_PER_SECOND;
        let tail_samples =
            (sample_rate as u64 * config.tail_grace_ms as u64) / config::MS_PER_SECOND;
        let min_duration = Duration::from_millis(config.min_duration_ms as u64);
        let repeat_guard = Duration::from_millis(config.repeat_guard_ms as u64);
        let paste_delay = Duration::from_millis(config.paste_delay_ms as u64);
        let restore_delay = Duration::from_millis(config.clipboard_restore_delay_ms as u64);

        Arc::new(Self {
            state: Mutex::new(StateMachine::new()),
            formatter: Mutex::new(formatter),
            dicts: Mutex::new(Dicts { words, external }),
            mtimes: Mutex::new(FileMtimes::read()),
            ring,
            xruns,
            tx,
            hotwords_supported: AtomicBool::new(hotwords_supported),
            sample_rate,
            pre_roll_samples,
            tail_samples,
            min_duration,
            repeat_guard,
            paste_delay,
            restore_delay,
            restore_clipboard: config.restore_clipboard,
            highpass: config.highpass,
            config,
        })
    }

    fn on_press(&self) {
        {
            let state = lock(&self.state);
            if state.state() == AppState::Recording {
                return;
            }
        }
        // 録音開始前に設定・辞書の変更を確認する。
        self.reload_if_changed();

        let now = Instant::now();
        let position = lock(&self.ring).position();
        let start_sample = position.saturating_sub(self.pre_roll_samples);
        let started = lock(&self.state).begin_recording(now, self.repeat_guard, start_sample);
        if started {
            info!("● 録音開始");
        } else {
            debug!("キーリピート/直後の再押下を無視");
        }
    }

    fn on_release(&self) {
        let now = Instant::now();
        let position = lock(&self.ring).position();
        let outcome =
            lock(&self.state).release(now, self.tail_samples, position, self.min_duration);
        match outcome {
            ReleaseOutcome::Ignored => {}
            ReleaseOutcome::Cancelled(duration) => {
                info!(
                    "× 破棄: Esc によるキャンセル ({:.2} 秒)",
                    duration.as_secs_f64()
                );
            }
            ReleaseOutcome::TooShort(duration) => {
                info!(
                    "× 破棄: 録音が短すぎます ({:.0} ms < {} ms)",
                    duration.as_secs_f64() * config::MS_PER_SECOND_F64,
                    self.config.min_duration_ms
                );
            }
            ReleaseOutcome::Captured {
                start_sample,
                end_sample,
                duration,
            } => {
                info!("■ 録音終了: {:.2} 秒", duration.as_secs_f64());
                let job = Job::Utterance(Utterance {
                    start_sample,
                    end_sample,
                    released_at: now,
                });
                if self.tx.send(job).is_err() {
                    warn!("推論ワーカーが停止しています");
                    lock(&self.state).abort();
                }
            }
        }
    }

    fn on_cancel(&self) {
        if lock(&self.state).cancel() {
            info!("× キャンセル (Esc)");
        }
    }

    fn reload_if_changed(&self) {
        let current = FileMtimes::read();
        let mut mtimes = lock(&self.mtimes);
        let mut dictionaries_changed = false;

        if current.replacements != mtimes.replacements {
            let external = text::load_external_replacements(Path::new(text::REPLACEMENTS_PATH));
            lock(&self.dicts).external = external.clone();
            mtimes.replacements = text::file_mtime_ns(Path::new(text::REPLACEMENTS_PATH));
            dictionaries_changed = true;
            info!(
                "ホットリロード: 置換辞書を再読み込み ({}件)",
                external.len()
            );
        }

        if current.words != mtimes.words {
            let words = text::load_words(Path::new(text::WORDS_PATH));
            lock(&self.dicts).words = words.clone();
            mtimes.words = text::file_mtime_ns(Path::new(text::WORDS_PATH));
            dictionaries_changed = true;
            info!("ホットリロード: 単語辞書を再読み込み ({}件)", words.len());
        }

        if dictionaries_changed {
            self.rebuild_formatter();
        }

        if current.hotwords != mtimes.hotwords {
            mtimes.hotwords = text::file_mtime_ns(Path::new(text::HOTWORDS_PATH));
            let count = text::count_hotwords(Path::new(text::HOTWORDS_PATH));
            info!("ホットリロード: ホットワードを再読み込み ({}単語)", count);
            if self.hotwords_supported.load(Ordering::Relaxed) {
                let _ = self.tx.send(Job::ReloadHotwords);
            }
        }
    }

    fn rebuild_formatter(&self) {
        let (external, words) = {
            let dicts = lock(&self.dicts);
            (dicts.external.clone(), dicts.words.clone())
        };
        let formatter = build_formatter(&self.config, &external, &words);
        *lock(&self.formatter) = formatter;
    }
}

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
        config.remove_tags,
        config.remove_ja_spaces,
    )
}

/// Push-to-Talk アプリ本体。
///
/// 生成時にワーカースレッドを起動し、`run()` でキーボードフックの
/// メッセージループへ入る。
pub struct App {
    shared: Arc<Shared>,
    worker: JoinHandle<()>,
}

impl App {
    /// ワーカーを起動する。`engine` の所有権はワーカーへ移る。
    pub fn new(
        config: Config,
        engine: Engine,
        ring: Arc<Mutex<RingBuffer>>,
        xruns: Arc<AtomicU64>,
        hotwords_supported: bool,
    ) -> Result<Self, String> {
        let (tx, rx) = channel();
        let shared = Shared::new(config, ring, xruns, tx, hotwords_supported);
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("voice-input-worker".to_string())
            .spawn(move || worker_loop(worker_shared, engine, rx))
            .map_err(|exc| format!("推論ワーカーを開始できません: {exc}"))?;
        Ok(Self { shared, worker })
    }

    /// キーボードフックを設置してメッセージループを回す。
    ///
    /// Ctrl+C でループを抜け、ワーカーを停止して終了コードを返す。
    pub fn run(self) -> i32 {
        let Self { shared, worker } = self;

        let hotkey = match win::parse_hotkey(&shared.config.hotkey) {
            Ok(hotkey) => hotkey,
            Err(exc) => {
                error!("{exc}");
                let _ = shared.tx.send(Job::Shutdown);
                let _ = worker.join();
                return 1;
            }
        };

        let press_shared = Arc::clone(&shared);
        let release_shared = Arc::clone(&shared);
        let cancel_shared = Arc::clone(&shared);
        let handler = win::HookHandler::new(
            hotkey,
            Box::new(move || press_shared.on_press()),
            Box::new(move || release_shared.on_release()),
            Box::new(move || cancel_shared.on_cancel()),
        );

        let guard = match win::install(handler) {
            Ok(guard) => guard,
            Err(exc) => {
                error!("{exc}");
                let _ = shared.tx.send(Job::Shutdown);
                let _ = worker.join();
                return 1;
            }
        };

        info!(
            "準備完了: ホットキー={}（押している間だけ録音） / キャンセル=Esc / 終了=Ctrl+C",
            shared.config.hotkey
        );
        win::run_message_loop();

        drop(guard);
        let _ = shared.tx.send(Job::Shutdown);
        let _ = worker.join();
        info!("終了します...");
        0
    }
}

/// 推論・貼り付けを直列化するワーカーループ。
fn worker_loop(shared: Arc<Shared>, mut engine: Engine, rx: Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        match job {
            Job::Shutdown => break,
            Job::ReloadHotwords => {
                if !shared.hotwords_supported.load(Ordering::Relaxed) {
                    continue;
                }
                match Engine::build(&shared.config) {
                    Ok(new_engine) => {
                        shared
                            .hotwords_supported
                            .store(new_engine.supports_hotwords(), Ordering::Relaxed);
                        engine = new_engine;
                        info!("ホットリロード: 推論エンジンを再初期化しました");
                    }
                    Err(exc) => warn!(
                        "ホットリロード: エンジン再初期化に失敗しました ({exc})。\
                         旧エンジンで継続します。"
                    ),
                }
            }
            Job::Utterance(item) => process(&engine, &shared, &item),
        }
    }
}

/// 1 発話を推論し、整形してアクティブウィンドウへ貼り付ける。
fn process(engine: &Engine, shared: &Shared, item: &Utterance) {
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
        return;
    }
    let xruns = shared.xruns.load(Ordering::Relaxed);
    if xruns > 0 {
        debug!("オーディオ xruns: {xruns}");
    }

    if shared.highpass {
        let started = Instant::now();
        crate::audio::highpass(
            &mut audio,
            shared.sample_rate as f64,
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
        audio.len() as f64 / shared.sample_rate as f64,
        rms
    );
    debug!("生の認識結果: {raw_text:?}");

    let formatted = lock(&shared.formatter).format(&raw_text);
    if formatted.is_empty() {
        info!("認識結果: (空) — ペーストしません");
        lock(&shared.state).abort();
        return;
    }
    info!("認識結果: {formatted}");

    lock(&shared.state).begin_pasting();
    paste(shared, &formatted, item.released_at);
    lock(&shared.state).finish_pasting();
}

/// クリップボードへ格納し、Ctrl+V を 1 回送る。
///
/// `restore_clipboard` が有効なら貼り付け後に元の内容へ戻す。
fn paste(shared: &Shared, text: &str, released_at: Instant) {
    let original = if shared.restore_clipboard {
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
        return;
    }

    thread::sleep(shared.paste_delay);
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

    if shared.restore_clipboard {
        if !paste_ok {
            warn!("貼り付けに失敗したため、認識結果をクリップボードに残します");
        } else if let Some(original) = original {
            thread::sleep(shared.restore_delay);
            match win::clipboard_set(&original) {
                Ok(()) => debug!("クリップボードを復元しました"),
                Err(exc) => warn!("クリップボードの復元に失敗: {exc}"),
            }
        } else {
            debug!("元クリップボードが無いため復元しません");
        }
    }
}
