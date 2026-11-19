//! GitHub Releases を使ったセルフアップデート。
//!
//! 流れ:
//! 1. `releases/latest` を読み、`Cargo.toml` のバージョンと比較する
//! 2. `BAREVoiceJP-<tag>-win64.zip` と `.sha256` を取得して検証する
//! 3. ZIP をデータフォルダの `updates\` へ展開する
//! 4. 終了を待って exe と DLL を差し替える PowerShell を起動する
//!
//! 実行中の exe / ロード済み DLL は Windows がロックするため、Rust 側では
//! 差し替えない。ヘルパースクリプトがプロセス終了を待ち、旧ファイルを
//! `.old` へ退避してから新しいファイルを移動する。失敗時は退避した
//! ファイルを戻して元のバージョンで起動し直す。

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::app::Shared;
use crate::config;

/// GitHub のリポジトリ。
const REPOSITORY: &str = "benzo322/barevoicejp";
/// 最新リリース API。
const RELEASES_LATEST: &str = "https://api.github.com/repos/benzo322/barevoicejp/releases/latest";
/// GitHub API が要求する User-Agent。
const USER_AGENT: &str = concat!("BAREVoiceJP/", env!("CARGO_PKG_VERSION"));
/// 配布 ZIP の上限 (壊れた/巨大なファイルでディスクを埋めない)。
const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;
/// 展開後の上限 (圧縮爆弾によるディスク枯渇を防ぐ)。
const MAX_EXTRACTED_BYTES: u64 = 512 * 1024 * 1024;
/// `.sha256` の上限。
const MAX_SHA_BYTES: u64 = 4 * 1024;
/// 更新後も必ず差し替える実行ファイル。
const EXECUTABLE: &str = "BAREVoiceJP.exe";
/// 配布 ZIP に必ず含まれる DLL。1 つでも欠けたら更新を中止する。
const REQUIRED_DLLS: [&str; 3] = [
    "sherpa-onnx-c-api.dll",
    "onnxruntime.dll",
    "onnxruntime_providers_shared.dll",
];
/// ユーザーが編集し得るファイル。インストール先に既にあれば上書きしない。
const KEEP_EXISTING: [&str; 3] = ["words.txt", "replacements.json", "hotwords.txt"];
/// 更新 ZIP から取り込まない実行時データ。
const FORBIDDEN: [&str; 4] = ["config.toml", "history.json", "stats.json", "portable.txt"];

/// 更新処理の段階。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Verifying,
    Extracting,
    Applying,
    Failed,
}

impl UpdatePhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Checking => "checking",
            Self::UpToDate => "up_to_date",
            Self::Available => "available",
            Self::Downloading => "downloading",
            Self::Verifying => "verifying",
            Self::Extracting => "extracting",
            Self::Applying => "applying",
            Self::Failed => "failed",
        }
    }
}

/// 設定画面へ渡す更新状態。
#[derive(Clone, Debug)]
pub(crate) struct UpdateState {
    pub(crate) phase: UpdatePhase,
    pub(crate) current: String,
    pub(crate) latest: Option<String>,
    pub(crate) release_url: Option<String>,
    pub(crate) downloaded: u64,
    pub(crate) total: u64,
    pub(crate) error: Option<String>,
    /// 以下は UI へは渡さない。更新実行時に使う。
    pub(crate) asset_name: Option<String>,
    pub(crate) download_url: Option<String>,
    pub(crate) sha_url: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            phase: UpdatePhase::Idle,
            current: env!("CARGO_PKG_VERSION").to_string(),
            latest: None,
            release_url: None,
            downloaded: 0,
            total: 0,
            error: None,
            asset_name: None,
            download_url: None,
            sha_url: None,
        }
    }
}

/// GitHub Releases の `latest` レスポンス (必要な項目だけ)。
#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
struct Asset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// 更新を確認する。結果は共有状態へ書き、最後に `notify` を呼ぶ。
///
/// `silent` が true のとき (起動時の自動確認) は通信エラーを UI に出さない。
pub(crate) fn check<F>(shared: Arc<Shared>, notify: F, silent: bool)
where
    F: Fn() + Send + 'static,
{
    if !shared.begin_update() {
        return;
    }
    shared.update_mut(|state| {
        state.phase = UpdatePhase::Checking;
        state.error = None;
        state.latest = None;
        state.release_url = None;
        state.asset_name = None;
        state.download_url = None;
        state.sha_url = None;
        state.downloaded = 0;
        state.total = 0;
    });

    let worker = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("BAREVoiceJP-update-check".to_string())
        .spawn(move || {
            match fetch_latest() {
                Ok(release) => match resolve_update(&release, &worker.update_state().current) {
                    Ok(Some(ready)) => worker.update_mut(|state| {
                        state.phase = UpdatePhase::Available;
                        state.latest = Some(ready.tag.clone());
                        state.release_url = Some(ready.release_url.clone());
                        state.asset_name = Some(ready.asset_name.clone());
                        state.download_url = Some(ready.download_url.clone());
                        state.sha_url = Some(ready.sha_url.clone());
                        state.total = ready.size;
                        state.downloaded = 0;
                        state.error = None;
                    }),
                    Ok(None) => worker.update_mut(|state| {
                        state.phase = UpdatePhase::UpToDate;
                        state.latest = Some(release.tag_name.clone());
                        state.release_url = Some(release.release_url());
                        state.asset_name = None;
                        state.download_url = None;
                        state.sha_url = None;
                        state.error = None;
                    }),
                    Err(exc) => worker.update_mut(|state| {
                        if silent {
                            state.phase = UpdatePhase::Idle;
                            state.error = None;
                        } else {
                            state.phase = UpdatePhase::Failed;
                            state.error = Some(exc);
                        }
                    }),
                },
                Err(exc) => worker.update_mut(|state| {
                    if silent {
                        state.phase = UpdatePhase::Idle;
                        state.error = None;
                    } else {
                        state.phase = UpdatePhase::Failed;
                        state.error = Some(exc);
                    }
                }),
            }
            worker.end_update();
            notify();
        });

    if let Err(exc) = spawned {
        shared.update_mut(|state| {
            state.phase = UpdatePhase::Failed;
            state.error = Some(format!("更新確認スレッドを開始できません: {exc}"));
        });
        shared.end_update();
    }
}

/// ダウンロード済みの更新を適用する。
///
/// 成功すると `ready` を呼ぶ (イベントループ側で終了処理を行う)。
/// 失敗時は状態を `Failed` にして `notify` を呼ぶ。
pub(crate) fn apply<F, G>(shared: Arc<Shared>, notify: F, ready: G)
where
    F: Fn() + Send + 'static,
    G: FnOnce() + Send + 'static,
{
    if !shared.begin_update() {
        return;
    }
    shared.update_mut(|state| {
        state.phase = UpdatePhase::Downloading;
        state.downloaded = 0;
        state.error = None;
    });

    let worker = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("BAREVoiceJP-update-apply".to_string())
        .spawn(move || match apply_worker(&worker) {
            Ok(()) => {
                worker.update_mut(|state| state.phase = UpdatePhase::Applying);
                worker.end_update();
                ready();
            }
            Err(exc) => {
                worker.update_mut(|state| {
                    state.phase = UpdatePhase::Failed;
                    state.error = Some(exc);
                });
                worker.end_update();
                notify();
            }
        });

    if let Err(exc) = spawned {
        shared.update_mut(|state| {
            state.phase = UpdatePhase::Failed;
            state.error = Some(format!("更新スレッドを開始できません: {exc}"));
        });
        shared.end_update();
    }
}

/// 更新アセットの情報。
struct ReadyUpdate {
    tag: String,
    release_url: String,
    asset_name: String,
    download_url: String,
    sha_url: String,
    size: u64,
}

impl Release {
    fn release_url(&self) -> String {
        if self.html_url.is_empty() {
            format!(
                "https://github.com/{REPOSITORY}/releases/tag/{}",
                self.tag_name
            )
        } else {
            self.html_url.clone()
        }
    }
}

/// リリースから更新用 ZIP と `.sha256` を選ぶ。
///
/// 戻り値が `None` なら最新版 (更新不要)。
fn resolve_update(release: &Release, current: &str) -> Result<Option<ReadyUpdate>, String> {
    if !is_newer(&release.tag_name, current) {
        return Ok(None);
    }
    let expected = format!("BAREVoiceJP-{}-win64.zip", release.tag_name);
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == expected)
        .or_else(|| {
            release.assets.iter().find(|asset| {
                asset.name.starts_with("BAREVoiceJP-") && asset.name.ends_with("-win64.zip")
            })
        })
        .ok_or_else(|| {
            format!(
                "リリース {} に Windows 用 ZIP ({expected}) がありません",
                release.tag_name
            )
        })?;
    let sha = release
        .assets
        .iter()
        .find(|item| item.name == format!("{}.sha256", asset.name))
        .ok_or_else(|| {
            format!(
                "リリース {} に {} の SHA-256 がありません",
                release.tag_name, asset.name
            )
        })?;
    Ok(Some(ReadyUpdate {
        tag: release.tag_name.clone(),
        release_url: release.release_url(),
        asset_name: asset.name.clone(),
        download_url: asset.browser_download_url.clone(),
        sha_url: sha.browser_download_url.clone(),
        size: asset.size,
    }))
}

/// GitHub の最新リリースを取得する。
fn fetch_latest() -> Result<Release, String> {
    let agent = http_agent();
    let response = agent
        .get(RELEASES_LATEST)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .config()
        .https_only(true)
        .build()
        .call()
        .map_err(|exc| format!("最新バージョンを取得できません: {exc}"))?;
    let mut body = response.into_body();
    let text = body
        .read_to_string()
        .map_err(|exc| format!("更新情報を読み込めません: {exc}"))?;
    let release: Release =
        serde_json::from_str(&text).map_err(|exc| format!("更新情報を解釈できません: {exc}"))?;
    if release.tag_name.trim().is_empty() {
        return Err("更新情報にバージョンがありません".to_string());
    }
    Ok(release)
}

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .timeout_resolve(Some(Duration::from_secs(10)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(30 * 60)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// `1.2.3` / `v1.2.3` を数値 3 つへ変換する。解釈できなければ `None`。
fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.trim();
    let core = value
        .strip_prefix(['v', 'V'])
        .unwrap_or(value)
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// `latest` が `current` より新しいか。解釈できない場合は更新しない。
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// 更新をダウンロード・検証・展開し、ヘルパースクリプトを起動する。
fn apply_worker(shared: &Shared) -> Result<(), String> {
    let state = shared.update_state();
    let (Some(tag), Some(download_url), Some(sha_url), Some(asset_name)) = (
        state.latest.clone(),
        state.download_url.clone(),
        state.sha_url.clone(),
        state.asset_name.clone(),
    ) else {
        return Err("更新情報が不足しています。もう一度「更新を確認」してください".to_string());
    };

    let install =
        config::exe_dir().ok_or_else(|| "実行ファイルの場所を取得できません".to_string())?;
    ensure_writable(&install)?;

    let work = update_dir(&tag);
    if work.exists() {
        fs::remove_dir_all(&work).map_err(|exc| {
            format!(
                "前回の更新ファイルを削除できません ({}): {exc}",
                work.display()
            )
        })?;
    }
    fs::create_dir_all(&work)
        .map_err(|exc| format!("更新用フォルダを作成できません ({}): {exc}", work.display()))?;

    let archive = work.join("update.zip");
    let agent = http_agent();
    download_asset(shared, &agent, &download_url, &archive)?;

    shared.update_mut(|state| state.phase = UpdatePhase::Verifying);
    let expected = fetch_expected_sha(&agent, &sha_url)?;
    verify_sha256(&archive, &expected).map_err(|exc| {
        let _ = fs::remove_file(&archive);
        format!("{asset_name} の検証に失敗しました: {exc}")
    })?;

    shared.update_mut(|state| state.phase = UpdatePhase::Extracting);
    let stage = work.join("stage");
    let files = extract_archive(&archive, &stage)?;
    validate_stage(&stage)?;

    let script = temp_script_path();
    let log = config::logs_dir().join("update.log");
    if let Err(exc) = fs::create_dir_all(config::logs_dir()) {
        return Err(format!("ログフォルダを作成できません: {exc}"));
    }
    write_updater(&tag, &install, &stage, &files, &log, &script)?;
    spawn_updater(&script)?;
    // 展開済み ZIP は不要。stage はスクリプトが削除する。
    let _ = fs::remove_file(&archive);
    Ok(())
}

/// インストール先へ書き込めるか確認する。
///
/// Program Files など書き込み不可の場所へ置かれている場合、更新後に
/// 起動できなくなるより先に UI へ理由を返す。
fn ensure_writable(install: &Path) -> Result<(), String> {
    let probe = install.join(".barevoicejp-write-test");
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(exc) => Err(format!(
            "インストール先に書き込めません ({}): {exc}。\
             BAREVoiceJP をユーザーフォルダ配下へ置き直してください。",
            install.display()
        )),
    }
}

/// `updates/<tag>` を作る。タグはファイル名に使える文字だけ残す。
fn update_dir(tag: &str) -> PathBuf {
    let safe: String = tag
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    config::data_root()
        .join("updates")
        .join(if safe.is_empty() { "latest" } else { &safe })
}

/// ZIP を進捗を更新しながら保存する。
fn download_asset(
    shared: &Shared,
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    let response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .config()
        .https_only(true)
        .build()
        .call()
        .map_err(|exc| format!("更新ファイルを取得できません: {exc}"))?;
    let total = response
        .body()
        .content_length()
        .or_else(|| {
            response
                .headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0);
    if total > MAX_UPDATE_BYTES {
        return Err(format!("更新ファイルが大きすぎます ({} バイト)", total));
    }
    shared.update_mut(|state| {
        state.phase = UpdatePhase::Downloading;
        state.total = total;
        state.downloaded = 0;
    });

    let mut reader = response.into_body().into_reader();
    let mut file = File::create(dest)
        .map_err(|exc| format!("更新ファイルを作成できません ({}): {exc}", dest.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut downloaded = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|exc| format!("更新ファイルを受信できません: {exc}"))?;
        if read == 0 {
            break;
        }
        downloaded = downloaded.saturating_add(read as u64);
        if downloaded > MAX_UPDATE_BYTES {
            let _ = fs::remove_file(dest);
            return Err("更新ファイルが大きすぎます。ダウンロードを中止しました".to_string());
        }
        file.write_all(&buffer[..read])
            .map_err(|exc| format!("更新ファイルを書き込めません: {exc}"))?;
        shared.update_mut(|state| state.downloaded = downloaded);
    }
    file.sync_all()
        .map_err(|exc| format!("更新ファイルを確定できません: {exc}"))?;
    if total > 0 && downloaded != total {
        let _ = fs::remove_file(dest);
        return Err(format!(
            "更新ファイルのサイズが想定と異なります (期待 {total} / 実際 {downloaded} バイト)"
        ));
    }
    Ok(())
}

/// `.sha256` アセットから期待するハッシュを読む。
fn fetch_expected_sha(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .config()
        .https_only(true)
        .build()
        .call()
        .map_err(|exc| format!("SHA-256 を取得できません: {exc}"))?;
    let mut body = response.into_body();
    let text = body
        .with_config()
        .limit(MAX_SHA_BYTES)
        .read_to_string()
        .map_err(|exc| format!("SHA-256 を読み込めません: {exc}"))?;
    let hash = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("配布元の SHA-256 が不正です".to_string());
    }
    Ok(hash)
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let mut file = File::open(path)
        .map_err(|exc| format!("更新ファイルを開けません ({}): {exc}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|exc| format!("更新ファイルを読めません: {exc}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        return Err(format!(
            "SHA-256 が一致しません (期待 {expected} / 実際 {actual})"
        ));
    }
    Ok(())
}

/// ZIP のルート直下のファイルだけを `stage` へ展開し、名前を返す。
///
/// パス区切りを含むエントリや実行時データは取り込まない。
fn extract_archive(archive: &Path, stage: &Path) -> Result<Vec<String>, String> {
    let file = File::open(archive)
        .map_err(|exc| format!("更新 ZIP を開けません ({}): {exc}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|exc| format!("更新 ZIP を読めません: {exc}"))?;
    fs::create_dir_all(stage)
        .map_err(|exc| format!("展開先を作成できません ({}): {exc}", stage.display()))?;

    let mut files = Vec::new();
    let mut extracted = 0u64;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|exc| format!("更新 ZIP のエントリを読めません: {exc}"))?;
        if entry.is_dir() {
            continue;
        }
        let Some(path) = entry.enclosed_name() else {
            return Err("更新 ZIP に不正なパスがあります".to_string());
        };
        let relative = path.strip_prefix("BAREVoiceJP").unwrap_or(&path);
        let mut components = relative.components();
        let Some(std::path::Component::Normal(name)) = components.next() else {
            continue;
        };
        // ルート直下のファイルだけを対象にする (`sub\file` は無視)。
        if components.next().is_some() {
            continue;
        }
        let Some(name) = name.to_str() else {
            continue;
        };
        if FORBIDDEN.contains(&name) {
            continue;
        }
        let target = stage.join(name);
        let mut writer = File::create(&target)
            .map_err(|exc| format!("更新ファイルを作成できません ({}): {exc}", target.display()))?;
        let remaining = MAX_EXTRACTED_BYTES.saturating_sub(extracted);
        let copied = std::io::copy(&mut (&mut entry).take(remaining + 1), &mut writer)
            .map_err(|exc| format!("更新 ZIP を展開できません: {exc}"))?;
        if copied > remaining {
            return Err("更新 ZIP の展開サイズが上限を超えています".to_string());
        }
        extracted = extracted.saturating_add(copied);
        files.push(name.to_string());
    }
    files.sort();
    Ok(files)
}

/// 展開結果に実行に必要なファイルが揃っているか。
fn validate_stage(stage: &Path) -> Result<(), String> {
    if !stage.join(EXECUTABLE).is_file() {
        return Err(format!("更新 ZIP に {EXECUTABLE} がありません"));
    }
    for name in REQUIRED_DLLS {
        if !stage.join(name).is_file() {
            return Err(format!("更新 ZIP に {name} がありません"));
        }
    }
    Ok(())
}

/// PowerShell の単一引用符文字列。
fn ps_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn ps_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("'{}'", value.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 差し替えスクリプトの一時パス。
fn temp_script_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "BAREVoiceJP-update-{}-{}.ps1",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0)
    ))
}

/// 終了後にファイルを差し替える PowerShell を生成する。
fn write_updater(
    tag: &str,
    install: &Path,
    stage: &Path,
    files: &[String],
    log: &Path,
    script: &Path,
) -> Result<(), String> {
    let exe = install.join(EXECUTABLE);
    let keep: Vec<String> = KEEP_EXISTING.iter().map(|name| name.to_string()).collect();
    let content = UPDATER_TEMPLATE
        .replace("__PID__", &std::process::id().to_string())
        .replace("__INSTALL__", &ps_quote(install))
        .replace("__STAGE__", &ps_quote(stage))
        .replace("__EXE__", &ps_quote(&exe))
        .replace("__LOG__", &ps_quote(log))
        .replace("__FILES__", &ps_list(files))
        .replace("__KEEP__", &ps_list(&keep))
        .replace("__TAG__", &tag.replace('\'', "''"));

    // Windows PowerShell 5.1 は BOM の無いスクリプトをシステムの ANSI
    // コードページで解釈するため、日本語を含むパスが壊れる。BOM を付けて
    // UTF-8 であることを明示する。
    let mut bytes = Vec::with_capacity(3 + content.len());
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(content.as_bytes());
    fs::write(script, bytes).map_err(|exc| {
        format!(
            "更新スクリプトを作成できません ({}): {exc}",
            script.display()
        )
    })
}

#[cfg(windows)]
fn spawn_updater(script: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let powershell = std::env::var_os("SystemRoot")
        .map(|root| {
            PathBuf::from(root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("powershell.exe"));
    std::process::Command::new(powershell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(script)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|exc| format!("更新プログラムを起動できません: {exc}"))?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_updater(_script: &Path) -> Result<(), String> {
    Err("このビルドではセルフアップデートを実行できません".to_string())
}

/// 差し替え本体。Rust の `format!` を避けるためプレースホルダを置換する。
const UPDATER_TEMPLATE: &str = r#"$ErrorActionPreference = 'Stop'
$targetPid = __PID__
$install = __INSTALL__
$stage = __STAGE__
$exe = __EXE__
$log = __LOG__
$tag = '__TAG__'
$files = @(__FILES__)
$keepExisting = @(__KEEP__)

function Write-Log([string]$message) {
  try {
    Add-Content -LiteralPath $log -Value ("[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $message) -Encoding UTF8
  } catch {}
}

Write-Log "waiting for BAREVoiceJP (pid $targetPid)"
$backups = @()
try {
  while (Get-Process -Id $targetPid -ErrorAction SilentlyContinue) {
    Start-Sleep -Milliseconds 200
  }
  Start-Sleep -Milliseconds 500

  foreach ($name in $files) {
    $src = Join-Path $stage $name
    if (-not (Test-Path -LiteralPath $src)) { continue }
    $dst = Join-Path $install $name
    if (($keepExisting -contains $name) -and (Test-Path -LiteralPath $dst)) { continue }
    if (Test-Path -LiteralPath $dst) {
      $bak = "$dst.old"
      if (Test-Path -LiteralPath $bak) {
        Remove-Item -LiteralPath $bak -Force -ErrorAction SilentlyContinue
      }
      $moved = $false
      for ($i = 0; $i -lt 60 -and -not $moved; $i++) {
        try {
          Move-Item -LiteralPath $dst -Destination $bak -Force
          $moved = $true
        } catch {
          Start-Sleep -Milliseconds 500
        }
      }
      if (-not $moved) { throw "cannot move $dst" }
      $backups += ,@($dst, $bak)
    }
    Move-Item -LiteralPath $src -Destination $dst -Force
  }

  foreach ($pair in $backups) {
    Remove-Item -LiteralPath $pair[1] -Force -ErrorAction SilentlyContinue
  }
  Write-Log "applied $tag"
  Start-Process -FilePath $exe -WorkingDirectory $install
} catch {
  Write-Log ("failed: " + $_.Exception.Message)
  foreach ($pair in $backups) {
    $dst = $pair[0]
    $bak = $pair[1]
    if (Test-Path -LiteralPath $bak) {
      Remove-Item -LiteralPath $dst -Force -ErrorAction SilentlyContinue
      Move-Item -LiteralPath $bak -Destination $dst -Force -ErrorAction SilentlyContinue
    }
  }
  if (Test-Path -LiteralPath $exe) {
    Start-Process -FilePath $exe -WorkingDirectory $install
  }
}

Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
        assert!(!is_newer("not-a-version", "0.2.0"));
        assert!(!is_newer("v0.3.0", "not-a-version"));
        assert_eq!(parse_version("v1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn selects_release_assets() {
        let release = Release {
            tag_name: "v0.3.0".to_string(),
            html_url: String::new(),
            assets: vec![
                Asset {
                    name: "BAREVoiceJP-v0.3.0-win64.zip".to_string(),
                    browser_download_url: "https://example.com/app.zip".to_string(),
                    size: 123,
                },
                Asset {
                    name: "BAREVoiceJP-v0.3.0-win64.zip.sha256".to_string(),
                    browser_download_url: "https://example.com/app.zip.sha256".to_string(),
                    size: 80,
                },
            ],
        };
        let ready = resolve_update(&release, "0.2.0")
            .expect("解釈")
            .expect("更新あり");
        assert_eq!(ready.tag, "v0.3.0");
        assert_eq!(ready.download_url, "https://example.com/app.zip");
        assert_eq!(ready.sha_url, "https://example.com/app.zip.sha256");
        assert_eq!(ready.size, 123);
        assert!(resolve_update(&release, "0.3.0").expect("解釈").is_none());
    }

    #[test]
    fn rejects_release_without_windows_asset() {
        let release = Release {
            tag_name: "v0.3.0".to_string(),
            html_url: String::new(),
            assets: Vec::new(),
        };
        assert!(resolve_update(&release, "0.2.0").is_err());
    }

    fn temp_dir(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("時刻")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "barevoicejp-update-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("一時ディレクトリ");
        dir
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).expect("ZIP 作成");
        let mut writer = zip::ZipWriter::new(file);
        for (name, content) in entries {
            writer
                .start_file(
                    *name,
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .expect("エントリ");
            writer.write_all(content).expect("書き込み");
        }
        writer.finish().expect("ZIP 完了");
    }

    #[test]
    fn extracts_windows_package_and_skips_runtime_data() {
        let dir = temp_dir("extract");
        let archive = dir.join("update.zip");
        write_zip(
            &archive,
            &[
                ("BAREVoiceJP/BAREVoiceJP.exe", b"exe"),
                ("BAREVoiceJP/sherpa-onnx-c-api.dll", b"dll"),
                ("BAREVoiceJP/onnxruntime.dll", b"dll"),
                ("BAREVoiceJP/onnxruntime_providers_shared.dll", b"dll"),
                ("BAREVoiceJP/words.txt", b"words"),
                ("BAREVoiceJP/config.toml", b"must not be copied"),
                ("BAREVoiceJP/history.json", b"must not be copied"),
                ("BAREVoiceJP/sub/extra.dll", b"nested"),
            ],
        );
        let stage = dir.join("stage");
        let files = extract_archive(&archive, &stage).expect("展開");
        assert_eq!(
            files,
            vec![
                "BAREVoiceJP.exe",
                "onnxruntime.dll",
                "onnxruntime_providers_shared.dll",
                "sherpa-onnx-c-api.dll",
                "words.txt",
            ]
        );
        validate_stage(&stage).expect("必須ファイル");
        assert!(!stage.join("config.toml").exists());
        assert!(!stage.join("history.json").exists());
        assert!(!stage.join("sub").exists());
        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn rejects_path_traversal_in_update_zip() {
        let dir = temp_dir("traversal");
        let archive = dir.join("update.zip");
        write_zip(&archive, &[("BAREVoiceJP/../../evil.txt", b"evil")]);
        let stage = dir.join("stage");
        assert!(extract_archive(&archive, &stage).is_err());
        assert!(!dir.join("evil.txt").exists());
        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn writes_updater_with_quoted_paths_and_file_list() {
        let dir = temp_dir("script");
        let install = dir.join("it's install");
        let stage = dir.join("stage");
        let log = dir.join("update.log");
        let script = dir.join("apply.ps1");
        fs::create_dir_all(&install).expect("インストール先");
        fs::create_dir_all(&stage).expect("ステージ");
        let files = vec!["BAREVoiceJP.exe".to_string(), "words.txt".to_string()];

        write_updater("v9.9.9", &install, &stage, &files, &log, &script).expect("生成");
        let raw = fs::read(&script).expect("読み込み");
        assert_eq!(&raw[..3], &[0xEF, 0xBB, 0xBF], "UTF-8 BOM が必要");
        let text = String::from_utf8_lossy(&raw[3..]).into_owned();

        assert!(text.contains("it''s install"), "{text}");
        assert!(text.contains("$files = @('BAREVoiceJP.exe', 'words.txt')"));
        assert!(
            text.contains("$keepExisting = @('words.txt', 'replacements.json', 'hotwords.txt')")
        );
        assert!(text.contains("$targetPid = "));
        assert!(text.contains("$tag = 'v9.9.9'"));
        assert!(!text.contains("__INSTALL__"));
        assert!(!text.contains("__FILES__"));
        fs::remove_dir_all(dir).expect("後片付け");
    }
}
