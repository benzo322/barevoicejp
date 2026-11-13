//! モデルのダウンロード・配置・削除。
//!
//! tar.bz2 は展開し、Haas-ja のような直接配布モデルは ONNX 一式を
//! そのまま `models/` 配下へ配置する。進捗は UI が読めるよう
//! プロセス内で共有する。

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::config::{self, Config};

/// tar.bz2 配布モデル (sherpa-onnx / GitHub Releases) の公式置き場。
///
/// `model_base_url` が空のときは、ここから直接 tar.bz2 を取得する。
/// 別の配布元を使う場合は `config.toml` の `model_base_url` で上書きする。
/// Haas-ja は `Package::Files` の `official_base` (Hugging Face) を使う。
const OFFICIAL_RELEASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models";

/// `model_base_url` 未設定時に設定画面へ表示する配布元の説明。
///
/// モデルごとに公式配布元が異なる (sherpa-onnx GitHub / Hugging Face) ため、
/// 単一の URL ではなく説明を返す。
pub(crate) fn official_source_label() -> &'static str {
    "公式配布元 (sherpa-onnx GitHub / Hugging Face)"
}

/// 配布元のルートに置く SHA-256 マニフェスト名。
const MANIFEST_FILE: &str = "checksums.sha256";
/// マニフェストの最大サイズ。壊れた/巨大なファイルを読み込まない。
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// 1 アーカイブの最大サイズ。ディスク枯渇攻撃を防ぐ。
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// 展開後の最大サイズ (シンボリックリンクを含む tar の展開爆弾対策)。
const MAX_EXTRACTED_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// HTTP クライアントを作る。`body_timeout` は本文受信の総時間。
fn http_agent(body_timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        // ネットワーク到達不能を早く検出する。本文受信は大容量を許容する。
        .timeout_resolve(Some(Duration::from_secs(10)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(body_timeout))
        .build();
    ureq::Agent::new_with_config(config)
}

/// アーカイブ用 HTTP クライアント。
///
/// ureq の TLS プロバイダは既定で Rustls だが、本クレートは native-tls のみを
/// 有効にしている。プロバイダを明示しないと HTTPS 接続時に ureq が panic する。
/// Haas-ja 量子化版の `encoder.data` は約 410 MB あるため、本文の総時間を長めに取る。
static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| http_agent(Duration::from_secs(2 * 60 * 60)));

/// マニフェスト用。本文が小さいので短いタイムアウトにする。
static HTTP_MANIFEST: LazyLock<ureq::Agent> = LazyLock::new(|| http_agent(Duration::from_secs(30)));
/// Haas-ja-v1.0 (int8/4bit 量子化) の公式配布元 (Hugging Face)。
const HAAS_JA_OFFICIAL: &str =
    "https://huggingface.co/oboroge0/Haas-ja-v1.0-onnx-quantized/resolve/main";

/// 直接配置するファイル 1 つ分の定義。
struct RemoteFile {
    name: &'static str,
    /// 配布元のファイルサイズ (バイト)。検証と進捗表示に使う。
    size: u64,
    /// 配布元のファイルの SHA-256。
    sha256: &'static str,
}

/// Haas-ja-v1.0 量子化版の配布ファイル。
///
/// `encoder.quantized.onnx` は外部データ `encoder.data` を参照するため、
/// 必ず同一ディレクトリへ配置する。
const HAAS_JA_FILES: [RemoteFile; 5] = [
    RemoteFile {
        name: "encoder.quantized.onnx",
        size: 42_261_252,
        sha256: "aba291d61d3d594bc6fb8c7f90acbd980e7bc7bf6b7caaf3965ddc25e6f42fac",
    },
    RemoteFile {
        name: "encoder.data",
        size: 410_744_064,
        sha256: "30befbf6aeb75383e6551f5e58de53efdb76972ddc7d0f76070c517899873586",
    },
    RemoteFile {
        name: "decoder.onnx",
        size: 15_661_602,
        sha256: "2947cb5512699cbba753fe45fe221428aadaa9856597debbb7aabe19cc253622",
    },
    RemoteFile {
        name: "joiner.onnx",
        size: 12_158_641,
        sha256: "8a68320e750890ff9e944a794b41ae1c42c6f439ac4c273503458a6b42adc12d",
    },
    RemoteFile {
        name: "tokens.txt",
        size: 28_704,
        sha256: "ec78b13c5d9f156cf0d5fdbd6b5aec5785680f319c92630c11d5d11b14b83a84",
    },
];

/// 公式配布元 (sherpa-onnx GitHub Releases) の tar.bz2 定義。
///
/// `size` は配布アーカイブのバイト数、`sha256` はダウンロード後の検証に使う。
/// 配布物が差し替えられた場合は、実際のアーカイブから再計算して更新すること。
const SENSEVOICE_ARCHIVE: RemoteFile = RemoteFile {
    name: "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2",
    size: 163_002_883,
    sha256: "7d1efa2138a65b0b488df37f8b89e3d91a60676e416f515b952358d83dfd347e",
};

const REAZONSPEECH_ARCHIVE: RemoteFile = RemoteFile {
    name: "sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01.tar.bz2",
    size: 713_097_333,
    sha256: "e0981d0d5d7b446d41010831b59091ebf57d2aa7b79980f67ca37af460b5842d",
};

const WHISPER_BASE_ARCHIVE: RemoteFile = RemoteFile {
    name: "sherpa-onnx-whisper-base.tar.bz2",
    size: 207_557_382,
    sha256: "911b2083efd7c0dca2ac3b358b75222660dc09fb716d64fbfc417ba6c99ff3de",
};

const WHISPER_SMALL_ARCHIVE: RemoteFile = RemoteFile {
    name: "sherpa-onnx-whisper-small.tar.bz2",
    size: 639_387_718,
    sha256: "486a46afbb7ba798507190ffe02fea2dd726049af212e774537efac6afb210a6",
};

const WHISPER_TURBO_ARCHIVE: RemoteFile = RemoteFile {
    name: "sherpa-onnx-whisper-turbo.tar.bz2",
    size: 563_790_207,
    sha256: "b11acbbcd660b44a8e0df33724feb5aaa709cf65668f2823d59f656312544f22",
};

/// 設定画面に表示するモデル。
pub(crate) struct ModelSpec {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) subtitle: &'static str,
    package: Package,
    pub(crate) size_label: &'static str,
    pub(crate) speed: u8,
    pub(crate) accuracy: u8,
    pub(crate) recommended: bool,
}

/// モデルの配布形式。
enum Package {
    /// tar.bz2 をダウンロードして `models/` へ展開する。
    Archive(RemoteFile),
    /// ONNX 等のファイルを個別にダウンロードして配置する。
    Files {
        /// `model_base_url` が空のときの公式ベース URL。
        official_base: &'static str,
        files: &'static [RemoteFile],
    },
}

impl Package {
    fn archive(&self) -> Option<&RemoteFile> {
        match self {
            Self::Archive(archive) => Some(archive),
            Self::Files { .. } => None,
        }
    }

    fn files(&self) -> &'static [RemoteFile] {
        match self {
            Self::Archive(_) => &[],
            Self::Files { files, .. } => files,
        }
    }

    fn official_base(&self) -> &'static str {
        match self {
            Self::Archive(_) => OFFICIAL_RELEASE,
            Self::Files { official_base, .. } => official_base,
        }
    }
}

/// 表示順に並べたモデル定義。
pub(crate) const SPECS: [ModelSpec; 6] = [
    ModelSpec {
        id: "sensevoice",
        name: "SenseVoice",
        subtitle: "日本語・英語・中国語・韓国語・広東語。バランスの良い定番。",
        package: Package::Archive(SENSEVOICE_ARCHIVE),
        size_label: "約 155 MB",
        speed: 5,
        accuracy: 4,
        recommended: true,
    },
    ModelSpec {
        id: "reazonspeech",
        name: "ReazonSpeech",
        subtitle: "日本語音声に特化。長い発話でも安定した精度。",
        package: Package::Archive(REAZONSPEECH_ARCHIVE),
        size_label: "約 680 MB",
        speed: 4,
        accuracy: 5,
        recommended: false,
    },
    ModelSpec {
        id: "haas-ja-v1.0",
        name: "Haas-ja-v1.0",
        subtitle: "日本語特化。int8量子化で軽量・省メモリ。",
        package: Package::Files {
            official_base: HAAS_JA_OFFICIAL,
            files: &HAAS_JA_FILES,
        },
        size_label: "約 480 MB",
        speed: 4,
        accuracy: 5,
        recommended: false,
    },
    ModelSpec {
        id: "whisper-base",
        name: "Whisper Base",
        subtitle: "軽量で起動が速い。短いメモやチャット向け。",
        package: Package::Archive(WHISPER_BASE_ARCHIVE),
        size_label: "約 198 MB",
        speed: 5,
        accuracy: 3,
        recommended: false,
    },
    ModelSpec {
        id: "whisper-small",
        name: "Whisper Small",
        subtitle: "速度と精度のバランス型。幅広い音声に対応。",
        package: Package::Archive(WHISPER_SMALL_ARCHIVE),
        size_label: "約 610 MB",
        speed: 3,
        accuracy: 4,
        recommended: false,
    },
    ModelSpec {
        id: "whisper-turbo",
        name: "Whisper Turbo",
        subtitle: "高い認識精度。マシンパワーに余裕がある場合に。",
        package: Package::Archive(WHISPER_TURBO_ARCHIVE),
        size_label: "約 538 MB",
        speed: 2,
        accuracy: 5,
        recommended: false,
    },
];

/// ID に対応するモデル定義を返す。
fn spec(id: &str) -> Option<&'static ModelSpec> {
    SPECS.iter().find(|spec| spec.id == id)
}

/// 設定画面・履歴表示用のモデル名を返す。
pub(crate) fn model_name(id: &str) -> &'static str {
    spec(id).map(|spec| spec.name).unwrap_or("Unknown")
}

/// モデルの配布元。公式配布元、HTTPS サーバー、UNC/絶対パス共有。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    /// `model_base_url` 未設定時の直接ダウンロード。
    Official,
    Http(String),
    File(PathBuf),
}

impl Source {
    /// `config.toml` の `model_base_url` を解釈する。
    ///
    /// 空文字は公式配布元からの直接ダウンロードを意味する。
    fn parse(base: &str) -> Result<Self, String> {
        let base = base.trim().trim_end_matches('/');
        if base.is_empty() {
            return Ok(Self::Official);
        }
        if let Some(rest) = base.strip_prefix("http://") {
            // 平文 HTTP はマニフェストごと差し替えられるため、ローカル検証以外は許可しない。
            if !is_loopback_authority(rest) {
                return Err(format!(
                    "model_base_url は https:// で指定してください（平文 HTTP は \
                     127.0.0.1 / localhost / [::1] の検証時のみ許可）: {}",
                    config::redact_source(base)
                ));
            }
            return Ok(Self::Http(base.to_string()));
        }
        if base.starts_with("https://") {
            return Ok(Self::Http(base.to_string()));
        }
        let path = if let Some(rest) = base.strip_prefix("file://") {
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            #[cfg(windows)]
            let rest = rest
                .strip_prefix('/')
                .filter(|value| value.as_bytes().get(1) == Some(&b':'))
                .unwrap_or(rest);
            PathBuf::from(rest)
        } else {
            PathBuf::from(base)
        };
        if path.is_absolute() {
            Ok(Self::File(path))
        } else {
            // 入力値は資格情報を含み得るため、エラーへそのまま埋め込まない。
            Err("model_base_url が URL / 絶対パスではありません。\
                 https://host/path または \\\\server\\share\\path を指定してください。"
                .to_string())
        }
    }

    /// 配布元のファイル URL。ローカル共有なら `None`。
    fn file_url(&self, spec: &ModelSpec, name: &str) -> Option<String> {
        match self {
            Self::Official => Some(format!("{}/{name}", spec.package.official_base())),
            Self::Http(base) => Some(format!("{base}/{name}")),
            Self::File(_) => None,
        }
    }

    /// 配布元から SHA-256 マニフェストを読む。
    fn manifest_text(&self) -> Result<String, String> {
        match self {
            Self::Http(base) => {
                let url = format!("{base}/{MANIFEST_FILE}");
                let response =
                    http_get(&HTTP_MANIFEST, &url).map_err(|exc| http_error(&url, exc))?;
                let mut text = String::new();
                response
                    .into_body()
                    .into_reader()
                    .take(MAX_MANIFEST_BYTES + 1)
                    .read_to_string(&mut text)
                    .map_err(|exc| {
                        format!(
                            "配布元の {MANIFEST_FILE} を読み込めません ({}): {exc}",
                            config::redact_source(&url)
                        )
                    })?;
                if text.len() as u64 > MAX_MANIFEST_BYTES {
                    return Err(format!(
                        "配布元の {MANIFEST_FILE} が大きすぎます: {}",
                        config::redact_source(&url)
                    ));
                }
                Ok(text)
            }
            Self::Official => Err(format!("公式配布元には {MANIFEST_FILE} がありません")),
            Self::File(dir) => {
                let path = dir.join(MANIFEST_FILE);
                let metadata = fs::metadata(&path).map_err(|exc| file_error(&path, exc))?;
                if metadata.len() > MAX_MANIFEST_BYTES {
                    return Err(format!(
                        "配布元の {MANIFEST_FILE} が大きすぎます: {}",
                        path.display()
                    ));
                }
                fs::read_to_string(&path).map_err(|exc| {
                    format!(
                        "配布元の {MANIFEST_FILE} を読み込めません ({}): {exc}",
                        path.display()
                    )
                })
            }
        }
    }
}

/// 平文 HTTP を許可するループバックの authority か。
///
/// 認証情報 (`user:pass@`) とポートを除いたホスト名だけを見る。
fn is_loopback_authority(rest: &str) -> bool {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// `https://` の URL ではリダイレクトも含めて HTTPS を強制する。
///
/// 平文 HTTP はループバックの検証用にだけ許可するため、その場合だけ
/// `https_only` を無効にする。
fn http_get(
    agent: &ureq::Agent,
    url: &str,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let secure = url.starts_with("https://");
    agent.get(url).config().https_only(secure).build().call()
}

/// 配布元の接続・HTTP エラーを UI 向けの日本語メッセージにする。
fn http_error(url: &str, exc: ureq::Error) -> String {
    let url = config::redact_source(url);
    match &exc {
        ureq::Error::Timeout(_) => format!(
            "モデル配布元に接続できません（タイムアウト）。\
             ネットワーク接続を確認してください: {url}"
        ),
        ureq::Error::HostNotFound => format!(
            "モデル配布元のホスト名を解決できません。\
             ネットワーク接続を確認してください: {url}"
        ),
        ureq::Error::StatusCode(code) => {
            format!("モデル配布元が HTTP {code} を返しました: {url}")
        }
        ureq::Error::RequireHttpsOnly(target) => {
            let target = config::redact_source(target);
            format!(
                "HTTPS 以外へのリダイレクトを拒否しました（配布元の設定を確認してください）: {target}"
            )
        }
        ureq::Error::ConnectionFailed | ureq::Error::Io(_) => format!(
            "モデル配布元に接続できません。ネットワーク接続を確認してください: {url} ({exc})"
        ),
        _ => format!("モデル配布元から取得できません: {url} ({exc})"),
    }
}

/// 共有パスのエラーを UI 向けの日本語メッセージにする。
fn file_error(path: &Path, exc: std::io::Error) -> String {
    match exc.kind() {
        ErrorKind::NotFound => {
            format!("配布元のフォルダにファイルがありません: {}", path.display())
        }
        ErrorKind::PermissionDenied => format!(
            "配布元のフォルダへアクセスできません（アクセス権を確認してください）: {}",
            path.display()
        ),
        _ => format!(
            "配布元のフォルダに接続できません: {} ({exc})",
            path.display()
        ),
    }
}

/// `sha256sum` 互換のマニフェストをファイル名 → ハッシュへ変換する。
fn parse_sha256_manifest(text: &str) -> Result<HashMap<String, String>, String> {
    let mut hashes = HashMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts
            .next()
            .ok_or_else(|| format!("{MANIFEST_FILE} {}行目が不正です", index + 1))?;
        let name = parts
            .next()
            .ok_or_else(|| format!("{MANIFEST_FILE} {}行目にファイル名がありません", index + 1))?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "{MANIFEST_FILE} {}行目の SHA-256 が不正です: {hash}",
                index + 1
            ));
        }
        let name = Path::new(name.trim_start_matches('*'))
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(name);
        hashes.insert(name.to_string(), hash.to_ascii_lowercase());
    }
    if hashes.is_empty() {
        return Err(format!("{MANIFEST_FILE} に有効なエントリがありません"));
    }
    Ok(hashes)
}

/// ローカルファイルの SHA-256 を小文字 16 進で返す。
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|exc| format!("検証対象を開けません ({}): {exc}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|exc| format!("検証対象を読めません ({}): {exc}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 期待する SHA-256 と一致するか検証する。
fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(format!(
        "SHA-256 が一致しません（期待 {expected} / 実際 {actual}）。\
         ダウンロードが不完全です。再ダウンロードしてください。"
    ))
}

/// ダウンロード工程。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    Downloading,
    Verifying,
    Extracting,
    Done,
    Failed,
}

/// 1 モデル分のダウンロード進捗。
#[derive(Clone, Debug)]
pub(crate) struct DownloadState {
    pub(crate) phase: Phase,
    pub(crate) downloaded: u64,
    pub(crate) total: u64,
    pub(crate) error: Option<String>,
}

impl Default for DownloadState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            downloaded: 0,
            total: 0,
            error: None,
        }
    }
}

static PROGRESS: LazyLock<Mutex<HashMap<String, DownloadState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn progress(id: &str) -> DownloadState {
    PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(id)
        .cloned()
        .unwrap_or_default()
}

fn update(id: &str, state: DownloadState) {
    PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(id.to_string(), state);
}

/// 設定画面が読むダウンロード進捗のコピー。
pub(crate) fn download_state(id: &str) -> DownloadState {
    progress(id)
}

fn fail(id: &str, message: impl Into<String>) {
    let message = message.into();
    crate::warn!("モデルのダウンロードに失敗しました ({id}): {message}");
    update(
        id,
        DownloadState {
            phase: Phase::Failed,
            error: Some(message),
            ..DownloadState::default()
        },
    );
}

/// モデルの実体が揃っているか。
pub(crate) fn is_installed(config: &Config, id: &str) -> bool {
    installed_at(&config.default_model_dir_for(id), id)
}

/// 指定ディレクトリにモデルの実体が揃っているか。
fn installed_at(dir: &Path, id: &str) -> bool {
    let files = || {
        fs::read_dir(dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|kind| kind.is_file())
                    .and_then(|_| entry.file_name().into_string().ok())
            })
            .collect::<Vec<_>>()
    };
    let names = files();
    if names.is_empty() {
        return false;
    }
    let has = |needle: &str, suffix: &str| {
        names
            .iter()
            .any(|name| name.contains(needle) && name.ends_with(suffix))
    };
    if let Some(spec) = spec(id)
        && let Package::Files { files, .. } = &spec.package
    {
        let installed = |name: &str| names.iter().any(|entry| entry.as_str() == name);
        if files.iter().all(|file| installed(file.name)) {
            return true;
        }
        // 旧 fp32 版 Haas-ja (`encoder.onnx` + 約2.4GB の `encoder.data`) が
        // 既にある場合は、そのままでも起動できるようインストール済みとして扱う。
        if id == "haas-ja-v1.0" {
            return files.iter().all(|file| {
                let name = if file.name == "encoder.quantized.onnx" {
                    "encoder.onnx"
                } else {
                    file.name
                };
                installed(name)
            });
        }
        return false;
    }
    match id {
        "sensevoice" => {
            (names.iter().any(|name| {
                matches!(
                    name.as_str(),
                    "model.int8.onnx" | "model.onnx" | "model.fp16.onnx"
                )
            })) && names.iter().any(|name| name == "tokens.txt")
        }
        "reazonspeech" => {
            has("encoder", ".onnx")
                && has("decoder", ".onnx")
                && has("joiner", ".onnx")
                && names.iter().any(|name| name == "tokens.txt")
        }
        _ => has("encoder", ".onnx") && has("decoder", ".onnx") && has("tokens", ".txt"),
    }
}

/// ディレクトリ配下の合計バイト数。UI の「使用容量」表示に使う。
///
/// シンボリックリンク/ジャンクションは辿らない。モデル外の巨大な
/// ディレクトリや循環参照を数えてしまうのを防ぐ。
pub(crate) fn installed_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            let Ok(file_type) = entry.file_type() else {
                return 0;
            };
            if file_type.is_symlink() {
                0
            } else if file_type.is_dir() {
                installed_bytes(&entry.path())
            } else {
                entry.metadata().map(|meta| meta.len()).unwrap_or(0)
            }
        })
        .sum()
}

fn archive_path(id: &str) -> Option<PathBuf> {
    let archive = spec(id)?.package.archive()?;
    Some(config::models_root().join(".cache").join(archive.name))
}

/// モデルをダウンロードして展開する。完了・失敗時は `done` を呼ぶ。
pub(crate) fn download<F>(id: &str, base_url: &str, done: F)
where
    F: FnOnce() + Send + 'static,
{
    if spec(id).is_none() {
        return;
    }
    let dest = config::models_root().join(config::model_dir_name(id));
    if dest.exists() {
        if installed_at(&dest, id) {
            update(
                id,
                DownloadState {
                    phase: Phase::Done,
                    ..DownloadState::default()
                },
            );
        } else {
            fail(
                id,
                format!(
                    "モデルの展開先が不完全な状態で存在します。\
                     削除してから再ダウンロードしてください: {}",
                    dest.display()
                ),
            );
        }
        done();
        return;
    }
    update(
        id,
        DownloadState {
            phase: Phase::Downloading,
            ..DownloadState::default()
        },
    );

    let worker_id = id.to_string();
    let thread_id = worker_id.clone();
    let base_url = base_url.to_string();
    let result = std::thread::Builder::new()
        .name(format!("model-download-{id}"))
        .spawn(move || {
            let Some(spec) = spec(&thread_id) else {
                return;
            };
            run_download(&thread_id, spec, &dest, &base_url);
            done();
        });
    if let Err(exc) = result {
        fail(id, format!("ダウンロードスレッドを開始できません: {exc}"));
    }
}

/// ダウンロードと展開を実行し、進捗を更新する。
fn run_download(id: &str, spec: &ModelSpec, dest: &Path, base_url: &str) {
    let cache_dir = config::models_root().join(".cache");
    run_download_with_cache(id, spec, dest, base_url, &cache_dir);
}

/// `cache_dir` を差し替えられる本体 (テスト用)。
fn run_download_with_cache(
    id: &str,
    spec: &ModelSpec,
    dest: &Path,
    base_url: &str,
    cache_dir: &Path,
) {
    let source = match Source::parse(base_url) {
        Ok(source) => source,
        Err(exc) => {
            fail(id, exc);
            return;
        }
    };
    let Some(archive_spec) = spec.package.archive() else {
        run_file_download(id, spec, dest, &source);
        return;
    };
    let archive_name = archive_spec.name;
    if let Err(exc) = fs::create_dir_all(cache_dir) {
        fail(id, format!("キャッシュディレクトリを作成できません: {exc}"));
        return;
    }
    let archive = cache_dir.join(archive_name);
    let (expected, expected_size) = match &source {
        Source::Official => {
            crate::info!("公式配布元からモデルを取得します: {archive_name}");
            (archive_spec.sha256.to_string(), Some(archive_spec.size))
        }
        _ => {
            let hashes = match manifest_hashes(&source) {
                Ok(hashes) => hashes,
                Err(exc) => {
                    fail(id, exc);
                    return;
                }
            };
            let Some(expected) = hashes.get(archive_name) else {
                fail(
                    id,
                    format!(
                        "{MANIFEST_FILE} に {archive_name} の SHA-256 がありません。\
                         配布元のマニフェストを確認してください。"
                    ),
                );
                return;
            };
            (expected.clone(), None)
        }
    };
    if let Err(exc) = fetch(
        id,
        &source,
        spec,
        archive_name,
        &archive,
        &expected,
        expected_size,
        0,
        0,
    ) {
        fail(id, exc);
        return;
    }

    update(
        id,
        DownloadState {
            phase: Phase::Extracting,
            downloaded: archive_metadata(&archive),
            total: archive_metadata(&archive),
            error: None,
        },
    );
    if let Err(exc) = extract(id, spec, &archive, dest) {
        fail(id, exc);
        if let Err(remove) = fs::remove_file(&archive) {
            crate::debug!("壊れたアーカイブを削除できません: {remove}");
        }
        return;
    }
    let total = installed_bytes(dest);
    update(
        id,
        DownloadState {
            phase: Phase::Done,
            downloaded: total,
            total,
            error: None,
        },
    );
    if let Err(exc) = fs::remove_file(&archive) {
        crate::debug!("アーカイブを削除できません: {exc}");
    }
    crate::info!("モデルを展開しました: {}", dest.display());
}

/// 配布元の SHA-256 マニフェストを読み、ファイル名 → ハッシュにする。
fn manifest_hashes(source: &Source) -> Result<HashMap<String, String>, String> {
    parse_sha256_manifest(&source.manifest_text()?)
}

/// ONNX 等を 1 ファイルずつ取得し、一時ディレクトリから `dest` へ移す。
fn run_file_download(id: &str, spec: &ModelSpec, dest: &Path, source: &Source) {
    let files = spec.package.files();
    let total = files.iter().map(|file| file.size).sum::<u64>();
    let hashes = match source {
        Source::Official => None,
        _ => match manifest_hashes(source) {
            Ok(hashes) => Some(hashes),
            Err(exc) => {
                fail(id, exc);
                return;
            }
        },
    };

    let temp_root = dest.parent().unwrap_or(Path::new("."));
    let temp = temp_root.join(format!(".extract-{id}"));
    if temp.exists()
        && let Err(exc) = fs::remove_dir_all(&temp)
    {
        fail(id, format!("古い一時ディレクトリを削除できません: {exc}"));
        return;
    }
    if let Err(exc) = fs::create_dir_all(&temp) {
        fail(id, format!("一時ディレクトリを作成できません: {exc}"));
        return;
    }

    let mut downloaded = 0u64;
    for file in files {
        let (expected, expected_size) = match &hashes {
            Some(hashes) => match hashes.get(file.name) {
                Some(expected) => (expected.as_str(), None),
                None => {
                    fail(
                        id,
                        format!(
                            "{MANIFEST_FILE} に {} の SHA-256 がありません。\
                             配布元のマニフェストを確認してください。",
                            file.name
                        ),
                    );
                    let _ = fs::remove_dir_all(&temp);
                    return;
                }
            },
            None => (file.sha256, Some(file.size)),
        };
        let target = temp.join(file.name);
        match fetch(
            id,
            source,
            spec,
            file.name,
            &target,
            expected,
            expected_size,
            downloaded,
            total,
        ) {
            Ok(bytes) => downloaded = downloaded.saturating_add(bytes),
            Err(exc) => {
                fail(id, exc);
                let _ = fs::remove_dir_all(&temp);
                return;
            }
        }
    }

    if dest.exists() {
        fail(id, format!("展開先が既に存在します: {}", dest.display()));
        let _ = fs::remove_dir_all(&temp);
        return;
    }
    if let Err(exc) = fs::rename(&temp, dest) {
        fail(id, format!("{} へ移動できません: {exc}", spec.name));
        let _ = fs::remove_dir_all(&temp);
        return;
    }
    let bytes = installed_bytes(dest);
    update(
        id,
        DownloadState {
            phase: Phase::Done,
            downloaded: bytes,
            total: bytes,
            error: None,
        },
    );
    crate::info!("モデルを配置しました: {}", dest.display());
}

/// `.part` を付けた一時パス。
///
/// `with_extension` は `encoder.quantized.onnx` と `encoder.data` を同じ名前に
/// してしまうため、拡張子を保ったまま末尾へ足す。
fn part_path(path: &Path) -> PathBuf {
    let mut part = path.as_os_str().to_os_string();
    part.push(".part");
    PathBuf::from(part)
}

/// 配布元が公表するサイズと、埋め込みの期待サイズを照合する。
///
/// `expected` が `None` の場合は照合しない (カスタム配布元はマニフェストの
/// ハッシュだけを信頼する)。
fn check_size(label: &str, actual: u64, expected: Option<u64>) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "配布元のファイルサイズが想定と異なります（期待 {expected} / 実際 {actual} バイト）: {label}"
    ))
}

/// 配布元から 1 ファイルを `.part` へ取得し、サイズと SHA-256 を検証後に確定する。
///
/// `expected_size` は公式配布元のように期待バイト数が分かっている場合に渡す。
/// HTTP 応答に `Content-Length` が無い場合はサイズを検証できないため失敗させる。
#[allow(clippy::too_many_arguments)]
fn fetch(
    id: &str,
    source: &Source,
    spec: &ModelSpec,
    file_name: &str,
    target: &Path,
    expected: &str,
    expected_size: Option<u64>,
    base: u64,
    grand_total: u64,
) -> Result<u64, String> {
    let part = part_path(target);
    let (reader, file_total): (Box<dyn Read>, u64) = match source {
        Source::Official | Source::Http(_) => {
            let url = source
                .file_url(spec, file_name)
                .expect("HTTP 配布元にはファイル URL がある");
            let response = http_get(&HTTP, &url).map_err(|exc| http_error(&url, exc))?;
            let total = response
                .body()
                .content_length()
                .or_else(|| {
                    response
                        .headers()
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse().ok())
                })
                .ok_or_else(|| {
                    format!(
                        "配布元が Content-Length を返しませんでした。\
                         サイズを検証できないためダウンロードを中止します: {}",
                        config::redact_source(&url)
                    )
                })?;
            if total > MAX_ARCHIVE_BYTES {
                return Err(format!(
                    "配布元のファイルが大きすぎます（{} バイト）: {}",
                    total,
                    config::redact_source(&url)
                ));
            }
            check_size(&config::redact_source(&url), total, expected_size)?;
            (Box::new(response.into_body().into_reader()), total)
        }
        Source::File(dir) => {
            let path = dir.join(file_name);
            let file = File::open(&path).map_err(|exc| file_error(&path, exc))?;
            let total = file.metadata().map(|meta| meta.len()).unwrap_or(0);
            if total > MAX_ARCHIVE_BYTES {
                return Err(format!(
                    "配布元のファイルが大きすぎます（{total} バイト）: {}",
                    path.display()
                ));
            }
            check_size(&path.display().to_string(), total, expected_size)?;
            (Box::new(file), total)
        }
    };
    let downloaded = match copy_with_progress(id, reader, file_total, &part, base, grand_total) {
        Ok(downloaded) => downloaded,
        Err(exc) => {
            let _ = fs::remove_file(&part);
            return Err(exc);
        }
    };
    if let Some(expected) = expected_size
        && downloaded != expected
    {
        let _ = fs::remove_file(&part);
        return Err(format!(
            "受信したファイルサイズが想定と異なります（期待 {expected} / 実際 {downloaded} バイト）。\
             再ダウンロードしてください。"
        ));
    }
    let shown = base.saturating_add(downloaded);
    let shown_total = if grand_total > 0 {
        grand_total
    } else {
        file_total
    };
    update(
        id,
        DownloadState {
            phase: Phase::Verifying,
            downloaded: shown,
            total: shown_total.max(shown),
            error: None,
        },
    );
    if let Err(exc) = verify_sha256(&part, expected) {
        let _ = fs::remove_file(&part);
        return Err(exc);
    }
    if target.exists() {
        let _ = fs::remove_file(target);
    }
    fs::rename(&part, target).map_err(|exc| {
        let _ = fs::remove_file(&part);
        format!("ファイルを確定できません: {exc}")
    })?;
    Ok(downloaded)
}

/// 受信しながら `.part` へ書き、120ms 間隔で進捗を更新する。
///
/// `base` は複数ファイルを続けて取得する際の既完了バイト数、
/// `grand_total` はモデル全体の合計サイズ (0 なら単一ファイルのサイズ)。
fn copy_with_progress<R: Read>(
    id: &str,
    mut reader: R,
    file_total: u64,
    part: &Path,
    base: u64,
    grand_total: u64,
) -> Result<u64, String> {
    let mut file =
        File::create(part).map_err(|exc| format!("一時ファイルを作成できません: {exc}"))?;
    let mut downloaded = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut last_report = Instant::now();
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|exc| format!("受信中にエラーが発生しました: {exc}"))?;
        if read == 0 {
            break;
        }
        if downloaded.saturating_add(read as u64) > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "受信データが上限（{MAX_ARCHIVE_BYTES} バイト）を超えました。\
                 配布元のファイルを確認してください。"
            ));
        }
        file.write_all(&buffer[..read])
            .map_err(|exc| format!("受信データを書き込めません: {exc}"))?;
        downloaded += read as u64;
        if last_report.elapsed() >= Duration::from_millis(120) {
            let shown = base.saturating_add(downloaded);
            let total = if grand_total > 0 {
                grand_total
            } else {
                file_total
            };
            update(
                id,
                DownloadState {
                    phase: Phase::Downloading,
                    downloaded: shown,
                    total: total.max(shown),
                    error: None,
                },
            );
            last_report = Instant::now();
        }
    }
    file.flush()
        .map_err(|exc| format!("受信データを保存できません: {exc}"))?;
    drop(file);
    if file_total > 0 && downloaded != file_total {
        return Err(format!(
            "配布元から受信したサイズが一致しません（期待 {file_total} / 実際 {downloaded} バイト）。\
             再ダウンロードしてください。"
        ));
    }
    let shown = base.saturating_add(downloaded);
    let total = if grand_total > 0 {
        grand_total
    } else {
        file_total
    };
    update(
        id,
        DownloadState {
            phase: Phase::Downloading,
            downloaded: shown,
            total: total.max(shown),
            error: None,
        },
    );
    Ok(downloaded)
}

/// 展開後サイズを上限で打ち切る読み取りラッパー。
///
/// 圧縮された tar の展開爆弾でディスクを埋め尽くされないようにする。
struct LimitedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> LimitedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::other(
                "展開後のサイズが上限を超えました（アーカイブを確認してください）",
            ));
        }
        let limit = buffer.len().min(self.remaining as usize);
        let read = self.inner.read(&mut buffer[..limit])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

/// 一時ディレクトリへ展開し、単一ディレクトリなら `models/` 直下へ移す。
fn extract(id: &str, spec: &ModelSpec, archive: &Path, dest: &Path) -> Result<(), String> {
    let temp_root = dest.parent().unwrap_or(Path::new("."));
    let temp = temp_root.join(format!(".extract-{id}"));
    if temp.exists() {
        fs::remove_dir_all(&temp)
            .map_err(|exc| format!("古い一時ディレクトリを削除できません: {exc}"))?;
    }
    fs::create_dir_all(&temp).map_err(|exc| format!("一時ディレクトリを作成できません: {exc}"))?;

    let file = File::open(archive).map_err(|exc| format!("アーカイブを開けません: {exc}"))?;
    let decoder = bzip2::read::BzDecoder::new(file);
    let mut tar = tar::Archive::new(LimitedReader::new(decoder, MAX_EXTRACTED_BYTES));
    if let Err(exc) = tar.unpack(&temp) {
        let _ = fs::remove_dir_all(&temp);
        return Err(format!("アーカイブを展開できません: {exc}"));
    }

    let entries = fs::read_dir(&temp)
        .map_err(|exc| format!("展開結果を読めません: {exc}"))?
        .filter_map(|entry| entry.ok())
        .collect::<Vec<_>>();
    let source =
        if entries.len() == 1 && entries[0].file_type().map(|t| t.is_dir()).unwrap_or(false) {
            entries[0].path()
        } else {
            temp.clone()
        };

    if dest.exists() {
        return Err(format!("展開先が既に存在します: {}", dest.display()));
    }
    fs::rename(&source, dest).map_err(|exc| format!("{} へ移動できません: {exc}", spec.name))?;
    if source != temp {
        let _ = fs::remove_dir_all(&temp);
    }
    Ok(())
}

/// アーカイブのサイズ。取得できなければ 0。
fn archive_metadata(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// モデルを削除して解放したバイト数を返す。
pub(crate) fn delete(config: &Config, id: &str) -> Result<u64, String> {
    if !config::MODEL_IDS.contains(&id) {
        return Err("不明なモデルです".to_string());
    }
    if config.model == id {
        return Err("使用中のモデルは削除できません".to_string());
    }
    let dir = config.default_model_dir_for(id);
    let root = config::models_root();
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    let canonical_dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
    if !canonical_dir.starts_with(&canonical_root) {
        return Err("モデルディレクトリが models/ の外にあります".to_string());
    }
    let bytes = installed_bytes(&dir);
    if dir.exists() {
        fs::remove_dir_all(&dir)
            .map_err(|exc| format!("モデルを削除できません ({}): {exc}", dir.display()))?;
    }
    if let Some(archive) = archive_path(id)
        && archive.exists()
    {
        let _ = fs::remove_file(archive);
    }
    update(id, DownloadState::default());
    Ok(bytes)
}

/// バイト数を B/KB/MB/GB の読みやすい表記にする。
pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("時刻")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "barevoicejp-models-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("一時ディレクトリ");
        dir
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    #[test]
    fn parses_http_and_unc_sources() {
        assert_eq!(
            Source::parse("https://models.example.co.jp/barevoice/models/"),
            Ok(Source::Http(
                "https://models.example.co.jp/barevoice/models".to_string()
            ))
        );
        match Source::parse(r"\\fileserver\share\BAREVoiceJP\models") {
            Ok(Source::File(path)) => assert!(path.is_absolute(), "{path:?}"),
            other => panic!("UNC パスとして解釈されない: {other:?}"),
        }
        assert_eq!(Source::parse(""), Ok(Source::Official));
        assert_eq!(
            Source::Official
                .file_url(&SPECS[0], "model.tar.bz2")
                .as_deref(),
            Some(
                "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/model.tar.bz2"
            )
        );
        let haas = spec("haas-ja-v1.0").expect("Haas の定義");
        assert_eq!(
            Source::Official
                .file_url(haas, "encoder.quantized.onnx")
                .as_deref(),
            Some(
                "https://huggingface.co/oboroge0/Haas-ja-v1.0-onnx-quantized/resolve/main/encoder.quantized.onnx"
            )
        );
        assert!(Source::parse("relative/models").is_err());
        assert!(Source::parse("htps://typo/models").is_err());
    }

    #[test]
    fn rejects_plain_http_except_loopback() {
        // 平文 HTTP はマニフェストごと差し替え可能なため、非ループバックでは拒否する。
        assert!(Source::parse("http://models.example.co.jp/models").is_err());
        assert!(Source::parse("http://192.168.0.10/models").is_err());
        assert!(Source::parse("http://user:pass@models.example.co.jp/m").is_err());
        // ループバックはローカル検証用に許可する。
        for base in [
            "http://127.0.0.1:8765",
            "http://localhost:8765/models",
            "http://[::1]:8765",
        ] {
            assert!(
                matches!(Source::parse(base), Ok(Source::Http(_))),
                "{base} が拒否された"
            );
        }
    }

    #[test]
    fn limits_extracted_bytes() {
        use std::io::{Cursor, Read as _};

        let mut reader = LimitedReader::new(Cursor::new(vec![7u8; 16]), 8);
        let mut out = [0u8; 8];
        reader.read_exact(&mut out).expect("上限までは読める");
        assert!(reader.read(&mut out).is_err(), "上限超過を検出する");
    }

    #[test]
    fn parses_sha256_manifest_with_paths_and_markers() {
        let text = "\
# comment
ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  *model.tar.bz2
BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD  models/other.tar.bz2
";
        let hashes = parse_sha256_manifest(text).expect("マニフェスト");
        assert_eq!(
            hashes.get("model.tar.bz2").map(String::as_str),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert!(hashes.contains_key("other.tar.bz2"));
    }

    #[test]
    fn rejects_bad_manifest_hash() {
        assert!(parse_sha256_manifest("not-a-hash  model.tar.bz2").is_err());
        assert!(parse_sha256_manifest("# empty").is_err());
    }

    #[test]
    fn verifies_local_sha256() {
        let dir = temp_dir("sha");
        let path = dir.join("payload.bin");
        fs::write(&path, b"abc").expect("書き込み");
        verify_sha256(
            &path,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .expect("一致");
        assert!(verify_sha256(&path, &"0".repeat(64)).is_err());
        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn file_source_copies_and_verifies_before_publishing() {
        let dir = temp_dir("file-source");
        let source_dir = dir.join("source");
        let cache_dir = dir.join("cache");
        fs::create_dir_all(&source_dir).expect("source");
        fs::create_dir_all(&cache_dir).expect("cache");
        let archive_name = "model.tar.bz2";
        let payload = b"dummy archive bytes";
        fs::write(source_dir.join(archive_name), payload).expect("source archive");
        let expected = sha256_hex(payload);
        let archive = cache_dir.join(archive_name);

        fetch(
            "test-file-ok",
            &Source::File(source_dir.clone()),
            &SPECS[0],
            archive_name,
            &archive,
            &expected,
            None,
            0,
            0,
        )
        .expect("コピーと検証");
        assert_eq!(fs::read(&archive).expect("archive"), payload);
        assert!(!part_path(&archive).exists());

        let bad_archive = cache_dir.join("bad.tar.bz2");
        let wrong = "0".repeat(64);
        let error = fetch(
            "test-file-bad",
            &Source::File(source_dir.clone()),
            &SPECS[0],
            archive_name,
            &bad_archive,
            &wrong,
            None,
            0,
            0,
        )
        .expect_err("ハッシュ不一致");
        assert!(error.contains("SHA-256"), "{error}");
        assert!(!bad_archive.exists());
        assert!(!part_path(&bad_archive).exists());

        // 期待サイズと一致しない場合は確定しない。
        let mismatch = cache_dir.join("mismatch.tar.bz2");
        let error = fetch(
            "test-file-size",
            &Source::File(source_dir),
            &SPECS[0],
            archive_name,
            &mismatch,
            &expected,
            Some(payload.len() as u64 + 1),
            0,
            0,
        )
        .expect_err("サイズ不一致");
        assert!(error.contains("サイズ"), "{error}");
        assert!(!mismatch.exists());

        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn http_source_downloads_and_verifies() {
        use std::io::{BufRead, BufReader, Write as _};
        use std::net::TcpListener;

        let dir = temp_dir("http-source");
        let archive_name = "model.tar.bz2";
        let payload: &[u8] = b"http archive bytes";
        let expected = sha256_hex(payload);
        let listener = TcpListener::bind("127.0.0.1:0").expect("HTTP サーバー");
        let address = listener.local_addr().expect("アドレス");
        let server_hash = expected.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("接続");
                let mut reader = BufReader::new(stream.try_clone().expect("stream"));
                let mut request = String::new();
                reader.read_line(&mut request).expect("リクエスト行");
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).expect("ヘッダー") == 0 || line == "\r\n" {
                        break;
                    }
                }
                if request.contains(MANIFEST_FILE) {
                    let body = format!("{server_hash}  {archive_name}\n");
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .expect("manifest response");
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    )
                    .expect("header");
                    stream.write_all(payload).expect("body");
                }
            }
        });

        let source = Source::Http(format!("http://{address}"));
        let manifest = source.manifest_text().expect("manifest");
        let hashes = parse_sha256_manifest(&manifest).expect("manifest parse");
        let expected = hashes.get(archive_name).expect("entry");
        let archive = dir.join(archive_name);
        fetch(
            "test-http",
            &source,
            &SPECS[0],
            archive_name,
            &archive,
            expected,
            None,
            0,
            0,
        )
        .expect("fetch");
        assert_eq!(fs::read(&archive).expect("archive"), payload);
        server.join().expect("server");
        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn downloads_extracts_and_reports_done_from_file_source() {
        let dir = temp_dir("run-download");
        let source_dir = dir.join("source");
        let cache_dir = dir.join("cache");
        let dest = dir.join("installed");
        fs::create_dir_all(&source_dir).expect("source");
        let archive_name = "test-model.tar.bz2";
        let archive = source_dir.join(archive_name);

        let file = File::create(&archive).expect("archive");
        let encoder = bzip2::write::BzEncoder::new(file, bzip2::Compression::best());
        let mut builder = tar::Builder::new(encoder);
        for (name, data) in [
            ("test-model/model.int8.onnx", b"model".as_slice()),
            ("test-model/tokens.txt", b"tokens".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            builder
                .append_data(&mut header, name, data)
                .expect("tar entry");
        }
        let encoder = builder.into_inner().expect("tar");
        encoder.finish().expect("bzip2");

        let bytes = fs::read(&archive).expect("archive bytes");
        fs::write(
            source_dir.join(MANIFEST_FILE),
            format!("{}  {archive_name}\n", sha256_hex(&bytes)),
        )
        .expect("manifest");
        let spec = ModelSpec {
            id: "test-model",
            name: "Test",
            subtitle: "",
            package: Package::Archive(RemoteFile {
                name: archive_name,
                size: bytes.len() as u64,
                sha256: "",
            }),
            size_label: "",
            speed: 1,
            accuracy: 1,
            recommended: false,
        };
        run_download_with_cache(
            spec.id,
            &spec,
            &dest,
            &source_dir.to_string_lossy(),
            &cache_dir,
        );

        assert!(dest.join("model.int8.onnx").is_file());
        assert!(dest.join("tokens.txt").is_file());
        let state = download_state(spec.id);
        assert_eq!(state.phase, Phase::Done);
        assert!(state.error.is_none(), "{:?}", state.error);
        assert!(!cache_dir.join(archive_name).exists());

        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn downloads_direct_files_and_reports_done_from_file_source() {
        let dir = temp_dir("run-files");
        let source_dir = dir.join("source");
        let dest = dir.join("installed");
        fs::create_dir_all(&source_dir).expect("source");

        let payloads = [
            ("encoder.quantized.onnx", b"encoder".as_slice()),
            ("encoder.data", b"external data".as_slice()),
            ("decoder.onnx", b"decoder".as_slice()),
            ("joiner.onnx", b"joiner".as_slice()),
            ("tokens.txt", b"tokens".as_slice()),
        ];
        let files: &'static [RemoteFile] = &[
            RemoteFile {
                name: "encoder.quantized.onnx",
                size: 7,
                sha256: "",
            },
            RemoteFile {
                name: "encoder.data",
                size: 13,
                sha256: "",
            },
            RemoteFile {
                name: "decoder.onnx",
                size: 7,
                sha256: "",
            },
            RemoteFile {
                name: "joiner.onnx",
                size: 6,
                sha256: "",
            },
            RemoteFile {
                name: "tokens.txt",
                size: 6,
                sha256: "",
            },
        ];
        let mut manifest = String::new();
        for (name, data) in payloads {
            fs::write(source_dir.join(name), data).expect("source file");
            manifest.push_str(&format!("{}  {name}\n", sha256_hex(data)));
        }
        fs::write(source_dir.join(MANIFEST_FILE), manifest).expect("manifest");

        let spec = ModelSpec {
            id: "test-files",
            name: "Test Files",
            subtitle: "",
            package: Package::Files {
                official_base: "https://example.invalid",
                files,
            },
            size_label: "",
            speed: 1,
            accuracy: 1,
            recommended: false,
        };
        run_download_with_cache(
            spec.id,
            &spec,
            &dest,
            &source_dir.to_string_lossy(),
            &dir.join("cache"),
        );

        for (name, data) in payloads {
            assert_eq!(fs::read(dest.join(name)).expect("installed"), data);
        }
        let state = download_state(spec.id);
        assert_eq!(state.phase, Phase::Done);
        assert!(state.error.is_none(), "{:?}", state.error);
        assert!(!dest.parent().unwrap().join(".extract-test-files").exists());

        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn reports_unreachable_source() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("空きポート");
        let address = listener.local_addr().expect("アドレス");
        drop(listener);
        let error = Source::Http(format!("http://{address}"))
            .manifest_text()
            .expect_err("接続失敗");
        assert!(error.contains("モデル配布元に接続できません"), "{error}");
    }

    #[test]
    fn every_known_model_has_a_spec() {
        for id in config::MODEL_IDS {
            assert!(spec(id).is_some(), "{id}");
        }
        assert_eq!(SPECS.len(), config::MODEL_IDS.len());
    }

    #[test]
    fn official_archives_are_pinned() {
        for spec in &SPECS {
            if let Some(archive) = spec.package.archive() {
                assert_eq!(archive.sha256.len(), 64, "{}", archive.name);
                assert!(
                    archive.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "{}",
                    archive.name
                );
                assert!(archive.size > 0, "{}", archive.name);
            }
        }
        // Haas-ja は個別ファイル側で SHA-256 を固定している。
        assert!(
            spec("haas-ja-v1.0")
                .expect("Haas の定義")
                .package
                .archive()
                .is_none()
        );
    }

    #[test]
    fn recognizes_quantized_and_legacy_haas_layouts() {
        let dir = temp_dir("haas-installed");
        for name in [
            "encoder.quantized.onnx",
            "encoder.data",
            "decoder.onnx",
            "joiner.onnx",
            "tokens.txt",
        ] {
            fs::write(dir.join(name), b"").expect("ダミーファイル作成");
        }
        assert!(installed_at(&dir, "haas-ja-v1.0"));

        fs::remove_file(dir.join("encoder.data")).expect("外部データ削除");
        assert!(!installed_at(&dir, "haas-ja-v1.0"));

        fs::write(dir.join("encoder.data"), b"").expect("外部データ作成");
        fs::remove_file(dir.join("encoder.quantized.onnx")).expect("量子化版削除");
        fs::write(dir.join("encoder.onnx"), b"").expect("旧 encoder 作成");
        assert!(installed_at(&dir, "haas-ja-v1.0"));

        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024 * 3 / 2), "1.5 MB");
    }
}
