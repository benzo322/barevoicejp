//! Windows ネイティブ連携。
//!
//! - グローバル低レベルキーボードフック (`WH_KEYBOARD_LL`) で右 Alt を監視し、
//!   OS へ伝播させずにメニューバーフォーカスを奪わせない。
//! - クリップボード (`CF_UNICODETEXT`) の読み書き。
//! - `SendInput` による単発の Ctrl+V。

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::Console::GetConsoleWindow;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, ReleaseCapture, SendInput,
    VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL,
    VK_RETURN, VK_RMENU, VK_RSHIFT, VK_SHIFT, VK_SPACE, VK_TAB, VK_V,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow,
    GetForegroundWindow, HHOOK, HTCAPTION, HWND_MESSAGE, HWND_NOTOPMOST, HWND_TOPMOST,
    KBDLLHOOKSTRUCT, RegisterClassW, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SendMessageW, SetForegroundWindow, SetWindowPos, SetWindowsHookExW, ShowWindow,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_NCLBUTTONDOWN, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WNDCLASSW,
};

use crate::config;

const CF_UNICODETEXT: u32 = 13;
const VK_F1: u16 = 0x70;
/// F1–F12 のキー番号範囲。
const FIRST_F_KEY: u16 = 1;
const LAST_F_KEY: u16 = 12;
/// `KBDLLHOOKSTRUCT.flags` の拡張キー bit (右 Alt 判定)。
const LLKHF_EXTENDED: u32 = 0x01;
/// `KBDLLHOOKSTRUCT.flags` の注入 bit。`SendInput` などの合成入力で立つ。
const LLKHF_INJECTED: u32 = 0x10;
/// 低い整合性レベルのプロセスから注入された入力で立つ。
const LLKHF_LOWER_IL_INJECTED: u32 = 0x02;

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
pub(crate) struct Hotkey {
    vk: u16,
    right_alt: bool,
}

/// NUL 終端の UTF-16 へ変換する。
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 設定ウィンドウを最前面へ出し、フォーカスを移す。
pub(crate) fn force_foreground(hwnd: *mut core::ffi::c_void) {
    // SAFETY: hwnd は呼び出し元の有効なトップレベルウィンドウ。
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 枠なし設定ウィンドウをドラッグ移動する。
pub(crate) fn begin_drag(hwnd: *mut core::ffi::c_void) {
    // SAFETY: hwnd は呼び出し元の有効なトップレベルウィンドウ。
    unsafe {
        ReleaseCapture();
        SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
    }
}

/// コンソールウィンドウを隠す (`--console` 指定時は呼ばない)。
pub(crate) fn hide_console() {
    // SAFETY: コンソールが無ければ NULL が返るだけ。
    unsafe {
        let console = GetConsoleWindow();
        if !console.is_null() {
            ShowWindow(console, SW_HIDE);
        }
    }
}

/// 現在のフォアグラウンドウィンドウ。取得できなければ 0。
///
/// 推論中に前面のウィンドウが変わっていないか確認するために使う。
pub(crate) fn foreground_window() -> usize {
    // SAFETY: 引数が無く、失敗時は NULL を返すだけ。
    unsafe { GetForegroundWindow() as usize }
}

const AUTOSTART_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const AUTOSTART_VALUE: &str = "BAREVoiceJP";

/// 自動起動がレジストリに登録されているか。
pub(crate) fn autostart_enabled() -> bool {
    let key = wide(AUTOSTART_KEY);
    let name = wide(AUTOSTART_VALUE);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: 有効な NUL 終端文字列と出力先を渡す。
    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        ) != 0
        {
            return false;
        }
        let mut data = [0u8; 1024];
        let mut size = data.len() as u32;
        let mut kind = 0u32;
        let status = RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            data.as_mut_ptr(),
            &mut size,
        );
        RegCloseKey(hkey);
        status == 0 && kind == REG_SZ && size > 1
    }
}

/// 自動起動を登録・解除する。
pub(crate) fn set_autostart(enabled: bool) -> Result<(), String> {
    let key = wide(AUTOSTART_KEY);
    let name = wide(AUTOSTART_VALUE);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: 文字列は NUL 終端で、hkey は成功時のみ閉じる。
    unsafe {
        if RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) != 0 {
            return Err("自動起動のレジストリキーを開けません".to_string());
        }
        let status = if enabled {
            let exe = std::env::current_exe()
                .map_err(|exc| format!("実行ファイルのパスを取得できません: {exc}"))?;
            let command = format!("\"{}\"", exe.display());
            let data = wide(&command);
            RegSetValueExW(
                hkey,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * std::mem::size_of::<u16>()) as u32,
            )
        } else {
            RegDeleteValueW(hkey, name.as_ptr())
        };
        RegCloseKey(hkey);
        if status == 0 {
            Ok(())
        } else {
            Err(format!("自動起動の設定に失敗しました (code={status})"))
        }
    }
}

/// 二重起動を防ぐミューテックス名。
///
/// `Local\` はログオンセッション単位。同一セッション内の同一ユーザー
/// (と、そのセッションへ入り込んだ管理者) だけが衝突する。
const SINGLE_INSTANCE_NAME: &str = "Local\\BAREVoiceJP.SingleInstance";

/// 単一インスタンスを保証する名前付きミューテックス。
///
/// ハンドルはプロセス終了まで保持する。既に同名ミューテックスが
/// 存在した場合 (別プロセスが起動中) は `Err` を返す。
pub(crate) struct SingleInstance {
    handle: HANDLE,
}

impl SingleInstance {
    /// 単一インスタンスを取得する。既に起動中なら `Err`。
    pub(crate) fn acquire() -> Result<Self, String> {
        let name = wide(SINGLE_INSTANCE_NAME);
        // SAFETY: 名前は NUL 終端。成功時はクローズが必要なハンドルを返す。
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
            if handle.is_null() {
                return Err(format!(
                    "単一インスタンス用ミューテックスを作成できません (error={})",
                    GetLastError()
                ));
            }
            // 作成直後の GetLastError で既存ミューテックスを判定する。
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return Err("BAREVoiceJP は既に起動しています".to_string());
            }
            Ok(Self { handle })
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: `acquire` が成功したハンドルを一度だけ閉じる。
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// `alt_r` / `f9` / `ctrl_r` などのキー名を仮想キーコードへ変換する。
///
/// `Key.` 接頭辞、`option`/`command`/`control` の別名も受け付ける。
pub(crate) fn parse_hotkey(spec: &str) -> Result<Hotkey, String> {
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
/// クリップボードの所有者として使うメッセージ専用ウィンドウのクラス名。
///
/// `OpenClipboard(NULL)` で開くと `EmptyClipboard` が所有者を NULL にし、
/// 続く `SetClipboardData` が必ず失敗する (Win32 の仕様)。書き込み時は
/// このウィンドウを所有者として渡す。
const CLIPBOARD_WINDOW_CLASS: &str = "BAREVoiceJP.ClipboardOwner";

unsafe extern "system" fn clipboard_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: 既定処理へ委譲するだけ。引数は OS から渡された値。
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// ウィンドウクラスをプロセスで一度だけ登録する。
fn register_clipboard_class() -> bool {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    *REGISTERED.get_or_init(|| {
        let class = wide(CLIPBOARD_WINDOW_CLASS);
        let window_class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(clipboard_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            // SAFETY: 引数なし。失敗時は NULL が返るだけ。
            hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        // SAFETY: クラス名は NUL 終端で、この呼び出しの間だけ生きている。
        unsafe { RegisterClassW(&window_class) != 0 }
    })
}

/// スレッドごとのメッセージ専用ウィンドウ。
///
/// `OpenClipboard` を呼ぶスレッドで作ることで、ウィンドウのスレッド親和性を
/// 保ったままクリップボードの所有者として使える。
struct ClipboardOwner {
    hwnd: HWND,
}

impl ClipboardOwner {
    fn create() -> Option<Self> {
        if !register_clipboard_class() {
            return None;
        }
        let class = wide(CLIPBOARD_WINDOW_CLASS);
        let title = wide("BAREVoiceJP clipboard owner");
        // SAFETY: 登録済みクラス名と NUL 終端のタイトルを渡す。
        // メッセージ専用ウィンドウは親に HWND_MESSAGE を指定して作る。
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        (!hwnd.is_null()).then_some(Self { hwnd })
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        // SAFETY: このスレッドで作ったウィンドウを同じスレッドで破棄する。
        unsafe {
            DestroyWindow(self.hwnd);
        }
    }
}

thread_local! {
    static CLIPBOARD_OWNER: RefCell<Option<ClipboardOwner>> = const { RefCell::new(None) };
}

/// このスレッドの所有者ウィンドウ。作成できなければ `None`。
fn clipboard_owner() -> Option<HWND> {
    CLIPBOARD_OWNER.with(|owner| {
        let mut owner = owner.borrow_mut();
        if owner.is_none() {
            *owner = ClipboardOwner::create();
        }
        owner.as_ref().map(|owner| owner.hwnd)
    })
}

/// クリップボードのオープン/クローズを RAII で管理する。
struct ClipboardGuard;

impl ClipboardGuard {
    /// 他のプロセスが保持している場合に備えて数回再試行する。
    ///
    /// `owner` にはこのスレッドの所有者ウィンドウを渡す。読み取りだけで
    /// あれば NULL でもよいが、書き込みには必須。
    fn open(owner: HWND) -> Result<Self, String> {
        for _ in 0..config::CLIPBOARD_OPEN_ATTEMPTS {
            // SAFETY: owner はこのスレッドが作ったウィンドウ、または NULL。
            if unsafe { OpenClipboard(owner) } != 0 {
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
pub(crate) fn clipboard_get() -> Result<String, String> {
    // 読み取りだけなら所有者は不要。ウィンドウがあれば流用する。
    let owner = clipboard_owner().unwrap_or(std::ptr::null_mut());
    let _clipboard = ClipboardGuard::open(owner)?;
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
        // 他プロセスが NUL 終端していない CF_UNICODETEXT を置く場合に備え、
        // 割り当てサイズを超えて読み進めない。
        let capacity = (GlobalSize(handle as _) as usize) / std::mem::size_of::<u16>();
        let wide = pointer as *const u16;
        let mut length = 0usize;
        while length < capacity && *wide.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, length));
        GlobalUnlock(handle as _);
        Ok(text)
    }
}

/// クリップボードを UTF-16 テキストで置き換える。
pub(crate) fn clipboard_set(text: &str) -> Result<(), String> {
    let owner = clipboard_owner()
        .ok_or_else(|| "クリップボードの所有者ウィンドウを作成できません".to_string())?;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();

    // メモリを先に確保してから空にする。確保に失敗しても既存の内容を失わない。
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if handle.is_null() {
        return Err(format!(
            "クリップボード用メモリを確保できません (error={})",
            unsafe { GetLastError() }
        ));
    }
    // SAFETY: handle はこの関数が所有する GMEM_MOVEABLE の領域。
    let pointer = unsafe { GlobalLock(handle) };
    if pointer.is_null() {
        let error = unsafe { GetLastError() };
        // SAFETY: まだクリップボードへ渡していないので所有権はこちらにある。
        unsafe { GlobalFree(handle) };
        return Err(format!(
            "クリップボード用メモリをロックできません (error={error})"
        ));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer as *mut u16, wide.len());
        GlobalUnlock(handle);
    }

    let _clipboard = match ClipboardGuard::open(owner) {
        Ok(guard) => guard,
        Err(exc) => {
            // SAFETY: まだクリップボードへ渡していないので所有権はこちらにある。
            unsafe { GlobalFree(handle) };
            return Err(exc);
        }
    };
    // SAFETY: ガードがクリップボードを保持し、成功時のみ所有権を移す。
    // 所有者が有効なウィンドウなので EmptyClipboard 後も SetClipboardData は成功する。
    unsafe {
        if EmptyClipboard() == 0 {
            let error = GetLastError();
            GlobalFree(handle);
            return Err(format!("クリップボードを空にできません (error={error})"));
        }
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
pub(crate) fn send_ctrl_v() -> Result<(), String> {
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
pub(crate) struct HookHandler {
    hotkey: Hotkey,
    cancel_vk: u16,
    on_press: Box<dyn Fn() + Send + Sync>,
    on_release: Box<dyn Fn() + Send + Sync>,
    on_cancel: Box<dyn Fn() + Send + Sync>,
}

impl HookHandler {
    /// 押下・離下・キャンセルのコールバックを登録する。
    pub(crate) fn new(
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
static HOTKEY_VK: AtomicU32 = AtomicU32::new(0);
static HOTKEY_RIGHT_ALT: AtomicBool = AtomicBool::new(false);

/// 設定画面からホットキーを差し替える。フックの再設置は不要。
pub(crate) fn set_hotkey(hotkey: Hotkey) {
    HOTKEY_VK.store(u32::from(hotkey.vk), Ordering::SeqCst);
    HOTKEY_RIGHT_ALT.store(hotkey.right_alt, Ordering::SeqCst);
}

/// 設置したキーボードフック。ドロップ時に自動で解除する。
pub(crate) struct HookGuard {
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
pub(crate) fn install(handler: HookHandler) -> Result<HookGuard, String> {
    set_hotkey(handler.hotkey);
    if HANDLER.set(handler).is_err() {
        return Err("キーボードフックは既に設定されています".to_string());
    }
    unsafe {
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

/// 右 Alt かどうか。拡張キービットも見て左右を区別する。
fn is_right_alt(vk: u16, flags: u32) -> bool {
    vk == VK_RMENU || (vk == VK_MENU && flags & LLKHF_EXTENDED != 0)
}

/// 合成入力かどうか。合成入力はホットキーとして扱わない。
///
/// 自前の Ctrl+V (`SendInput`) による自己トリガーと、他プロセスが
/// 注入したキーで録音が始まるのを防ぐ。
fn is_injected(flags: u32) -> bool {
    flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0
}

/// フック内のパニックで OS の入力処理を止めないよう包んで実行する。
fn fire(callback: &(dyn Fn() + Send + Sync)) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
}

unsafe extern "system" fn hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    // この範囲ではログのファイル I/O をキューへ退避する (パニック時も含む)。
    let _hook_scope = crate::logging::HookScope::enter();
    if code < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let Some(handler) = HANDLER.get() else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };

    let keyboard = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    // 合成入力 (SendInput / keybd_event / 低い整合性レベルからの注入) は
    // アプリのホットキーとして扱わない。OS へはそのまま通す。
    if is_injected(keyboard.flags) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let message = wparam as u32;
    let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let up = message == WM_KEYUP || message == WM_SYSKEYUP;
    let vk = keyboard.vkCode as u16;
    let hotkey_vk = HOTKEY_VK.load(Ordering::SeqCst) as u16;
    let hotkey_right_alt = HOTKEY_RIGHT_ALT.load(Ordering::SeqCst);

    // 右 Alt: OS へ伝播させず、アプリの押下/離下処理だけを呼ぶ。
    if hotkey_right_alt && is_right_alt(vk, keyboard.flags) {
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
    if vk == hotkey_vk {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use windows_sys::Win32::UI::WindowsAndMessaging::HC_ACTION;

    #[test]
    fn treats_injected_keyboard_flags_as_injected() {
        assert!(is_injected(LLKHF_INJECTED));
        assert!(is_injected(LLKHF_LOWER_IL_INJECTED));
        assert!(is_injected(LLKHF_INJECTED | LLKHF_EXTENDED));
        assert!(!is_injected(0));
        assert!(!is_injected(LLKHF_EXTENDED));
    }

    /// フックコールバック本体に合成入力を渡し、コールバックが呼ばれない
    /// ことを確認する。通常入力では呼ばれることも同じテストで確かめる。
    #[test]
    fn hook_ignores_injected_events() {
        static PRESSES: AtomicUsize = AtomicUsize::new(0);
        static RELEASES: AtomicUsize = AtomicUsize::new(0);

        fn dispatch(keyboard: &KBDLLHOOKSTRUCT, message: u32) {
            // SAFETY: keyboard は呼び出しの間だけ生きている実体を指す。
            unsafe {
                hook_proc(
                    HC_ACTION as i32,
                    message as usize,
                    keyboard as *const KBDLLHOOKSTRUCT as isize,
                );
            }
        }

        // `HANDLER` はプロセスで一度だけ設定できる。
        let handler = HookHandler::new(
            Hotkey {
                vk: VK_RMENU,
                right_alt: true,
            },
            Box::new(|| {
                PRESSES.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(|| {
                RELEASES.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(|| {}),
        );
        let _ = HANDLER.set(handler);
        set_hotkey(Hotkey {
            vk: VK_RMENU,
            right_alt: true,
        });

        let mut keyboard: KBDLLHOOKSTRUCT = unsafe { std::mem::zeroed() };
        keyboard.vkCode = u32::from(VK_RMENU);

        // 合成入力は無視する。
        keyboard.flags = LLKHF_INJECTED;
        dispatch(&keyboard, WM_KEYDOWN);
        dispatch(&keyboard, WM_KEYUP);
        assert_eq!(PRESSES.load(Ordering::SeqCst), 0);
        assert_eq!(RELEASES.load(Ordering::SeqCst), 0);

        // 物理入力と同じフラグ構成なら従来どおり反応する (対照実験)。
        keyboard.flags = 0;
        dispatch(&keyboard, WM_KEYDOWN);
        assert_eq!(PRESSES.load(Ordering::SeqCst), 1);
        dispatch(&keyboard, WM_KEYUP);
        assert_eq!(RELEASES.load(Ordering::SeqCst), 1);
    }

    /// 所有者ウィンドウ経由でクリップボードへ書き込み、読み戻せることを
    /// 確認する。`OpenClipboard(NULL)` のままでは `SetClipboardData` が
    /// 失敗するため、このテストがその退行を検出する。
    ///
    /// 実行するとクリップボードを一時的に書き換える。終了時に元のテキスト
    /// (テキストが無ければ空文字列) へ戻す。
    #[test]
    fn writes_clipboard_with_owner_window() {
        let original = clipboard_get();
        let marker = format!("BAREVoiceJP clipboard test {}", std::process::id());
        clipboard_set(&marker).expect("クリップボードへ書き込めること");
        assert_eq!(
            clipboard_get().expect("クリップボードを読み取れること"),
            marker
        );
        let original = original.unwrap_or_default();
        if let Err(exc) = clipboard_set(&original) {
            eprintln!("クリップボードを復元できません: {exc}");
        }
    }
}
