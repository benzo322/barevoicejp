//! モデルのダウンロード・展開・削除。
//!
//! sherpa-onnx の公式リリースから tar.bz2 を取得し、`models/` 配下へ
//! 展開する。進捗は UI が読めるようプロセス内で共有する。

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::config::{self, Config};

const RELEASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models";

/// 設定画面に表示するモデル。
pub struct ModelSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub subtitle: &'static str,
    pub archive: &'static str,
    pub size_label: &'static str,
    pub speed: u8,
    pub accuracy: u8,
    pub recommended: bool,
}

/// 表示順に並べたモデル定義。
pub const SPECS: [ModelSpec; 5] = [
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

pub fn spec(id: &str) -> Option<&'static ModelSpec> {
    SPECS.iter().find(|spec| spec.id == id)
}

/// ダウンロード工程。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Downloading,
    Extracting,
    Done,
    Failed,
}

#[derive(Clone, Debug)]
pub struct DownloadState {
    pub phase: Phase,
    pub downloaded: u64,
    pub total: u64,
    pub error: Option<String>,
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

pub fn download_state(id: &str) -> DownloadState {
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
pub fn is_installed(config: &Config, id: &str) -> bool {
    let dir = config.default_model_dir_for(id);
    let files = || {
        fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
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
pub fn installed_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            let Ok(metadata) = entry.metadata() else {
                return 0;
            };
            if metadata.is_dir() {
                installed_bytes(&entry.path())
            } else {
                metadata.len()
            }
        })
        .sum()
}

fn archive_path(id: &str) -> Option<PathBuf> {
    spec(id).map(|spec| config::models_root().join(".cache").join(spec.archive))
}

/// モデルをダウンロードして展開する。完了・失敗時は `done` を呼ぶ。
pub fn download<F>(id: &str, done: F)
where
    F: FnOnce() + Send + 'static,
{
    if spec(id).is_none() {
        return;
    }
    let dest = config::models_root().join(config::model_dir_name(id));
    if dest.exists() {
        update(
            id,
            DownloadState {
                phase: Phase::Done,
                ..DownloadState::default()
            },
        );
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
    let result = std::thread::Builder::new()
        .name(format!("model-download-{id}"))
        .spawn(move || {
            let Some(spec) = spec(&thread_id) else {
                return;
            };
            run_download(&thread_id, spec, &dest);
            done();
        });
    if let Err(exc) = result {
        fail(id, format!("ダウンロードスレッドを開始できません: {exc}"));
    }
}

fn run_download(id: &str, spec: &ModelSpec, dest: &Path) {
    let url = format!("{RELEASE}/{}", spec.archive);
    let cache_dir = config::models_root().join(".cache");
    if let Err(exc) = fs::create_dir_all(&cache_dir) {
        fail(id, format!("キャッシュディレクトリを作成できません: {exc}"));
        return;
    }
    let archive = cache_dir.join(spec.archive);
    if let Err(exc) = fetch(id, &url, &archive) {
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

fn fetch(id: &str, url: &str, archive: &Path) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|exc| format!("ダウンロードできません: {exc}"))?;
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
    let mut reader = response.into_body().into_reader();
    let part = archive.with_extension("part");
    let mut file =
        File::create(&part).map_err(|exc| format!("一時ファイルを作成できません: {exc}"))?;
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
    fs::rename(&part, archive).map_err(|exc| format!("アーカイブを確定できません: {exc}"))?;
    update(
        id,
        DownloadState {
            phase: Phase::Downloading,
            downloaded,
            total: total.max(downloaded),
            error: None,
        },
    );
    Ok(())
}

fn extract(id: &str, spec: &ModelSpec, archive: &Path, dest: &Path) -> Result<(), String> {
    let temp = config::models_root().join(format!(".extract-{id}"));
    if temp.exists() {
        fs::remove_dir_all(&temp)
            .map_err(|exc| format!("古い一時ディレクトリを削除できません: {exc}"))?;
    }
    fs::create_dir_all(&temp).map_err(|exc| format!("一時ディレクトリを作成できません: {exc}"))?;

    let file = File::open(archive).map_err(|exc| format!("アーカイブを開けません: {exc}"))?;
    let decoder = bzip2::read::BzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(&temp)
        .map_err(|exc| format!("アーカイブを展開できません: {exc}"))?;

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

fn archive_metadata(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// モデルを削除して解放したバイト数を返す。
pub fn delete(config: &Config, id: &str) -> Result<u64, String> {
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

pub fn format_bytes(bytes: u64) -> String {
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
