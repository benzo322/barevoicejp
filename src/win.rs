//! Windows ネイティブ連携。
//!
//! - グローバル低レベルキーボードフック (`WH_KEYBOARD_LL`) で右 Alt を監視し、
//!   OS へ伝播させずにメニューバーフォーカスを奪わせない。
//! - クリップボード (`CF_UNICODETEXT`) の読み書き。
//! - `SendInput` による単発の Ctrl+V。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_CONTROL, VK_ESCAPE,
    VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RETURN, VK_RMENU,
    VK_RSHIFT, VK_SHIFT, VK_SPACE, VK_TAB, VK_V,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, MSG, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::config;

const CF_UNICODETEXT: u32 = 13;
const VK_F1: u16 = 0x70;
/// F1–F12 のキー番号範囲。
const FIRST_F_KEY: u16 = 1;
const LAST_F_KEY: u16 = 12;
/// `KBDLLHOOKSTRUCT.flags` の拡張キー bit (右 Alt 判定)。
const LLKHF_EXTENDED: u32 = 0x01;

// windows-sys 0.61 に GlobalFree のバインディングが無いため直接宣言する。
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalFree(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
}

// ---------------------------------------------------------------------------
// ホットキー
// ---------------------------------------------------------------------------
/// 監視対象のキー。
///
/// `right_alt` が真のときだけ OS への伝播を抑制し、メニューバーへの
/// フォーカス移動を防ぐ。
#[derive(Clone, Copy, Debug)]
pub struct Hotkey {
    vk: u16,
    right_alt: bool,
}

/// `alt_r` / `f9` / `ctrl_r` などのキー名を仮想キーコードへ変換する。
///
/// `Key.` 接頭辞、`option`/`command`/`control` の別名も受け付ける。
pub fn parse_hotkey(spec: &str) -> Result<Hotkey, String> {
    let text = spec.trim();
    if text.is_empty() {
        return Err("ホットキーが空です。例: ctrl_r, alt_r, f9".to_string());
    }
    let mut name = text.to_lowercase();
    if let Some(stripped) = name.strip_prefix("key.") {
        name = stripped.to_string();
    }
    let name = match name.as_str() {
        "option" => "alt",
        "option_l" => "alt_l",
        "option_r" => "alt_r",
        "command" => "cmd",
        "command_l" => "cmd_l",
        "command_r" => "cmd_r",
        "control" => "ctrl",
        "control_l" => "ctrl_l",
        "control_r" => "ctrl_r",
        "escape" => "esc",
        "return" => "enter",
        other => other,
    };

    let hotkey = |vk: u16, right_alt: bool| Hotkey { vk, right_alt };
    match name {
        "alt_r" | "alt_gr" => Ok(hotkey(VK_RMENU, true)),
        "alt_l" => Ok(hotkey(VK_LMENU, false)),
        "alt" => Ok(hotkey(VK_MENU, false)),
        "ctrl_l" => Ok(hotkey(VK_LCONTROL, false)),
        "ctrl_r" => Ok(hotkey(VK_RCONTROL, false)),
        "ctrl" => Ok(hotkey(VK_CONTROL, false)),
        "shift_l" => Ok(hotkey(VK_LSHIFT, false)),
        "shift_r" => Ok(hotkey(VK_RSHIFT, false)),
        "shift" => Ok(hotkey(VK_SHIFT, false)),
        "cmd" | "cmd_l" => Ok(hotkey(VK_LWIN, false)),
        "esc" => Ok(hotkey(VK_ESCAPE, false)),
        "enter" => Ok(hotkey(VK_RETURN, false)),
        "space" => Ok(hotkey(VK_SPACE, false)),
        "tab" => Ok(hotkey(VK_TAB, false)),
        _ => {
            if let Some(rest) = name.strip_prefix('f')
                && let Ok(number) = rest.parse::<u16>()
                && (FIRST_F_KEY..=LAST_F_KEY).contains(&number)
            {
                return Ok(hotkey(VK_F1 + number - FIRST_F_KEY, false));
            }
            let mut chars = name.chars();
            if let (Some(c), None) = (chars.next(), chars.next())
                && c.is_ascii_alphanumeric()
            {
                return Ok(hotkey(c.to_ascii_uppercase() as u16, false));
            }
            Err(format!(
                "不明なキー名です: {spec}\n  例: ctrl_r, alt_r, shift_r, f9, space, v"
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// クリップボード
// ---------------------------------------------------------------------------
/// クリップボードのオープン/クローズを RAII で管理する。
struct ClipboardGuard;

impl ClipboardGuard {
    /// 他のプロセスが保持している場合に備えて数回再試行する。
    fn open() -> Result<Self, String> {
        for _ in 0..config::CLIPBOARD_OPEN_ATTEMPTS {
            // SAFETY: NULL オーナーで現在のタスクにクリップボードを開く。
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(Self);
            }
            std::thread::sleep(Duration::from_millis(config::CLIPBOARD_OPEN_RETRY_MS));
        }
        Err(format!(
            "クリップボードを開けません (error={})",
            unsafe { GetLastError() }
        ))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: `open()` が成功したときだけ構築される。
        unsafe {
            CloseClipboard();
        }
    }
}

/// クリップボードのテキストを読み取る。テキストが無ければ空文字列。
pub fn clipboard_get() -> Result<String, String> {
    let _clipboard = ClipboardGuard::open()?;
    // SAFETY: ガードがクリップボードを開いたまま保持している。
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return Ok(String::new());
        }
        let handle = GetClipboardData(CF_UNICODETEXT);
        if handle.is_null() {
            return Ok(String::new());
        }
        let pointer = GlobalLock(handle as _);
        if pointer.is_null() {
            return Err("クリップボードの内容をロックできません".to_string());
        }
        let wide = pointer as *const u16;
        let mut length = 0usize;
        while *wide.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, length));
        GlobalUnlock(handle as _);
        Ok(text)
    }
}

/// クリップボードを UTF-16 テキストで置き換える。
pub fn clipboard_set(text: &str) -> Result<(), String> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();

    let _clipboard = ClipboardGuard::open()?;
    // SAFETY: ガードがクリップボードを保持し、メモリは成功時のみ所有権を移す。
    unsafe {
        if EmptyClipboard() == 0 {
            return Err(format!(
                "クリップボードを空にできません (error={})",
                GetLastError()
            ));
        }
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if handle.is_null() {
            return Err(format!(
                "クリップボード用メモリを確保できません (error={})",
                GetLastError()
            ));
        }
        let pointer = GlobalLock(handle);
        if pointer.is_null() {
            let error = GetLastError();
            GlobalFree(handle);
            return Err(format!(
                "クリップボード用メモリをロックできません (error={error})"
            ));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer as *mut u16, wide.len());
        GlobalUnlock(handle);
        if SetClipboardData(CF_UNICODETEXT, handle as HANDLE).is_null() {
            let error = GetLastError();
            GlobalFree(handle);
            return Err(format!("クリップボードへ書き込めません (error={error})"));
        }
        // 成功時はメモリの所有権がクリップボードへ移るため GlobalFree しない。
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SendInput
// ---------------------------------------------------------------------------
/// `SendInput` 用のキーボードイベントを 1 つ作る。
fn key_event(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Ctrl down → V down → V up → Ctrl up を 1 回の SendInput で送る。
pub fn send_ctrl_v() -> Result<(), String> {
    let inputs = [
        key_event(VK_CONTROL, false),
        key_event(VK_V, false),
        key_event(VK_V, true),
        key_event(VK_CONTROL, true),
    ];
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent != inputs.len() as u32 {
        return Err(format!(
            "SendInput による Ctrl+V 送信が途中で失敗しました (sent={}/{}, error={})",
            sent,
            inputs.len(),
            unsafe { GetLastError() }
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 低レベルキーボードフック
// ---------------------------------------------------------------------------
/// フックが呼ぶコールバック一式。
///
/// `cancel_vk` は常に Esc。ホットキー押下中でも Esc で発話を破棄できる。
pub struct HookHandler {
    hotkey: Hotkey,
    cancel_vk: u16,
    on_press: Box<dyn Fn() + Send + Sync>,
    on_release: Box<dyn Fn() + Send + Sync>,
    on_cancel: Box<dyn Fn() + Send + Sync>,
}

impl HookHandler {
    /// 押下・離下・キャンセルのコールバックを登録する。
    pub fn new(
        hotkey: Hotkey,
        on_press: Box<dyn Fn() + Send + Sync>,
        on_release: Box<dyn Fn() + Send + Sync>,
        on_cancel: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            hotkey,
            cancel_vk: VK_ESCAPE,
            on_press,
            on_release,
            on_cancel,
        }
    }
}

static HANDLER: OnceLock<HookHandler> = OnceLock::new();
static ALT_DOWN: AtomicBool = AtomicBool::new(false);
static KEY_DOWN: AtomicBool = AtomicBool::new(false);
static MAIN_TID: AtomicU32 = AtomicU32::new(0);

/// 設置したキーボードフック。ドロップ時に自動で解除する。
pub struct HookGuard {
    hook: HHOOK,
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        unsafe {
            UnhookWindowsHookEx(self.hook);
        }
    }
}

/// 低レベルキーボードフックを設置する。
///
/// フックコールバックはこの関数を呼んだスレッドで実行されるため、
/// 呼び出し後に [`run_message_loop`] を回す必要がある。
pub fn install(handler: HookHandler) -> Result<HookGuard, String> {
    if HANDLER.set(handler).is_err() {
        return Err("キーボードフックは既に設定されています".to_string());
    }
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), 1);
        let module = GetModuleHandleW(std::ptr::null());
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0);
        if hook.is_null() {
            return Err(format!(
                "キーボード監視を開始できません (error={})",
                GetLastError()
            ));
        }
        Ok(HookGuard { hook })
    }
}

/// Ctrl+C を受けたら WM_QUIT を投げ、メッセージループを終了させる。
pub fn run_message_loop() {
    unsafe {
        MAIN_TID.store(GetCurrentThreadId(), Ordering::SeqCst);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> i32 {
    unsafe {
        let thread_id = MAIN_TID.load(Ordering::SeqCst);
        if thread_id != 0 {
            windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                thread_id, WM_QUIT, 0, 0,
            );
        }
    }
    1
}

fn is_right_alt(vk: u16, flags: u32) -> bool {
    vk == VK_RMENU || (vk == VK_MENU && flags & LLKHF_EXTENDED != 0)
}

fn fire(callback: &(dyn Fn() + Send + Sync)) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
}

unsafe extern "system" fn hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let Some(handler) = HANDLER.get() else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };

    let keyboard = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let message = wparam as u32;
    let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let up = message == WM_KEYUP || message == WM_SYSKEYUP;
    let vk = keyboard.vkCode as u16;

    // 右 Alt: OS へ伝播させず、アプリの押下/離下処理だけを呼ぶ。
    if handler.hotkey.right_alt && is_right_alt(vk, keyboard.flags) {
        if down {
            if !ALT_DOWN.swap(true, Ordering::SeqCst) {
                fire(&handler.on_press);
            }
        } else if up && ALT_DOWN.swap(false, Ordering::SeqCst) {
            fire(&handler.on_release);
        }
        return 1;
    }

    // 右 Alt 以外のホットキーはそのまま OS にも通す。
    if vk == handler.hotkey.vk {
        if down {
            if !KEY_DOWN.swap(true, Ordering::SeqCst) {
                fire(&handler.on_press);
            }
        } else if up && KEY_DOWN.swap(false, Ordering::SeqCst) {
            fire(&handler.on_release);
        }
    }

    if vk == handler.cancel_vk && down {
        fire(&handler.on_cancel);
    }

    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}
