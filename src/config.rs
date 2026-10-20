//! 起動設定と CLI。
//!
//! `voice_input.py` の `CONFIG` と `build_arg_parser` / `apply_cli_overrides` を
//! そのまま Rust へ移植したもの。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// モデルディレクトリ名。`models/` 配下の展開先と一致させる。
const SENSEVOICE_DIR_NAME: &str = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17";
const REAZONSPEECH_DIR_NAME: &str = "sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01";
const WHISPER_BASE_DIR_NAME: &str = "sherpa-onnx-whisper-base";
const WHISPER_SMALL_DIR_NAME: &str = "sherpa-onnx-whisper-small";
const WHISPER_TURBO_DIR_NAME: &str = "sherpa-onnx-whisper-large-v3-turbo";

/// UI と CLI が受け付けるモデル ID。
pub(crate) const MODEL_IDS: [&str; 5] = [
    "sensevoice",
    "reazonspeech",
    "whisper-base",
    "whisper-small",
    "whisper-turbo",
];

/// モデル ID に対応する展開先ディレクトリ名。
pub(crate) fn model_dir_name(model: &str) -> &'static str {
    match model {
        "reazonspeech" => REAZONSPEECH_DIR_NAME,
        "whisper-base" => WHISPER_BASE_DIR_NAME,
        "whisper-small" => WHISPER_SMALL_DIR_NAME,
        "whisper-turbo" => WHISPER_TURBO_DIR_NAME,
        _ => SENSEVOICE_DIR_NAME,
    }
}

/// 設定ファイル名 (カレントディレクトリ基準)。
pub(crate) const CONFIG_PATH: &str = "config.toml";

// ---------------------------------------------------------------------------
// 内部チューニング定数
//
// ユーザーが CLI / `Config` から変更する値は `Config` に置き、ここには
// 実装上の固定値 (モデル仕様・無音判定・再試行回数など) を集約する。
// ---------------------------------------------------------------------------
/// 推論モデルが要求する特徴量次元。
pub(crate) const FEATURE_DIM: i32 = 80;
/// greedy/beam 探索で保持するパス数。
pub(crate) const MAX_ACTIVE_PATHS: i32 = 4;
/// これ未満のピーク振幅は無音とみなす。
pub(crate) const SILENCE_PEAK_THRESHOLD: f32 = 1e-4;
/// ハイパスフィルタのカットオフ周波数 (Hz)。
pub(crate) const HIGHPASS_CUTOFF_HZ: f64 = 80.0;
/// tail サンプルの到着を待つ上限 (ms)。
pub(crate) const WORKER_WAIT_TIMEOUT_MS: u64 = 2_000;
/// tail 待ちのポーリング間隔 (ms)。
pub(crate) const WORKER_POLL_INTERVAL_MS: u64 = 5;
/// クリップボードを開く際の再試行回数。
pub(crate) const CLIPBOARD_OPEN_ATTEMPTS: u32 = 10;
/// クリップボード再試行の間隔 (ms)。
pub(crate) const CLIPBOARD_OPEN_RETRY_MS: u64 = 10;
/// cpal のストリームエラーコールバック待ちタイムアウト (ms)。
pub(crate) const AUDIO_STREAM_TIMEOUT_MS: u64 = 1_000;
/// 音声コールバックの一時バッファ初期容量 (サンプル数)。
pub(crate) const AUDIO_SCRATCH_SAMPLES: usize = 8_192;
/// ファジーマッチ対象にする読み・正式表記の最小文字数。
pub(crate) const FUZZY_MIN_CHARS: usize = 4;
/// 読みが長い単語でしきい値を緩和する文字数。
pub(crate) const FUZZY_LENIENT_MIN_CHARS: usize = 6;
/// 読みが長い単語の緩和しきい値 (%)。
pub(crate) const FUZZY_LENIENT_THRESHOLD: f64 = 75.0;
/// ファジー照合で許容する読みとの長さ差 (文字)。
pub(crate) const FUZZY_LENGTH_SLACK: usize = 2;
/// リングバッファ長の下限 (秒)。
pub(crate) const MIN_RING_SECONDS: f32 = 1.0;
/// ミリ秒と秒の換算。
pub(crate) const MS_PER_SECOND: u64 = 1_000;
/// 浮動小数で扱うミリ秒換算。
pub(crate) const MS_PER_SECOND_F64: f64 = MS_PER_SECOND as f64;

/// `models/` のルートを探す。
///
/// リファレンスはスクリプトと同じディレクトリの `models/` を使う。
/// Rust 版はビルド時のマニフェスト位置 → exe の隣 → カレントディレクトリの順に
/// 実在するものを採用する。どこにも無ければマニフェスト位置 (開発時) か
/// カレントディレクトリを使う。
pub(crate) fn models_root() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(3);
    if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("models"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("models"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("models"));
    }
    if let Some(found) = candidates.into_iter().find(|path| path.exists()) {
        return found;
    }
    // どこにも無ければ exe の隣に作る。自動起動時もカレントディレクトリに
    // 依存せず、モデル・辞書・設定を同じ場所へ置ける。
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        return dir.join("models");
    }
    PathBuf::from("models")
}

/// データルート (models/ の親) をカレントディレクトリにする。
///
/// Windows の自動起動ではカレントディレクトリが System32 などになるため、
/// `config.toml` / `words.txt` / `models/` を exe の隣で一貫して扱う。
pub(crate) fn use_data_root() {
    let Some(root) = models_root().parent().map(Path::to_path_buf) else {
        return;
    };
    if root.is_dir()
        && let Err(exc) = std::env::set_current_dir(&root)
    {
        crate::warn!(
            "データディレクトリへ移動できません ({}): {exc}",
            root.display()
        );
    }
}

/// 既定のモデルディレクトリを探す。
fn default_model_dir(name: &str) -> PathBuf {
    models_root().join(name)
}

/// 通知音のテーマ。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SoundTheme {
    /// アンビエントな「フワン…」と和音フェード。
    #[default]
    Soft,
    /// 明るくメリハリのある「ピコッ」「ポンッ」。
    Pop,
    /// 透明感のあるベル・チャイム。
    Chime,
    /// ごく控えめな短いタップ音。
    Subtle,
}

impl SoundTheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Pop => "pop",
            Self::Chime => "chime",
            Self::Subtle => "subtle",
        }
    }

    /// 設定文字列をテーマへ変換する。不明な値は `Soft`。
    pub(crate) fn parse(value: &str) -> Self {
        match value {
            "pop" => Self::Pop,
            "chime" => Self::Chime,
            "subtle" => Self::Subtle,
            _ => Self::Soft,
        }
    }
}

/// 実行時の全設定。
///
/// 既定値は `Default` に集約している。CLI で指定された項目だけを
/// [`Config::apply_cli`] が上書きする。キー割り当て・録音長・しきい値などを
/// 変える場合はまずこの構造体を編集する。
#[derive(Clone, Debug)]
pub(crate) struct Config {
    // --- モデル ---
    pub(crate) model: String,
    pub(crate) model_dir_override: Option<PathBuf>,
    pub(crate) sensevoice_model_dir: PathBuf,
    pub(crate) reazonspeech_model_dir: PathBuf,
    pub(crate) whisper_base_model_dir: PathBuf,
    pub(crate) whisper_small_model_dir: PathBuf,
    pub(crate) whisper_turbo_model_dir: PathBuf,
    pub(crate) num_threads: i32,
    pub(crate) provider: String,
    pub(crate) language: String,
    pub(crate) use_itn: bool,
    pub(crate) hotwords_score: f32,

    // --- 録音 / ホットキー ---
    /// 押している間だけ録音するキー名 (`alt_r`, `f9`, `ctrl_r` など)。
    pub(crate) hotkey: String,
    pub(crate) paste_modifier: String,
    pub(crate) paste_key: String,
    pub(crate) sample_rate: u32,
    pub(crate) block_size: u32,
    /// 押下直前の音声を巻き戻して含める長さ (ms)。
    pub(crate) pre_roll_ms: u32,
    /// 離した直後の取りこぼしを防ぐため、余分に取得する長さ (ms)。
    pub(crate) tail_grace_ms: u32,
    /// これ未満の押下は破棄する (ms)。
    pub(crate) min_duration_ms: u32,
    /// 離下直後の再押下・キーリピートを無視する時間 (ms)。
    pub(crate) repeat_guard_ms: u32,
    pub(crate) ring_seconds: f32,
    pub(crate) device: Option<String>,

    // --- UI / 通知音 ---
    pub(crate) autostart: bool,
    pub(crate) sound_start: bool,
    pub(crate) sound_finish: bool,
    pub(crate) sound_volume: f32,
    pub(crate) sound_theme: SoundTheme,
    pub(crate) floating_bar: bool,

    // --- 音響前処理 ---
    pub(crate) highpass: bool,
    pub(crate) denoise: bool,

    // --- 貼り付け / クリップボード ---
    /// クリップボード設定後、Ctrl+V を送るまでの待機 (ms)。
    pub(crate) paste_delay_ms: u32,
    pub(crate) restore_clipboard: bool,
    pub(crate) clipboard_restore_delay_ms: u32,

    // --- テキスト整形 ---
    pub(crate) remove_tags: bool,
    pub(crate) remove_ja_spaces: bool,
    /// ファジーマッチを有効にする (既定 OFF)。
    pub(crate) fuzzy: bool,
    /// words.txt のファジーマッチしきい値 (%)。
    pub(crate) fuzzy_threshold: f64,
    /// 組み込みの完全一致置換 (replacements.json が優先)。
    pub(crate) replacements: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "sensevoice".to_string(),
            model_dir_override: None,
            sensevoice_model_dir: default_model_dir(SENSEVOICE_DIR_NAME),
            reazonspeech_model_dir: default_model_dir(REAZONSPEECH_DIR_NAME),
            whisper_base_model_dir: default_model_dir(WHISPER_BASE_DIR_NAME),
            whisper_small_model_dir: default_model_dir(WHISPER_SMALL_DIR_NAME),
            whisper_turbo_model_dir: default_model_dir(WHISPER_TURBO_DIR_NAME),
            num_threads: 2,
            provider: "cpu".to_string(),
            language: "ja".to_string(),
            use_itn: true,
            hotwords_score: 2.0,

            hotkey: "alt_r".to_string(),
            paste_modifier: "ctrl".to_string(),
            paste_key: "v".to_string(),
            sample_rate: 16_000,
            block_size: 256,
            pre_roll_ms: 200,
            tail_grace_ms: 80,
            min_duration_ms: 200,
            repeat_guard_ms: 250,
            ring_seconds: 60.0,
            device: None,

            autostart: false,
            sound_start: true,
            sound_finish: true,
            sound_volume: 0.55,
            sound_theme: SoundTheme::Soft,
            floating_bar: true,

            highpass: false,
            denoise: false,

            paste_delay_ms: 50,
            restore_clipboard: false,
            clipboard_restore_delay_ms: 300,

            remove_tags: true,
            remove_ja_spaces: true,
            fuzzy: false,
            fuzzy_threshold: 80.0,
            replacements: default_replacements(),
        }
    }
}

/// Python の dict リテラルと同じ順序を保つ。
fn default_replacements() -> Vec<(String, String)> {
    [
        ("えーぴーあい", "API"),
        ("えーぴーアイ", "API"),
        ("エーピーアイ", "API"),
        ("えーあい", "AI"),
        ("エーアイ", "AI"),
        ("じーぴーてぃー", "GPT"),
        ("ジーピーティー", "GPT"),
        ("えるえるえむ", "LLM"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect()
}

impl Config {
    /// hotwords.txt が存在する場合のみ推論エンジンへ渡す。
    pub(crate) fn hotwords_file(&self) -> Option<PathBuf> {
        let path = PathBuf::from(crate::text::HOTWORDS_PATH);
        path.is_file().then_some(path)
    }

    /// 選択中のモデルに対応するディレクトリを返す。
    /// `--model-dir` が指定されていればそれを優先する。
    pub(crate) fn model_dir_for(&self, model: &str) -> PathBuf {
        if let Some(dir) = &self.model_dir_override {
            return dir.clone();
        }
        self.default_model_dir_for(model)
    }

    /// `--model-dir` を無視した、モデル管理用の既定ディレクトリ。
    pub(crate) fn default_model_dir_for(&self, model: &str) -> PathBuf {
        match model {
            "reazonspeech" => self.reazonspeech_model_dir.clone(),
            "whisper-base" => self.whisper_base_model_dir.clone(),
            "whisper-small" => self.whisper_small_model_dir.clone(),
            "whisper-turbo" => self.whisper_turbo_model_dir.clone(),
            _ => self.sensevoice_model_dir.clone(),
        }
    }
}

/// `config.toml` に保存する UI 設定。
///
/// 起動時はこの構造体を [`Config`] に反映し、CLI が指定された項目だけを
/// さらに上書きする。設定画面での変更は即座に保存・反映される。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// 初回起動のオンボーディングを表示するか。未設定の既存ファイルでも true。
    pub(crate) first_run: bool,
    pub(crate) model: String,
    pub(crate) hotkey: String,
    pub(crate) autostart: bool,
    pub(crate) device: Option<String>,
    pub(crate) language: String,
    pub(crate) num_threads: i32,
    pub(crate) sound_start: bool,
    pub(crate) sound_finish: bool,
    pub(crate) sound_volume: f32,
    pub(crate) sound_theme: String,
    pub(crate) floating_bar: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let config = Config::default();
        Self::from_config(&config)
    }
}

impl Settings {
    /// `config.toml` を読む。戻り値の bool はファイルが存在したか。
    pub(crate) fn load(path: &Path) -> (Self, bool) {
        match fs::read_to_string(path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(settings) => (settings, true),
                Err(exc) => {
                    crate::warn!(
                        "設定ファイルを解釈できません ({}): {exc}。既定値で起動します。",
                        path.display()
                    );
                    (Self::default(), true)
                }
            },
            Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => (Self::default(), false),
            Err(exc) => {
                crate::warn!(
                    "設定ファイルを読み込めません ({}): {exc}。既定値で起動します。",
                    path.display()
                );
                (Self::default(), false)
            }
        }
    }

    /// 設定を TOML として保存する。
    pub(crate) fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self)
            .map_err(|exc| format!("設定を TOML に変換できません: {exc}"))?;
        fs::write(path, text)
            .map_err(|exc| format!("設定を保存できません ({}): {exc}", path.display()))
    }

    /// UI 設定を実行時 [`Config`] へ反映する。
    pub(crate) fn apply_to(&self, config: &mut Config) {
        if MODEL_IDS.contains(&self.model.as_str()) {
            config.model.clone_from(&self.model);
        }
        if !self.hotkey.trim().is_empty() {
            config.hotkey = self.hotkey.trim().to_string();
        }
        config.autostart = self.autostart;
        config.device = self.device.clone().filter(|value| !value.trim().is_empty());
        // 旧バージョンの `auto` は日本語誤爆の原因になるため引き継がない。
        // 明示的に auto を使いたい場合は CLI の --language auto を使う。
        if matches!(self.language.as_str(), "ja" | "en" | "zh" | "ko" | "yue") {
            config.language.clone_from(&self.language);
        }
        config.num_threads = self.num_threads.clamp(1, 16);
        config.sound_start = self.sound_start;
        config.sound_finish = self.sound_finish;
        config.sound_volume = self.sound_volume.clamp(0.0, 1.0);
        config.sound_theme = SoundTheme::parse(&self.sound_theme);
        config.floating_bar = self.floating_bar;
    }

    /// 実行時 [`Config`] から保存用の設定を作る。
    ///
    /// `first_run` は実行時 [`Config`] に存在しないため、既定では true とする。
    /// 保存済みの値を維持したい呼び出し元は
    /// [`Settings::from_config_with_first_run`] を使う。
    pub(crate) fn from_config(config: &Config) -> Self {
        Self::from_config_with_first_run(config, true)
    }

    /// 実行時 [`Config`] と既存の `first_run` から保存用の設定を作る。
    pub(crate) fn from_config_with_first_run(config: &Config, first_run: bool) -> Self {
        Self {
            first_run,
            model: config.model.clone(),
            hotkey: config.hotkey.clone(),
            autostart: config.autostart,
            device: config.device.clone(),
            language: config.language.clone(),
            num_threads: config.num_threads,
            sound_start: config.sound_start,
            sound_finish: config.sound_finish,
            sound_volume: config.sound_volume,
            sound_theme: config.sound_theme.as_str().to_string(),
            floating_bar: config.floating_bar,
        }
    }
}

/// CLI の生の上書き値。
///
/// 値が `None` の項目は [`Config::default`] の値を維持する。
#[derive(Debug, Default)]
pub(crate) struct Cli {
    model: Option<String>,
    model_dir: Option<PathBuf>,
    hotkey: Option<String>,
    paste_modifier: Option<String>,
    device: Option<String>,
    language: Option<String>,
    num_threads: Option<i32>,
    provider: Option<String>,
    hotwords_score: Option<f32>,
    highpass: Option<bool>,
    denoise: Option<bool>,
    pre_roll_ms: Option<u32>,
    min_duration_ms: Option<u32>,
    paste_delay_ms: Option<u32>,
    restore_clipboard: Option<bool>,
    itn: Option<bool>,
    /// ファジーマッチを有効にする。
    pub(crate) fuzzy: bool,
    /// 入力デバイス一覧を表示して終了する。
    pub(crate) list_devices: bool,
    /// テキスト整形の評価ケース JSON。
    pub(crate) eval: Option<PathBuf>,
    /// 評価時にファジーしきい値をスイープする。
    pub(crate) sweep: bool,
    /// デバッグログを有効にする。
    pub(crate) debug: bool,
    /// コンソールウィンドウを表示したままにする。
    pub(crate) console: bool,
    /// ヘルプを表示して終了する。
    pub(crate) help: bool,
}

/// CLI ヘルプ。`--help` でそのまま表示する。
pub(crate) const HELP: &str = "\
使い方: BAREVoiceJP [オプション]

SenseVoice / ReazonSpeech / Whisper を使う Push-to-Talk 音声入力 (Windows)

オプション:
  -h, --help                 このヘルプを表示して終了
  --model NAME               推論モデル: sensevoice (既定), reazonspeech,
                             whisper-base, whisper-small, whisper-turbo
  --model-dir DIR            選択したモデルのディレクトリ
  --hotkey KEY               押している間だけ録音するキー (既定: alt_r)
  --paste-modifier MOD       貼り付けの修飾キー (既定: ctrl)
  --device SPEC              入力デバイス番号または名前の一部
  --language LANG            SenseVoice の認識言語 (既定: ja / auto/en/zh/ko/yue)
                             Whisper は ja 固定
  --num-threads N            CPU 推論スレッド数 (既定: 2)
  --provider NAME            ONNX Runtime provider (既定: cpu)
  --hotwords-score F         ホットワードスコア (既定: 2.0)
  --highpass / --no-highpass 80Hz ハイパスフィルタ (既定: OFF)
  --denoise / --no-denoise   RNNoise ノイズリダクション (既定: OFF)
  --pre-roll-ms MS           押下直前の音声を含める長さ (既定: 200)
  --min-duration-ms MS       これ未満の押下は破棄 (既定: 200)
  --paste-delay-ms MS        クリップボード設定から貼り付けまでの待機 (既定: 50)
  --restore-clipboard / --no-restore-clipboard
                             貼り付け後に元のクリップボードを復元 (既定: OFF)
  --itn / --no-itn           SenseVoice の逆テキスト正規化 (既定: ON)
  --fuzzy                    あいまい一致を有効化 (既定: OFF、完全一致のみ)
  --list-devices             入力デバイス一覧を表示して終了
  --eval FILE                テキスト整形の評価ケース JSON を実行して終了
                             音声キャプチャと ASR は初期化しない
  --sweep                    --eval 時にファジーしきい値 70/75/80/85 を比較
  --debug                    デバッグログを表示
  --console                  コンソールを表示したまま起動 (既定: 非表示)
";

impl Cli {
    /// データルートへ移動する前に、相対パス引数を絶対パスへ直す。
    pub(crate) fn resolve_paths(&mut self) {
        self.eval = self
            .eval
            .take()
            .map(|path| path.canonicalize().unwrap_or(path));
        self.model_dir = self
            .model_dir
            .take()
            .map(|path| path.canonicalize().unwrap_or(path));
    }
}

/// コマンドライン引数 (プログラム名を除く) を解析する。
///
/// 値の検証と `--no-*` の解釈だけを行い、`Config` への反映は
/// [`Config::apply_cli`] が担当する。
pub(crate) fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let (name, inline_value) = match arg.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };
        let mut take_value = |name: &str| -> Result<String, String> {
            if let Some(value) = inline_value.clone() {
                return Ok(value);
            }
            iter.next()
                .ok_or_else(|| format!("引数 {name} には値が必要です"))
        };
        match name.as_str() {
            "-h" | "--help" => cli.help = true,
            "--model" => {
                let value = take_value("--model")?;
                if !MODEL_IDS.contains(&value.as_str()) {
                    return Err(format!(
                        "--model は sensevoice / reazonspeech / whisper-base / \
                         whisper-small / whisper-turbo のいずれかです: {value}"
                    ));
                }
                cli.model = Some(value);
            }
            "--model-dir" => cli.model_dir = Some(PathBuf::from(take_value("--model-dir")?)),
            "--hotkey" => cli.hotkey = Some(take_value("--hotkey")?),
            "--paste-modifier" => {
                cli.paste_modifier = Some(take_value("--paste-modifier")?);
            }
            "--device" => cli.device = Some(take_value("--device")?),
            "--language" => {
                let value = take_value("--language")?;
                const LANGS: [&str; 6] = ["auto", "ja", "en", "zh", "ko", "yue"];
                if !LANGS.contains(&value.as_str()) {
                    return Err(format!(
                        "--language は auto/ja/en/zh/ko/yue のいずれかです: {value}"
                    ));
                }
                cli.language = Some(value);
            }
            "--num-threads" => {
                let value = take_value("--num-threads")?;
                cli.num_threads = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--num-threads が数値ではありません: {value}"))?,
                );
            }
            "--provider" => cli.provider = Some(take_value("--provider")?),
            "--hotwords-score" => {
                let value = take_value("--hotwords-score")?;
                cli.hotwords_score = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--hotwords-score が数値ではありません: {value}"))?,
                );
            }
            "--highpass" => cli.highpass = Some(true),
            "--no-highpass" => cli.highpass = Some(false),
            "--denoise" => cli.denoise = Some(true),
            "--no-denoise" => cli.denoise = Some(false),
            "--pre-roll-ms" => {
                let value = take_value("--pre-roll-ms")?;
                cli.pre_roll_ms = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--pre-roll-ms が数値ではありません: {value}"))?,
                );
            }
            "--min-duration-ms" => {
                let value = take_value("--min-duration-ms")?;
                cli.min_duration_ms = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--min-duration-ms が数値ではありません: {value}"))?,
                );
            }
            "--paste-delay-ms" => {
                let value = take_value("--paste-delay-ms")?;
                cli.paste_delay_ms = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--paste-delay-ms が数値ではありません: {value}"))?,
                );
            }
            "--restore-clipboard" => cli.restore_clipboard = Some(true),
            "--no-restore-clipboard" => cli.restore_clipboard = Some(false),
            "--itn" => cli.itn = Some(true),
            "--no-itn" => cli.itn = Some(false),
            "--fuzzy" => cli.fuzzy = true,
            "--list-devices" => cli.list_devices = true,
            "--eval" => cli.eval = Some(PathBuf::from(take_value("--eval")?)),
            "--sweep" => cli.sweep = true,
            "--debug" => cli.debug = true,
            "--console" => cli.console = true,
            other => {
                if other.starts_with('-') {
                    return Err(format!("不明なオプションです: {other}\n\n{HELP}"));
                }
                return Err(format!("位置引数は受け付けません: {other}"));
            }
        }
    }
    Ok(cli)
}

impl Config {
    /// CLI で明示された項目だけを `Config` に反映する。
    pub(crate) fn apply_cli(&mut self, cli: &Cli) {
        if let Some(value) = &cli.model {
            self.model = value.clone();
        }
        if let Some(value) = &cli.hotkey {
            self.hotkey = value.clone();
        }
        if let Some(value) = &cli.paste_modifier {
            self.paste_modifier = value.clone();
        }
        if let Some(value) = &cli.device {
            self.device = Some(value.clone());
        }
        if let Some(value) = &cli.language {
            self.language = value.clone();
        }
        if let Some(value) = cli.num_threads {
            self.num_threads = value;
        }
        if let Some(value) = &cli.provider {
            self.provider = value.clone();
        }
        if let Some(value) = cli.hotwords_score {
            self.hotwords_score = value;
        }
        if let Some(value) = cli.highpass {
            self.highpass = value;
        }
        if let Some(value) = cli.denoise {
            self.denoise = value;
        }
        if let Some(value) = cli.pre_roll_ms {
            self.pre_roll_ms = value;
        }
        if let Some(value) = cli.min_duration_ms {
            self.min_duration_ms = value;
        }
        if let Some(value) = cli.paste_delay_ms {
            self.paste_delay_ms = value;
        }
        if let Some(value) = cli.restore_clipboard {
            self.restore_clipboard = value;
        }
        if let Some(value) = cli.itn {
            self.use_itn = value;
        }
        if cli.fuzzy {
            self.fuzzy = true;
        }
        self.model_dir_override = cli.model_dir.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_is_off_by_default_and_enabled_by_flag() {
        let mut config = Config::default();
        assert!(!config.fuzzy);

        config.apply_cli(&parse_args(Vec::<String>::new()).expect("空の引数"));
        assert!(!config.fuzzy);

        config.apply_cli(&parse_args(["--fuzzy".to_string()]).expect("--fuzzy"));
        assert!(config.fuzzy);
    }

    #[test]
    fn accepts_whisper_models_and_resolves_dirs() {
        for model in ["whisper-base", "whisper-small", "whisper-turbo"] {
            let mut config = Config::default();
            config.apply_cli(
                &parse_args(["--model".to_string(), model.to_string()]).expect("有効なモデル"),
            );
            assert_eq!(config.model, model);
            assert!(config.model_dir_for(model).ends_with(model_dir_name(model)));
        }
    }

    #[test]
    fn settings_roundtrip_and_clamp() {
        let mut config = Config::default();
        let mut settings = Settings::from_config(&config);
        settings.first_run = false;
        settings.model = "whisper-small".to_string();
        settings.sound_theme = "chime".to_string();
        settings.sound_volume = 2.0;
        settings.num_threads = 99;
        settings.apply_to(&mut config);
        assert_eq!(config.model, "whisper-small");
        assert_eq!(config.sound_theme, SoundTheme::Chime);
        assert!((config.sound_volume - 1.0).abs() < f32::EPSILON);
        assert_eq!(config.num_threads, 16);

        let text = toml::to_string_pretty(&settings).expect("TOML へ変換");
        let parsed: Settings = toml::from_str(&text).expect("TOML から復元");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn language_defaults_to_ja_and_stale_auto_is_ignored() {
        let mut config = Config::default();
        assert_eq!(config.language, "ja");

        let mut settings = Settings::from_config(&config);
        settings.language = "auto".to_string();
        settings.apply_to(&mut config);
        assert_eq!(config.language, "ja");

        config.apply_cli(
            &parse_args(["--language".to_string(), "auto".to_string()]).expect("--language auto"),
        );
        assert_eq!(config.language, "auto");
    }

    #[test]
    fn settings_defaults_fill_missing_keys() {
        let settings: Settings = toml::from_str("hotkey = \"f9\"\n").expect("部分的な設定");
        assert_eq!(settings.hotkey, "f9");
        assert_eq!(settings.model, "sensevoice");
        assert!(settings.floating_bar);
        assert!(settings.first_run);
    }
}
