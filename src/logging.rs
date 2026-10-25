//! 最小ロガー。stderr と `logs/app.log` の両方へ出力する。
//!
//! リリースビルドは Windows GUI サブシステムで起動するためコンソールが無い。
//! その場合でもログを失わないよう、exe と同じディレクトリの
//! `logs/app.log` へ常に書き出す。1 ファイル 5 MiB、バックアップ 3 世代で
//! ローテーションする。
//!
//! 書き込みは専用スレッドが行い、[`log`] はキューへ渡すだけなので、
//! 呼び出し側 (特にキーボードフック) をファイル I/O でブロックしない。

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

/// ログファイルを置くディレクトリ (exe と同じ場所)。
const LOG_DIR: &str = "logs";
/// 現在のログファイル名。
const LOG_FILE_NAME: &str = "app.log";
/// 1 ファイルの上限 (5 MiB)。
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// 保持するバックアップ世代数。
const BACKUP_COUNT: u32 = 3;

static DEBUG: AtomicBool = AtomicBool::new(false);
/// ライタースレッドへの待ち行列。キューが満杯なら行を捨てる。
const QUEUE_CAPACITY: usize = 1024;

/// ライタースレッドへ渡すメッセージ。
enum Record {
    Line(String),
    /// 未書き込みの行を処理してから完了を通知する。
    Flush(SyncSender<()>),
}

static SINK: OnceLock<SyncSender<Record>> = OnceLock::new();

/// 追記先のログファイル。書き込みごとにサイズを更新し、上限超過時に
/// `app.log.1` … `app.log.3` へローテーションする。
struct LogFile {
    path: PathBuf,
    file: File,
    size: u64,
    max_bytes: u64,
}

impl LogFile {
    fn open(path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        let mut log = Self {
            path,
            file,
            size,
            max_bytes: MAX_LOG_BYTES,
        };
        // 前回終了時点ですでに上限へ達している場合は起動時に切り替える。
        if size >= log.max_bytes {
            log.rotate();
        }
        Ok(log)
    }

    fn write_line(&mut self, line: &str) {
        let bytes = line.len() as u64;
        if self.size + bytes > self.max_bytes {
            self.rotate();
        }
        if let Err(exc) = self.file.write_all(line.as_bytes()) {
            // ログ書き込みの失敗でアプリを止めない。
            let _ = writeln!(std::io::stderr(), "ログを書き込めません: {exc}");
            return;
        }
        self.size += bytes;
    }

    /// `app.log` → `.1` → `.2` → `.3` とずらし、古い世代を捨てる。
    fn rotate(&mut self) {
        let _ = self.file.flush();
        for index in (1..=BACKUP_COUNT).rev() {
            let target = backup_path(&self.path, index);
            let _ = fs::remove_file(&target);
            let source = if index == 1 {
                self.path.clone()
            } else {
                backup_path(&self.path, index - 1)
            };
            let _ = fs::rename(source, target);
        }
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            self.size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
            self.file = file;
        } else {
            // 再オープンに失敗しても書き込み先 (旧ハンドル) は残っている。
            self.size = 0;
        }
    }
}

/// `app.log.1` のようなバックアップ名を作る。
fn backup_path(path: &Path, index: u32) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

/// ログファイルの絶対パス。exe の隣に置くため、カレントディレクトリに依存しない。
fn log_path() -> PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join(LOG_DIR).join(LOG_FILE_NAME)
}

/// デバッグログの出力可否を設定し、ファイル出力を開く。
pub(crate) fn init(debug: bool) {
    DEBUG.store(debug, Ordering::Relaxed);
    let path = log_path();
    install_panic_hook(path.clone());
    let mut log = match LogFile::open(path.clone()) {
        Ok(log) => log,
        Err(exc) => {
            // ファイルを開けなくてもアプリは起動させる (stderr があればそこへ出す)。
            let _ = writeln!(
                std::io::stderr(),
                "ログファイルを開けません ({}): {exc}",
                path.display()
            );
            return;
        }
    };
    let (tx, rx) = sync_channel(QUEUE_CAPACITY);
    // ライターを起動できなければ `log()` は stderr へ直接書く。
    if std::thread::Builder::new()
        .name("BAREVoiceJP-log".to_string())
        .spawn(move || {
            for record in rx {
                match record {
                    Record::Line(line) => {
                        let _ = std::io::stderr().write_all(line.as_bytes());
                        log.write_line(&line);
                    }
                    Record::Flush(done) => {
                        let _ = log.file.flush();
                        let _ = done.send(());
                    }
                }
            }
        })
        .is_ok()
    {
        let _ = SINK.set(tx);
    }
}

/// 未書き込みの行をすべて処理してから戻る。終了直前に呼ぶ。
///
/// キーボードフックの経路からは呼ばない (ファイル I/O で待つため)。
pub(crate) fn flush() {
    let Some(sink) = SINK.get() else {
        return;
    };
    let (tx, rx) = sync_channel(0);
    if sink.send(Record::Flush(tx)).is_ok() {
        let _ = rx.recv_timeout(Duration::from_millis(500));
    }
}

/// パニック内容もログファイルへ残す。GUI サブシステムでは stderr が見えないため。
fn install_panic_hook(path: PathBuf) {
    std::panic::set_hook(Box::new(move |info| {
        let line = format!("{} [ERROR] パニック: {info}\n", timestamp());
        // フックコールバック内ではファイル I/O をしない。ライターが動いて
        // いればキューへ渡し、それ以外のスレッドでは確実に直接書く。
        if IN_HOOK.with(Cell::get) {
            if let Some(sink) = SINK.get() {
                let _ = sink.try_send(Record::Line(line));
            }
            return;
        }
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(line.as_bytes());
        }
    }));
}

thread_local! {
    /// キーボードフックのコールバック実行中かどうか。
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// フックコールバックの実行範囲をマークする。
///
/// パニック時も確実に解除されるよう `Drop` で戻す。この範囲では
/// ログのファイル I/O をキューへ退避する。
pub(crate) struct HookScope;

impl HookScope {
    pub(crate) fn enter() -> Self {
        IN_HOOK.with(|flag| flag.set(true));
        Self
    }
}

impl Drop for HookScope {
    fn drop(&mut self) {
        IN_HOOK.with(|flag| flag.set(false));
    }
}

fn enabled(level: Level) -> bool {
    level != Level::Debug || DEBUG.load(Ordering::Relaxed)
}

/// 1 行ログを stderr とログファイルへ出力する。マクロ経由で呼ぶ。
pub(crate) fn log(level: Level, args: std::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let label = match level {
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARNING",
        Level::Error => "ERROR",
    };
    let line = format!("{} [{}] {}\n", timestamp(), label, args);
    // ファイル I/O はライタースレッドに任せる。ここはキューへ渡すだけで、
    // フックコールバックでも待たない。初期化前だけ stderr へ直接書く。
    match SINK.get() {
        Some(sink) => {
            // キューが満杯なら行を捨てる。呼び出し側を止めない。
            let _ = sink.try_send(Record::Line(line));
        }
        None => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }
}

#[cfg(windows)]
fn timestamp() -> String {
    let mut time: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut time);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
    )
}

#[cfg(not(windows))]
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

/// ログ種別。`Debug` は `--debug` 指定時のみ出力する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Debug, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Info, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Warn, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Error, format_args!($($arg)*))
    };
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
            "barevoicejp-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("一時ディレクトリ");
        dir
    }

    #[test]
    fn rotates_at_limit_and_keeps_three_backups() {
        let dir = temp_dir("log-rotate");
        let path = dir.join(LOG_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("ログファイル");
        let mut log = LogFile {
            path: path.clone(),
            file,
            size: 0,
            max_bytes: 64,
        };

        // 1 行 18 バイト。18*3 = 54、4 行目で 64 を超えてローテーションする。
        for index in 0..13 {
            log.write_line(&format!("line-{index}-0123456789\n"));
        }

        assert!(path.is_file());
        for index in 1..=BACKUP_COUNT {
            assert!(backup_path(&path, index).is_file(), ".{index} が無い");
        }
        assert!(!backup_path(&path, BACKUP_COUNT + 1).exists());

        // 4 回ローテーション後: .1 = 9..11, .2 = 6..8, .3 = 3..5。
        let current = fs::read_to_string(&path).expect("現行ログ");
        assert!(current.contains("line-12"));
        let newest = fs::read_to_string(backup_path(&path, 1)).expect(".1");
        assert!(newest.contains("line-9"));
        let oldest = fs::read_to_string(backup_path(&path, 3)).expect(".3");
        assert!(oldest.contains("line-3"));
        assert!(!oldest.contains("line-0"), "最古の世代が残っている");

        drop(log);
        fs::remove_dir_all(dir).expect("後片付け");
    }
}
