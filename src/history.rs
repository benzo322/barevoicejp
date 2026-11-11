//! 認識テキストの履歴 (`history.json`)。
//!
//! 直近 [`MAX_ENTRIES`] 件だけを保持し、古いものから捨てる。
//! 書き込みは `stats.json` と同じく一時ファイルへ書き切ってから
//! `rename` するため、クラッシュしても既存の履歴は壊れない。

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 履歴ファイル名 (データルート基準)。
pub(crate) const HISTORY_PATH: &str = "history.json";

/// 保持する履歴の最大件数。
pub(crate) const MAX_ENTRIES: usize = 10;

/// 1 件分の認識履歴。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct HistoryEntry {
    /// ローカル日時 (`YYYY-MM-DD HH:MM:SS`)。
    pub(crate) time: String,
    /// 整形後の認識テキスト。
    pub(crate) text: String,
    /// 使用したモデルの表示名。
    pub(crate) model: String,
}

/// 永続化する履歴。新しい順に並べる。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct History {
    pub(crate) entries: Vec<HistoryEntry>,
}

impl History {
    /// 履歴ファイルを読む。存在しない・壊れている場合は空から始める。
    pub(crate) fn load(path: &Path) -> Self {
        let mut history = match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(history) => history,
                Err(exc) => {
                    crate::warn!(
                        "履歴ファイルを解釈できません ({}): {exc}。空の履歴で起動します。",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(exc) => {
                crate::warn!(
                    "履歴ファイルを読み込めません ({}): {exc}。空の履歴で起動します。",
                    path.display()
                );
                Self::default()
            }
        };
        history.entries.truncate(MAX_ENTRIES);
        history
    }

    /// 履歴を JSON としてアトミックに保存する。
    pub(crate) fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            && let Err(exc) = fs::create_dir_all(parent)
        {
            return Err(format!(
                "履歴ディレクトリを作成できません ({}): {exc}",
                parent.display()
            ));
        }
        let text = serde_json::to_vec_pretty(self)
            .map_err(|exc| format!("履歴を JSON に変換できません: {exc}"))?;
        let temp = path.with_extension("json.tmp");
        let mut file = match File::create(&temp) {
            Ok(file) => file,
            Err(exc) => {
                let _ = fs::remove_file(&temp);
                return Err(format!("履歴を保存できません ({}): {exc}", temp.display()));
            }
        };
        if let Err(exc) = file.write_all(&text).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(format!("履歴を保存できません ({}): {exc}", temp.display()));
        }
        drop(file);
        if let Err(exc) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(format!("履歴を保存できません ({}): {exc}", path.display()));
        }
        Ok(())
    }

    /// 最新の 1 件を先頭へ追加し、上限を超えた古い履歴を捨てる。
    pub(crate) fn record(&mut self, entry: HistoryEntry) {
        if entry.text.is_empty() {
            return;
        }
        self.entries.insert(0, entry);
        self.entries.truncate(MAX_ENTRIES);
    }

    /// 全件削除する。
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// ローカル日時を `YYYY-MM-DD HH:MM:SS` で返す。
#[cfg(windows)]
pub(crate) fn local_datetime() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let mut time: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        GetLocalTime(&mut time);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
    )
}

/// Windows 以外 (テスト・ビルド確認用) は UTC 日時を返す。
#[cfg(not(windows))]
pub(crate) fn local_datetime() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let time = seconds % 86_400;
    format!(
        "{} {:02}:{:02}:{:02}",
        crate::stats::local_date(),
        time / 3_600,
        (time / 60) % 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("時刻")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "barevoicejp-history-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("一時ディレクトリ");
        dir
    }

    fn entry(text: &str) -> HistoryEntry {
        HistoryEntry {
            time: "2026-11-11 12:34:56".to_string(),
            text: text.to_string(),
            model: "SenseVoice".to_string(),
        }
    }

    #[test]
    fn keeps_only_latest_ten_newest_first() {
        let mut history = History::default();
        for index in 0..12 {
            history.record(entry(&format!("text {index}")));
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        assert_eq!(history.entries[0].text, "text 11");
        assert_eq!(history.entries[9].text, "text 2");
    }

    #[test]
    fn empty_text_is_not_recorded() {
        let mut history = History::default();
        history.record(entry(""));
        assert!(history.entries.is_empty());
    }

    #[test]
    fn saves_and_loads_roundtrip_atomically() {
        let dir = temp_dir("roundtrip");
        let path = dir.join(HISTORY_PATH);
        let mut history = History::default();
        history.record(HistoryEntry {
            time: "2026-11-11 12:34:56".to_string(),
            text: "こんにちは".to_string(),
            model: "SenseVoice".to_string(),
        });

        history.save(&path).expect("保存");
        assert_eq!(History::load(&path), history);
        assert!(!path.with_extension("json.tmp").exists());
        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn corrupt_file_starts_empty() {
        let dir = temp_dir("corrupt");
        let path = dir.join(HISTORY_PATH);
        fs::write(&path, b"{not json").expect("書き込み");
        assert_eq!(History::load(&path), History::default());
        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn timestamp_has_fixed_shape() {
        let value = local_datetime();
        assert_eq!(value.chars().count(), 19);
        assert_eq!(value.as_bytes()[4], b'-');
        assert_eq!(value.as_bytes()[7], b'-');
        assert_eq!(value.as_bytes()[10], b' ');
        assert_eq!(value.as_bytes()[13], b':');
        assert_eq!(value.as_bytes()[16], b':');
    }
}
