//! 累積入力実績 (`stats.json`)。
//!
//! 保存するのは文字数と発話回数だけ。認識結果の本文・音声・時刻は含めない。
//! 書き込みは同じディレクトリの一時ファイル → `rename` の順で行い、
//! クラッシュや電源断で既存の統計が半端な JSON にならないようにする。

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 統計ファイル名 (データルート基準)。
pub(crate) const STATS_PATH: &str = "stats.json";

/// タイピング削減時間の換算に使う入力速度 (文字/分)。
///
/// 日本語の一般的なビジネスタイピング (200〜250 文字/分) の下限を採用し、
/// 控えめな「削減時間」を表示する。
pub(crate) const TYPING_CHARS_PER_MINUTE: f64 = 200.0;

/// 認識結果の文字数 (Unicode スカラー値単位)。
///
/// 結合絵文字などは複数のスカラー値として数える。書記素単位ではなく、
/// 保存する本文を持たないための最小限のカウント方法。
pub(crate) fn char_count(text: &str) -> u64 {
    text.chars().count() as u64
}

/// 永続化する累積実績。
///
/// フィールドを追加しても古い `stats.json` を読めるよう `serde(default)` を付ける。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Stats {
    /// アプリ導入からの総入力文字数。
    pub(crate) total_chars: u64,
    /// 貼り付けに成功した発話の総回数。
    pub(crate) total_sessions: u64,
    /// 当日の入力文字数。
    pub(crate) today_chars: u64,
    /// `today_chars` が属するローカル日付 (`YYYY-MM-DD`)。
    pub(crate) today_date: String,
}

impl Stats {
    /// 統計ファイルを読む。存在しない・壊れている場合は 0 から始める。
    pub(crate) fn load(path: &Path) -> Self {
        match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(stats) => stats,
                Err(exc) => {
                    crate::warn!(
                        "統計ファイルを解釈できません ({}): {exc}。0 から集計します。",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(exc) => {
                crate::warn!(
                    "統計ファイルを読み込めません ({}): {exc}。0 から集計します。",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// 統計を JSON としてアトミックに保存する。
    ///
    /// 一時ファイルへ書き切って `sync_all` した後に置き換えるため、
    /// 途中で落ちても `stats.json` は旧内容か新内容のどちらかになる。
    pub(crate) fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            && let Err(exc) = fs::create_dir_all(parent)
        {
            return Err(format!(
                "統計ディレクトリを作成できません ({}): {exc}",
                parent.display()
            ));
        }
        let text = serde_json::to_vec_pretty(self)
            .map_err(|exc| format!("統計を JSON に変換できません: {exc}"))?;
        let temp = path.with_extension("json.tmp");
        let mut file = match fs::File::create(&temp) {
            Ok(file) => file,
            Err(exc) => {
                let _ = fs::remove_file(&temp);
                return Err(format!("統計を保存できません ({}): {exc}", temp.display()));
            }
        };
        if let Err(exc) = file.write_all(&text).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(format!("統計を保存できません ({}): {exc}", temp.display()));
        }
        drop(file);
        if let Err(exc) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(format!("統計を保存できません ({}): {exc}", path.display()));
        }
        Ok(())
    }

    /// 日付が変わっていれば当日分を 0 に戻す。戻したときだけ true。
    pub(crate) fn roll_day(&mut self, today: &str) -> bool {
        if self.today_date == today {
            return false;
        }
        self.today_date = today.to_string();
        self.today_chars = 0;
        true
    }

    /// 貼り付けに成功した 1 発話を加算する。
    ///
    /// 日付をまたいだ最初の発話でも当日分が前日へ混ざらないよう、
    /// 加算前に当日日付を確認する。
    pub(crate) fn record(&mut self, chars: u64, today: &str) {
        self.roll_day(today);
        self.total_chars = self.total_chars.saturating_add(chars);
        self.total_sessions = self.total_sessions.saturating_add(1);
        self.today_chars = self.today_chars.saturating_add(chars);
    }

    /// 200 文字/分で換算した推定タイピング削減時間 (分)。
    pub(crate) fn estimated_saved_minutes(&self) -> f64 {
        self.total_chars as f64 / TYPING_CHARS_PER_MINUTE
    }
}

/// ローカル日付を `YYYY-MM-DD` で返す。
#[cfg(windows)]
pub(crate) fn local_date() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let mut time: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        GetLocalTime(&mut time);
    }
    format!("{:04}-{:02}-{:02}", time.wYear, time.wMonth, time.wDay)
}

/// Windows 以外 (テスト・ビルド確認用) は UTC 日付を返す。
#[cfg(not(windows))]
pub(crate) fn local_date() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_secs() / 86_400) as i64)
        .unwrap_or(0);
    format_days(days)
}

/// Unix epoch からの日数を暦日へ変換する (Howard Hinnant の civil_from_days)。
#[cfg(not(windows))]
fn format_days(days_since_epoch: i64) -> String {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// テストごとに衝突しない一時ディレクトリを作る。
    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("時刻")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "barevoicejp-stats-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("一時ディレクトリ");
        dir
    }

    #[test]
    fn counts_unicode_scalar_values() {
        assert_eq!(char_count("AI"), 2);
        assert_eq!(char_count("日本語"), 3);
        assert_eq!(char_count("a🌏"), 2);
        assert_eq!(char_count("a\nb"), 3);
        assert_eq!(char_count(""), 0);
    }

    #[test]
    fn records_chars_and_sessions() {
        let mut stats = Stats::default();
        stats.record(5, "2026-10-25");
        stats.record(7, "2026-10-25");
        assert_eq!(stats.total_chars, 12);
        assert_eq!(stats.total_sessions, 2);
        assert_eq!(stats.today_chars, 12);
        assert_eq!(stats.today_date, "2026-10-25");
    }

    #[test]
    fn rolls_today_over_without_touching_totals() {
        let mut stats = Stats {
            total_chars: 1_000,
            total_sessions: 9,
            today_chars: 120,
            today_date: "2026-10-24".to_string(),
        };
        assert!(stats.roll_day("2026-10-25"));
        assert_eq!(stats.today_chars, 0);
        assert_eq!(stats.today_date, "2026-10-25");
        assert_eq!(stats.total_chars, 1_000);
        assert_eq!(stats.total_sessions, 9);

        // 同じ日付では何もしない (当日分を消さない)。
        assert!(!stats.roll_day("2026-10-25"));
        stats.record(30, "2026-10-25");
        assert_eq!(stats.today_chars, 30);
        assert_eq!(stats.total_chars, 1_030);
    }

    #[test]
    fn record_resets_stale_today_before_adding() {
        let mut stats = Stats {
            total_chars: 500,
            total_sessions: 3,
            today_chars: 200,
            today_date: "2026-10-24".to_string(),
        };
        stats.record(10, "2026-10-25");
        assert_eq!(stats.today_chars, 10);
        assert_eq!(stats.total_chars, 510);
        assert_eq!(stats.total_sessions, 4);
    }

    #[test]
    fn estimates_typing_time_at_200_chars_per_minute() {
        let stats = Stats {
            total_chars: 12_340,
            ..Stats::default()
        };
        let minutes = stats.estimated_saved_minutes();
        assert!((minutes - 61.7).abs() < 0.01, "{minutes}");
        assert_eq!(Stats::default().estimated_saved_minutes(), 0.0);
    }

    #[test]
    fn saves_and_loads_atomically_without_text_fields() {
        let dir = temp_dir("roundtrip");
        let path = dir.join(STATS_PATH);
        let stats = Stats {
            total_chars: 12_340,
            total_sessions: 42,
            today_chars: 120,
            today_date: "2026-10-25".to_string(),
        };

        stats.save(&path).expect("保存");
        assert!(path.is_file());
        assert!(
            !path.with_extension("json.tmp").exists(),
            "一時ファイルが残っている"
        );
        assert_eq!(Stats::load(&path), stats);

        // 既存ファイルを置き換えても壊れない。
        let mut updated = stats.clone();
        updated.total_chars += 1;
        updated.save(&path).expect("上書き保存");
        assert_eq!(Stats::load(&path), updated);
        assert!(!path.with_extension("json.tmp").exists());

        // 保存形式に本文らしき文字列が含まれないこと。
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("読み込み")).expect("JSON");
        let object = value.as_object().expect("オブジェクト");
        let mut keys: Vec<_> = object.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["today_chars", "today_date", "total_chars", "total_sessions"]
        );
        assert_eq!(object["today_date"], "2026-10-25");
        assert_eq!(
            object.values().filter(|value| value.is_string()).count(),
            1,
            "日付以外に文字列を含めない"
        );

        fs::remove_dir_all(dir).expect("後片付け");
    }

    #[test]
    fn corrupt_file_starts_from_zero() {
        let dir = temp_dir("corrupt");
        let path = dir.join(STATS_PATH);
        fs::write(&path, b"{ not json").expect("壊れたファイル");
        assert_eq!(Stats::load(&path), Stats::default());
        fs::remove_dir_all(dir).expect("後片付け");
    }
}
