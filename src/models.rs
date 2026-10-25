//! モデルのダウンロード・展開・削除。
//!
//! sherpa-onnx の公式リリースから tar.bz2 を取得し、`models/` 配下へ
//! 展開する。進捗は UI が読めるようプロセス内で共有する。

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::config::{self, Config};

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
        // 社内LAN外・VPN未接続を早く検出する。本文受信は大容量を許容する。
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
static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| http_agent(Duration::from_secs(30 * 60)));

/// マニフェスト用。本文が小さいので短いタイムアウトにする。
static HTTP_MANIFEST: LazyLock<ureq::Agent> = LazyLock::new(|| http_agent(Duration::from_secs(30)));

/// 設定画面に表示するモデル。
pub(crate) struct ModelSpec {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) subtitle: &'static str,
    pub(crate) archive: &'static str,
    pub(crate) size_label: &'static str,
    pub(crate) speed: u8,
    pub(crate) accuracy: u8,
    pub(crate) recommended: bool,
}

/// 表示順に並べたモデル定義。
pub(crate) const SPECS: [ModelSpec; 5] = [
    ModelSpec {
        id: "sensevoice",
        name: "SenseVoice",
        subtitle: "日本語・英語・中国語・韓国語・広東語。バランスの良い定番。",
        archive: "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2",
        size_label: "約 155 MB",
        speed: 5,
        accuracy: 4,
        recommended: true,
    },
    ModelSpec {
        id: "reazonspeech",
        name: "ReazonSpeech",
        subtitle: "日本語音声に特化。長い発話でも安定した精度。",
        archive: "sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01.tar.bz2",
        size_label: "約 680 MB",
        speed: 4,
        accuracy: 5,
        recommended: false,
    },
    ModelSpec {
        id: "whisper-base",
        name: "Whisper Base",
        subtitle: "軽量で起動が速い。短いメモやチャット向け。",
        archive: "sherpa-onnx-whisper-base.tar.bz2",
        size_label: "約 198 MB",
        speed: 5,
        accuracy: 3,
        recommended: false,
    },
    ModelSpec {
        id: "whisper-small",
        name: "Whisper Small",
        subtitle: "速度と精度のバランス型。幅広い音声に対応。",
        archive: "sherpa-onnx-whisper-small.tar.bz2",
        size_label: "約 610 MB",
        speed: 3,
        accuracy: 4,
        recommended: false,
    },
    ModelSpec {
        id: "whisper-turbo",
        name: "Whisper Turbo",
        subtitle: "高い認識精度。マシンパワーに余裕がある場合に。",
        archive: "sherpa-onnx-whisper-turbo.tar.bz2",
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

/// モデルの配布元。社内 HTTPS サーバーまたは UNC/絶対パス共有。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    Http(String),
    File(PathBuf),
}

impl Source {
    /// `config.toml` の `model_base_url` を解釈する。
    fn parse(base: &str) -> Result<Self, String> {
        let base = base.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err("モデル配布元が未設定です。config.toml の model_base_url に\
                 社内サーバーの URL または UNC パスを設定してください。"
                .to_string());
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
            "社内サーバーに接続できません（タイムアウト）。\
             VPN/社内LANの接続を確認してください: {url}"
        ),
        ureq::Error::HostNotFound => format!(
            "社内サーバーのホスト名を解決できません。\
             VPN/社内LANの接続を確認してください: {url}"
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
            "社内サーバーに接続できません。VPN/社内LANの接続を確認してください: {url} ({exc})"
        ),
        _ => format!("モデル配布元から取得できません: {url} ({exc})"),
    }
}

/// 共有パスのエラーを UI 向けの日本語メッセージにする。
fn file_error(path: &Path, exc: std::io::Error) -> String {
    match exc.kind() {
        ErrorKind::NotFound => {
            format!(
                "社内サーバーの共有パスにファイルがありません: {}",
                path.display()
            )
        }
        ErrorKind::PermissionDenied => format!(
            "社内サーバーの共有パスへアクセスできません（共有権限を確認してください）: {}",
            path.display()
        ),
        _ => format!(
            "社内サーバーの共有パスに接続できません: {} ({exc})",
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
    spec(id).map(|spec| config::models_root().join(".cache").join(spec.archive))
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
    if let Err(exc) = fs::create_dir_all(cache_dir) {
        fail(id, format!("キャッシュディレクトリを作成できません: {exc}"));
        return;
    }
    let archive = cache_dir.join(spec.archive);
    let manifest = match source.manifest_text() {
        Ok(manifest) => manifest,
        Err(exc) => {
            fail(id, exc);
            return;
        }
    };
    let hashes = match parse_sha256_manifest(&manifest) {
        Ok(hashes) => hashes,
        Err(exc) => {
            fail(id, exc);
            return;
        }
    };
    let Some(expected) = hashes.get(spec.archive) else {
        fail(
            id,
            format!(
                "{MANIFEST_FILE} に {} の SHA-256 がありません。\
                 配布元のマニフェストを確認してください。",
                spec.archive
            ),
        );
        return;
    };
    if let Err(exc) = fetch(id, &source, spec.archive, &archive, expected) {
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

/// 配布元から tar.bz2 を `.part` へ取得し、SHA-256 検証後に確定する。
fn fetch(
    id: &str,
    source: &Source,
    archive_name: &str,
    archive: &Path,
    expected: &str,
) -> Result<(), String> {
    let part = archive.with_extension("part");
    let (reader, total): (Box<dyn Read>, u64) = match source {
        Source::Http(base) => {
            let url = format!("{base}/{archive_name}");
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
                .unwrap_or(0);
            if total > MAX_ARCHIVE_BYTES {
                return Err(format!(
                    "配布元のアーカイブが大きすぎます（{} バイト）: {}",
                    total,
                    config::redact_source(&url)
                ));
            }
            (Box::new(response.into_body().into_reader()), total)
        }
        Source::File(dir) => {
            let path = dir.join(archive_name);
            let file = File::open(&path).map_err(|exc| file_error(&path, exc))?;
            let total = file.metadata().map(|meta| meta.len()).unwrap_or(0);
            if total > MAX_ARCHIVE_BYTES {
                return Err(format!(
                    "配布元のアーカイブが大きすぎます（{total} バイト）: {}",
                    path.display()
                ));
            }
            (Box::new(file), total)
        }
    };
    let downloaded = match copy_with_progress(id, reader, total, &part) {
        Ok(downloaded) => downloaded,
        Err(exc) => {
            let _ = fs::remove_file(&part);
            return Err(exc);
        }
    };
    update(
        id,
        DownloadState {
            phase: Phase::Verifying,
            downloaded,
            total: total.max(downloaded),
            error: None,
        },
    );
    if let Err(exc) = verify_sha256(&part, expected) {
        let _ = fs::remove_file(&part);
        return Err(exc);
    }
    if archive.exists() {
        let _ = fs::remove_file(archive);
    }
    fs::rename(&part, archive).map_err(|exc| {
        let _ = fs::remove_file(&part);
        format!("アーカイブを確定できません: {exc}")
    })?;
    Ok(())
}

/// 受信しながら `.part` へ書き、120ms 間隔で進捗を更新する。
fn copy_with_progress<R: Read>(
    id: &str,
    mut reader: R,
    total: u64,
    part: &Path,
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
                 配布元のアーカイブを確認してください。"
            ));
        }
        file.write_all(&buffer[..read])
            .map_err(|exc| format!("受信データを書き込めません: {exc}"))?;
        downloaded += read as u64;
        if last_report.elapsed() >= Duration::from_millis(120) {
            update(
                id,
                DownloadState {
                    phase: Phase::Downloading,
                    downloaded,
                    total,
                    error: None,
                },
            );
            last_report = Instant::now();
        }
    }
    file.flush()
        .map_err(|exc| format!("受信データを保存できません: {exc}"))?;
    drop(file);
    if total > 0 && downloaded != total {
        return Err(format!(
            "配布元から受信したサイズが一致しません（期待 {total} / 実際 {downloaded} バイト）。\
             再ダウンロードしてください。"
        ));
    }
    update(
        id,
        DownloadState {
            phase: Phase::Downloading,
            downloaded,
            total: total.max(downloaded),
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
        assert!(Source::parse("").is_err());
        assert!(Source::parse("relative/models").is_err());
        assert!(Source::parse("htps://typo/models").is_err());
    }

    #[test]
    fn rejects_plain_http_except_loopback() {
        // 平文 HTTP はマニフェストごと差し替え可能なため、社内ホストでは拒否する。
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
            archive_name,
            &archive,
            &expected,
        )
        .expect("コピーと検証");
        assert_eq!(fs::read(&archive).expect("archive"), payload);
        assert!(!archive.with_extension("part").exists());

        let bad_archive = cache_dir.join("bad.tar.bz2");
        let error = fetch(
            "test-file-bad",
            &Source::File(source_dir),
            archive_name,
            &bad_archive,
            &"0".repeat(64),
        )
        .expect_err("ハッシュ不一致");
        assert!(error.contains("SHA-256"), "{error}");
        assert!(!bad_archive.exists());
        assert!(!bad_archive.with_extension("part").exists());

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
        fetch("test-http", &source, archive_name, &archive, expected).expect("fetch");
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
            archive: archive_name,
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
    fn reports_unreachable_internal_server() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("空きポート");
        let address = listener.local_addr().expect("アドレス");
        drop(listener);
        let error = Source::Http(format!("http://{address}"))
            .manifest_text()
            .expect_err("接続失敗");
        assert!(error.contains("社内サーバーに接続できません"), "{error}");
    }

    #[test]
    fn every_known_model_has_a_spec() {
        for id in config::MODEL_IDS {
            assert!(spec(id).is_some(), "{id}");
        }
        assert_eq!(SPECS.len(), config::MODEL_IDS.len());
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024 * 3 / 2), "1.5 MB");
    }
}
