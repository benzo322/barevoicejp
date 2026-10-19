//! sherpa-onnx C-API の最小 FFI バインディングと推論エンジン。
//!
//! `include/sherpa-onnx/c-api/c-api.h` の構造体レイアウトをそのまま写し、
//! SenseVoice と ReazonSpeech (transducer) のオフライン認識だけを扱う。

use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};

use crate::config::{self, Config};

// ---------------------------------------------------------------------------
// C-API 構造体 (フィールド順は c-api.h と完全に一致させる)
// ---------------------------------------------------------------------------
#[repr(C)]
struct SherpaOnnxFeatureConfig {
    sample_rate: i32,
    feature_dim: i32,
}

#[repr(C)]
struct SherpaOnnxOfflineTransducerModelConfig {
    encoder: *const c_char,
    decoder: *const c_char,
    joiner: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineParaformerModelConfig {
    model: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineNemoEncDecCtcModelConfig {
    model: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineWhisperModelConfig {
    encoder: *const c_char,
    decoder: *const c_char,
    language: *const c_char,
    task: *const c_char,
    tail_paddings: i32,
    enable_token_timestamps: i32,
    enable_segment_timestamps: i32,
}

#[repr(C)]
struct SherpaOnnxOfflineTdnnModelConfig {
    model: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineLMConfig {
    model: *const c_char,
    scale: f32,
}

#[repr(C)]
struct SherpaOnnxOfflineSenseVoiceModelConfig {
    model: *const c_char,
    language: *const c_char,
    use_itn: i32,
}

#[repr(C)]
struct SherpaOnnxOfflineMoonshineModelConfig {
    preprocessor: *const c_char,
    encoder: *const c_char,
    uncached_decoder: *const c_char,
    cached_decoder: *const c_char,
    merged_decoder: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineFireRedAsrModelConfig {
    encoder: *const c_char,
    decoder: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineDolphinModelConfig {
    model: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineZipformerCtcModelConfig {
    model: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineCanaryModelConfig {
    encoder: *const c_char,
    decoder: *const c_char,
    src_lang: *const c_char,
    tgt_lang: *const c_char,
    use_pnc: i32,
}

#[repr(C)]
struct SherpaOnnxOfflineWenetCtcModelConfig {
    model: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineOmnilingualAsrCtcModelConfig {
    model: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineMedAsrCtcModelConfig {
    model: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineFunASRNanoModelConfig {
    encoder_adaptor: *const c_char,
    llm: *const c_char,
    embedding: *const c_char,
    tokenizer: *const c_char,
    system_prompt: *const c_char,
    user_prompt: *const c_char,
    max_new_tokens: i32,
    temperature: f32,
    top_p: f32,
    seed: i32,
    language: *const c_char,
    itn: i32,
    hotwords: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineFireRedAsrCtcModelConfig {
    model: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineQwen3ASRModelConfig {
    conv_frontend: *const c_char,
    encoder: *const c_char,
    decoder: *const c_char,
    tokenizer: *const c_char,
    max_total_len: i32,
    max_new_tokens: i32,
    temperature: f32,
    top_p: f32,
    seed: i32,
    hotwords: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineCohereTranscribeModelConfig {
    encoder: *const c_char,
    decoder: *const c_char,
    language: *const c_char,
    use_punct: i32,
    use_itn: i32,
}

#[repr(C)]
struct SherpaOnnxOfflineModelConfig {
    transducer: SherpaOnnxOfflineTransducerModelConfig,
    paraformer: SherpaOnnxOfflineParaformerModelConfig,
    nemo_ctc: SherpaOnnxOfflineNemoEncDecCtcModelConfig,
    whisper: SherpaOnnxOfflineWhisperModelConfig,
    tdnn: SherpaOnnxOfflineTdnnModelConfig,
    tokens: *const c_char,
    num_threads: i32,
    debug: i32,
    provider: *const c_char,
    model_type: *const c_char,
    modeling_unit: *const c_char,
    bpe_vocab: *const c_char,
    telespeech_ctc: *const c_char,
    sense_voice: SherpaOnnxOfflineSenseVoiceModelConfig,
    moonshine: SherpaOnnxOfflineMoonshineModelConfig,
    fire_red_asr: SherpaOnnxOfflineFireRedAsrModelConfig,
    dolphin: SherpaOnnxOfflineDolphinModelConfig,
    zipformer_ctc: SherpaOnnxOfflineZipformerCtcModelConfig,
    canary: SherpaOnnxOfflineCanaryModelConfig,
    wenet_ctc: SherpaOnnxOfflineWenetCtcModelConfig,
    omnilingual: SherpaOnnxOfflineOmnilingualAsrCtcModelConfig,
    medasr: SherpaOnnxOfflineMedAsrCtcModelConfig,
    funasr_nano: SherpaOnnxOfflineFunASRNanoModelConfig,
    fire_red_asr_ctc: SherpaOnnxOfflineFireRedAsrCtcModelConfig,
    qwen3_asr: SherpaOnnxOfflineQwen3ASRModelConfig,
    cohere_transcribe: SherpaOnnxOfflineCohereTranscribeModelConfig,
}

#[repr(C)]
struct SherpaOnnxHomophoneReplacerConfig {
    dict_dir: *const c_char,
    lexicon: *const c_char,
    rule_fsts: *const c_char,
}

#[repr(C)]
struct SherpaOnnxOfflineRecognizerConfig {
    feat_config: SherpaOnnxFeatureConfig,
    model_config: SherpaOnnxOfflineModelConfig,
    lm_config: SherpaOnnxOfflineLMConfig,
    decoding_method: *const c_char,
    max_active_paths: i32,
    hotwords_file: *const c_char,
    hotwords_score: f32,
    rule_fsts: *const c_char,
    rule_fars: *const c_char,
    blank_penalty: f32,
    hr: SherpaOnnxHomophoneReplacerConfig,
}

#[repr(C)]
struct SherpaOnnxOfflineRecognizer {
    _private: [u8; 0],
}

#[repr(C)]
struct SherpaOnnxOfflineStream {
    _private: [u8; 0],
}

/// `text` は先頭フィールドなので、この前方宣言だけでも安全に読める。
#[repr(C)]
struct SherpaOnnxOfflineRecognizerResult {
    text: *const c_char,
}

unsafe extern "C" {
    fn SherpaOnnxCreateOfflineRecognizer(
        config: *const SherpaOnnxOfflineRecognizerConfig,
    ) -> *const SherpaOnnxOfflineRecognizer;
    fn SherpaOnnxDestroyOfflineRecognizer(recognizer: *const SherpaOnnxOfflineRecognizer);
    fn SherpaOnnxCreateOfflineStream(
        recognizer: *const SherpaOnnxOfflineRecognizer,
    ) -> *const SherpaOnnxOfflineStream;
    fn SherpaOnnxDestroyOfflineStream(stream: *const SherpaOnnxOfflineStream);
    fn SherpaOnnxAcceptWaveformOffline(
        stream: *const SherpaOnnxOfflineStream,
        sample_rate: i32,
        samples: *const f32,
        n: i32,
    );
    fn SherpaOnnxDecodeOfflineStream(
        recognizer: *const SherpaOnnxOfflineRecognizer,
        stream: *const SherpaOnnxOfflineStream,
    );
    fn SherpaOnnxGetOfflineStreamResult(
        stream: *const SherpaOnnxOfflineStream,
    ) -> *const SherpaOnnxOfflineRecognizerResult;
    fn SherpaOnnxDestroyOfflineRecognizerResult(result: *const SherpaOnnxOfflineRecognizerResult);
}

// ---------------------------------------------------------------------------
// RAII ハンドル
// ---------------------------------------------------------------------------
/// 認識器ハンドル。ドロップ時に必ず C API で破棄する。
struct Recognizer(*const SherpaOnnxOfflineRecognizer);

impl Recognizer {
    /// NULL をエラーへ変換し、成功時は所有権を保持する。
    fn from_raw(ptr: *const SherpaOnnxOfflineRecognizer) -> Result<Self, String> {
        if ptr.is_null() {
            Err("SherpaOnnxCreateOfflineRecognizer が NULL を返しました".to_string())
        } else {
            Ok(Self(ptr))
        }
    }

    fn as_ptr(&self) -> *const SherpaOnnxOfflineRecognizer {
        self.0
    }
}

impl Drop for Recognizer {
    fn drop(&mut self) {
        // SAFETY: `from_raw` が非 NULL を確認した所有ハンドルだけを保持する。
        unsafe {
            SherpaOnnxDestroyOfflineRecognizer(self.0);
        }
    }
}

// SAFETY: 認識器はワーカースレッドだけが使用する。C API のハンドルを
// 複数スレッドから同時に操作しない前提でスレッド間の移動を許可する。
unsafe impl Send for Recognizer {}

/// 1 発話分のオフラインストリーム。ドロップ時に破棄する。
struct StreamGuard(*const SherpaOnnxOfflineStream);

impl StreamGuard {
    /// ストリームを作成する。作成できなければ `None`。
    fn create(recognizer: *const SherpaOnnxOfflineRecognizer) -> Option<Self> {
        // SAFETY: recognizer は有効なハンドル。
        let ptr = unsafe { SherpaOnnxCreateOfflineStream(recognizer) };
        (!ptr.is_null()).then_some(Self(ptr))
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        // SAFETY: `create` が非 NULL を確認したストリームのみ保持する。
        unsafe {
            SherpaOnnxDestroyOfflineStream(self.0);
        }
    }
}

/// 認識結果スナップショット。ドロップ時に破棄する。
struct ResultGuard(*const SherpaOnnxOfflineRecognizerResult);

impl Drop for ResultGuard {
    fn drop(&mut self) {
        // SAFETY: 非 NULL の結果だけを保持する。
        unsafe {
            SherpaOnnxDestroyOfflineRecognizerResult(self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// エンジン
// ---------------------------------------------------------------------------
/// SenseVoice / ReazonSpeech のオフライン認識エンジン。
pub struct Engine {
    recognizer: Recognizer,
    sample_rate: i32,
    hotwords_supported: bool,
}

impl Engine {
    /// 設定に応じてモデルを読み込み、認識器を初期化する。
    pub fn build(config: &Config) -> Result<Self, String> {
        match config.model.as_str() {
            "sensevoice" => Self::sense_voice(config),
            "reazonspeech" => Self::reazonspeech(config),
            other => Err(format!(
                "不明なモデル種別です: {other} (sensevoice / reazonspeech)"
            )),
        }
    }

    /// SenseVoice のホットワードが有効に初期化できたか。
    pub fn supports_hotwords(&self) -> bool {
        self.hotwords_supported
    }

    /// 16kHz / mono / float32 の波形を文字起こしする。
    pub fn transcribe(&self, samples: &[f32]) -> String {
        if samples.is_empty() {
            return String::new();
        }
        let Some(stream) = StreamGuard::create(self.recognizer.as_ptr()) else {
            crate::warn!("オフラインストリームを作成できませんでした");
            return String::new();
        };
        let sample_count = i32::try_from(samples.len()).unwrap_or(i32::MAX);

        // SAFETY: recognizer / stream は有効で、samples は呼び出し中有効。
        unsafe {
            SherpaOnnxAcceptWaveformOffline(
                stream.0,
                self.sample_rate,
                samples.as_ptr(),
                sample_count,
            );
            SherpaOnnxDecodeOfflineStream(self.recognizer.as_ptr(), stream.0);
        }

        // SAFETY: stream は有効。結果の所有権は ResultGuard が引き継ぐ。
        let raw_result = unsafe { SherpaOnnxGetOfflineStreamResult(stream.0) };
        if raw_result.is_null() {
            return String::new();
        }
        let result = ResultGuard(raw_result);

        // SAFETY: 結果は有効。`text` は NUL 終端か NULL。
        let ptr = unsafe { (*result.0).text };
        if ptr.is_null() {
            String::new()
        } else {
            // SAFETY: 結果が生きている間だけ `text` を読む。
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    }

    fn sense_voice(config: &Config) -> Result<Self, String> {
        let dir = config.model_dir_for("sensevoice");
        let (model, tokens) = resolve_sensevoice_files(&dir)?;
        crate::info!("SenseVoice モデル読み込み: {}", model.display());

        let hotwords = config.hotwords_file();
        if let Some(hotwords_path) = hotwords.as_deref() {
            match create_sense_voice(config, &model, &tokens, Some(hotwords_path)) {
                Ok(recognizer) => return Ok(Self::finish(recognizer, config, true)),
                Err(exc) => crate::warn!(
                    "ホットワードの読み込みに失敗しました。\
                     ホットワードなしで初期化します: {exc}"
                ),
            }
        }

        let recognizer = create_sense_voice(config, &model, &tokens, None).map_err(|exc| {
            format!(
                "SenseVoice モデルの初期化に失敗しました: {}\n\
                 \x20 provider={} / num_threads={}\n\
                 \x20 詳細: {exc}",
                model.display(),
                config.provider,
                config.num_threads
            )
        })?;
        Ok(Self::finish(recognizer, config, false))
    }

    fn reazonspeech(config: &Config) -> Result<Self, String> {
        let dir = config.model_dir_for("reazonspeech");
        let (encoder, decoder, joiner, tokens) = resolve_reazonspeech_files(&dir)?;
        crate::info!(
            "ReazonSpeech モデル読み込み: {}",
            encoder.parent().unwrap_or(&dir).display()
        );
        let recognizer =
            create_transducer(config, &encoder, &decoder, &joiner, &tokens).map_err(|exc| {
                format!(
                    "ReazonSpeech モデルの初期化に失敗しました: {}\n\
                     \x20 provider={} / num_threads={}\n\
                     \x20 詳細: {exc}",
                    dir.display(),
                    config.provider,
                    config.num_threads
                )
            })?;
        crate::info!(
            "モデル準備完了 (ReazonSpeech / threads={}, provider={})",
            config.num_threads,
            config.provider
        );
        Ok(Self {
            recognizer,
            sample_rate: config.sample_rate as i32,
            hotwords_supported: false,
        })
    }

    fn finish(recognizer: Recognizer, config: &Config, hotwords_supported: bool) -> Self {
        crate::info!(
            "モデル準備完了 (SenseVoice / ITN={}, language={}, threads={}, provider={})",
            if config.use_itn { "on" } else { "off" },
            config.language,
            config.num_threads,
            config.provider
        );
        Self {
            recognizer,
            sample_rate: config.sample_rate as i32,
            hotwords_supported,
        }
    }
}

// ---------------------------------------------------------------------------
// モデル解決
// ---------------------------------------------------------------------------
/// SenseVoice の `.onnx` と `tokens.txt` を探す。
fn resolve_sensevoice_files(dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let dir = dir.to_path_buf();
    let (base, model_candidates) = if dir.is_file() {
        let base = dir.parent().unwrap_or(Path::new(".")).to_path_buf();
        (base, vec![dir.clone()])
    } else {
        (
            dir.clone(),
            ["model.int8.onnx", "model.onnx", "model.fp16.onnx"]
                .iter()
                .map(|name| dir.join(name))
                .collect(),
        )
    };

    let Some(model) = model_candidates.iter().find(|path| path.is_file()) else {
        let names = model_candidates
            .iter()
            .map(|path| path.file_name().unwrap_or_default().to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "SenseVoice モデルが見つかりません: {}\n\
             探したファイル: {names}\n\
             次のいずれかで用意してください:\n{SENSEVOICE_DOWNLOAD_HINT}\
             または --model-dir /path/to/model-dir を指定してください。",
            base.display()
        ));
    };
    let tokens = base.join("tokens.txt");
    if !tokens.is_file() {
        return Err(format!(
            "tokens.txt が見つかりません: {}\n\
             モデルの tar.bz2 を展開し直すか、--model-dir を確認してください。",
            tokens.display()
        ));
    }
    Ok((model.clone(), tokens))
}

/// ReazonSpeech Zipformer の encoder/decoder/joiner/tokens を探す。
fn resolve_reazonspeech_files(dir: &Path) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), String> {
    let base = if dir.is_file() {
        dir.parent().unwrap_or(Path::new(".")).to_path_buf()
    } else {
        dir.to_path_buf()
    };
    let encoder = base.join("encoder-epoch-99-avg-1.int8.onnx");
    let decoder = base.join("decoder-epoch-99-avg-1.onnx");
    let joiner = base.join("joiner-epoch-99-avg-1.int8.onnx");
    let tokens = base.join("tokens.txt");
    let expected = [
        ("encoder", &encoder),
        ("decoder", &decoder),
        ("joiner", &joiner),
        ("tokens", &tokens),
    ];
    let missing: Vec<String> = expected
        .iter()
        .filter(|(_, path)| !path.is_file())
        .map(|(name, path)| {
            format!(
                "{name}: {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        })
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "ReazonSpeech モデルが見つかりません: {}\n\
             不足ファイル: {}\n\
             次のコマンドで用意してください:\n{REAZONSPEECH_DOWNLOAD_HINT}\
             または --model-dir /path/to/model-dir を指定してください。",
            base.display(),
            missing.join(", ")
        ));
    }
    Ok((encoder, decoder, joiner, tokens))
}

const SENSEVOICE_DOWNLOAD_HINT: &str = "\
  mkdir -p models && cd models\n\
  curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2\n\
  tar xf sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2\n";

const REAZONSPEECH_DOWNLOAD_HINT: &str = "\
  mkdir -p models && cd models\n\
  curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01.tar.bz2\n\
  tar xf sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01.tar.bz2\n";

fn path_string(path: &Path) -> Result<CString, String> {
    CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| format!("パスに NUL 文字が含まれています: {}", path.display()))
}

fn text_string(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| format!("文字列に NUL 文字が含まれています: {value:?}"))
}

/// C 構造体をゼロ初期化し、全モデルで共通の値だけ埋める。
///
/// ゼロ初期化により未使用モデルのポインタはすべて NULL になり、
/// sherpa-onnx 側で「未設定」として扱われる。
fn empty_config(config: &Config) -> SherpaOnnxOfflineRecognizerConfig {
    // SAFETY: この C 構造体は NULL ポインタと 0 を有効な未設定値として扱う。
    let mut cfg: SherpaOnnxOfflineRecognizerConfig = unsafe { std::mem::zeroed() };
    cfg.feat_config.sample_rate = config.sample_rate as i32;
    cfg.feat_config.feature_dim = config::FEATURE_DIM;
    cfg.model_config.num_threads = config.num_threads;
    cfg.model_config.debug = 0;
    cfg.max_active_paths = config::MAX_ACTIVE_PATHS;
    cfg
}

/// SenseVoice 用の C 設定を組み立てて認識器を作る。
///
/// 設定内の `CString` は呼び出し中だけ生かせばよい。C API は初期化時に
/// 文字列を内部へコピーする。
fn create_sense_voice(
    config: &Config,
    model: &Path,
    tokens: &Path,
    hotwords: Option<&Path>,
) -> Result<Recognizer, String> {
    let model = path_string(model)?;
    let tokens = path_string(tokens)?;
    let provider = text_string(&config.provider)?;
    let language = text_string(&config.language)?;
    let hotwords_file = hotwords.map(path_string).transpose()?;

    let mut cfg = empty_config(config);
    cfg.model_config.tokens = tokens.as_ptr();
    cfg.model_config.provider = provider.as_ptr();
    cfg.model_config.sense_voice = SherpaOnnxOfflineSenseVoiceModelConfig {
        model: model.as_ptr(),
        language: language.as_ptr(),
        use_itn: i32::from(config.use_itn),
    };
    let decoding = text_string("greedy_search")?;
    cfg.decoding_method = decoding.as_ptr();
    if let Some(hotwords) = &hotwords_file {
        cfg.hotwords_file = hotwords.as_ptr();
        cfg.hotwords_score = config.hotwords_score;
    }

    // SAFETY: cfg のポインタはすべて生存中の CString を指している。
    let recognizer = unsafe { SherpaOnnxCreateOfflineRecognizer(&cfg) };
    Recognizer::from_raw(recognizer)
}

/// ReazonSpeech (transducer) 用の C 設定を組み立てて認識器を作る。
fn create_transducer(
    config: &Config,
    encoder: &Path,
    decoder: &Path,
    joiner: &Path,
    tokens: &Path,
) -> Result<Recognizer, String> {
    let encoder = path_string(encoder)?;
    let decoder = path_string(decoder)?;
    let joiner = path_string(joiner)?;
    let tokens = path_string(tokens)?;
    let provider = text_string(&config.provider)?;
    let decoding = text_string("greedy_search")?;

    let mut cfg = empty_config(config);
    cfg.model_config.transducer = SherpaOnnxOfflineTransducerModelConfig {
        encoder: encoder.as_ptr(),
        decoder: decoder.as_ptr(),
        joiner: joiner.as_ptr(),
    };
    cfg.model_config.tokens = tokens.as_ptr();
    cfg.model_config.provider = provider.as_ptr();
    cfg.decoding_method = decoding.as_ptr();

    // SAFETY: cfg のポインタはすべて生存中の CString を指している。
    let recognizer = unsafe { SherpaOnnxCreateOfflineRecognizer(&cfg) };
    Recognizer::from_raw(recognizer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_c_api_header() {
        use std::mem::{offset_of, size_of};
        // c-api.h を MSVC でコンパイルして得た実測値。
        assert_eq!(size_of::<SherpaOnnxOfflineRecognizerConfig>(), 608);
        assert_eq!(size_of::<SherpaOnnxOfflineModelConfig>(), 504);
        assert_eq!(size_of::<SherpaOnnxOfflineSenseVoiceModelConfig>(), 24);
        assert_eq!(size_of::<SherpaOnnxFeatureConfig>(), 8);
        assert_eq!(size_of::<SherpaOnnxHomophoneReplacerConfig>(), 24);
        assert_eq!(
            offset_of!(SherpaOnnxOfflineRecognizerConfig, model_config),
            8
        );
        assert_eq!(
            offset_of!(SherpaOnnxOfflineRecognizerConfig, lm_config),
            512
        );
        assert_eq!(offset_of!(SherpaOnnxOfflineModelConfig, sense_voice), 152);
        assert_eq!(offset_of!(SherpaOnnxOfflineModelConfig, qwen3_asr), 408);
    }

    #[test]
    #[ignore = "実モデルと sherpa-onnx DLL が必要"]
    fn transcribes_reference_wav() {
        let config = Config::default();
        let engine = Engine::build(&config).expect("エンジン初期化");
        let wav = std::fs::read(config.sensevoice_model_dir.join("test_wavs/ja.wav"))
            .expect("ja.wav を読めません");
        let samples = parse_wav(&wav);
        assert_eq!(samples.len(), 115200);
        let text = engine.transcribe(&samples);
        eprintln!(
            "hotwords_supported={} text={text:?}",
            engine.supports_hotwords()
        );
        assert_eq!(
            text,
            "うちの中学は弁当制で持っていきない場合は50円の学校販売のパンを買う。"
        );
    }

    #[test]
    #[ignore = "実モデルと sherpa-onnx DLL が必要"]
    fn transcribes_reazonspeech_wav() {
        let config = Config {
            model: "reazonspeech".to_string(),
            ..Config::default()
        };
        let engine = Engine::build(&config).expect("エンジン初期化");
        let wav = std::fs::read(config.sensevoice_model_dir.join("test_wavs/ja.wav"))
            .expect("ja.wav を読めません");
        let samples = parse_wav(&wav);
        let text = engine.transcribe(&samples);
        eprintln!("text={text:?}");
        assert_eq!(
            text,
            "うちの中学は弁当制で持っていけない場合は五十円の学校販売のパンを買う"
        );
    }

    fn parse_wav(bytes: &[u8]) -> Vec<f32> {
        assert_eq!(&bytes[0..4], b"RIFF");
        let mut offset = 12;
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let size =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let start = offset + 8;
            if id == b"data" {
                return bytes[start..start + size]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[lo, hi]| i16::from_le_bytes([*lo, *hi]) as f32 / 32768.0)
                    .collect();
            }
            offset = start + size + (size & 1);
        }
        panic!("data チャンクが見つかりません");
    }
}
