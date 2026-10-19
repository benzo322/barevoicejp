//! Python `logging` と同じ形式の最小ロガー。
//!
//! 出力先は stderr、形式は `HH:MM:SS [LEVEL] message`。

use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

/// ログ種別。`Debug` は `--debug` 指定時のみ出力する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

/// デバッグログの出力可否を設定する。
pub fn init(debug: bool) {
    DEBUG.store(debug, Ordering::Relaxed);
}

fn enabled(level: Level) -> bool {
    level != Level::Debug || DEBUG.load(Ordering::Relaxed)
}

/// 1 行ログを stderr へ出力する。マクロ経由で呼ぶ。
pub fn log(level: Level, args: std::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let label = match level {
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARNING",
        Level::Error => "ERROR",
    };
    eprintln!("{} [{}] {}", now_hms(), label, args);
}

#[cfg(windows)]
fn now_hms() -> String {
    let mut time: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut time);
    }
    format!("{:02}:{:02}:{:02}", time.wHour, time.wMinute, time.wSecond)
}

#[cfg(not(windows))]
fn now_hms() -> String {
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
