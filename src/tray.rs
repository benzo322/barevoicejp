//! タスクトレイ常駐 (`Shell_NotifyIconW`)。
//!
//! トレイスレッドが隠しウィンドウとアイコンを所有し、メニュー操作を
//! tao のイベントループへ転送する。

use std::mem::size_of;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use tao::event_loop::EventLoopProxy;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, HICON, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

use crate::ui::UiEvent;
use crate::win::wide;

const CLASS_NAME: &str = "BAREVoiceJPTray";
const CALLBACK_MESSAGE: u32 = WM_APP + 7;
const ICON_ID: u32 = 1;
const MENU_SETTINGS: usize = 1001;
const MENU_RELOAD: usize = 1002;
const MENU_EXIT: usize = 1003;

static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

/// トレイアイコンを削除してトレイスレッドを終了させる。
pub(crate) fn shutdown() {
    let hwnd = TRAY_HWND.load(Ordering::SeqCst);
    if hwnd != 0 {
        // SAFETY: hwnd はトレイスレッドが作成した隠しウィンドウ。
        unsafe {
            PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// トレイスレッドを起動し、アイコン登録が終わるまで待つ。
pub(crate) fn spawn(proxy: EventLoopProxy<UiEvent>) -> Result<(), String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("BAREVoiceJP-tray".to_string())
        .spawn(move || {
            let _ = run(proxy, ready_tx);
        })
        .map_err(|exc| format!("トレイスレッドを開始できません: {exc}"))?;
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(result) => result,
        Err(_) => Err("トレイアイコンの初期化がタイムアウトしました".to_string()),
    }
}

/// トレイスレッドが所有するアイコンとイベント送信先。
struct TrayCtx {
    proxy: EventLoopProxy<UiEvent>,
    icon: HICON,
}

/// 失敗理由を起動待ちの呼び出し元へ伝えてから終了する。
fn run(
    proxy: EventLoopProxy<UiEvent>,
    ready: mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let result = run_inner(proxy, &ready);
    if let Err(exc) = &result {
        let _ = ready.send(Err(exc.clone()));
    }
    result
}

fn run_inner(
    proxy: EventLoopProxy<UiEvent>,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let class = wide(CLASS_NAME);
    let title = wide("BAREVoiceJP");
    // SAFETY: クラス名・ウィンドウ名は NUL 終端で、コールバックは 'static。
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let window_class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        let atom = RegisterClassW(&window_class);
        if atom == 0 {
            let error = GetLastError();
            if error != 1410 {
                return Err(format!("トレイウィンドウクラスを登録できません: {error}"));
            }
        }
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err(format!(
                "トレイウィンドウを作成できません: {}",
                GetLastError()
            ));
        }
        let icon = create_icon();
        TRAY_HWND.store(hwnd as isize, Ordering::SeqCst);
        let ctx = Box::new(TrayCtx { proxy, icon });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ctx) as isize);

        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = ICON_ID;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = CALLBACK_MESSAGE;
        data.hIcon = icon;
        let tooltip = wide("BAREVoiceJP — Push-to-Talk 音声入力");
        let length = tooltip.len().min(data.szTip.len());
        data.szTip[..length].copy_from_slice(&tooltip[..length]);

        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            let error = GetLastError();
            let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayCtx;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !pointer.is_null() {
                let ctx = Box::from_raw(pointer);
                if !ctx.icon.is_null() {
                    DestroyIcon(ctx.icon);
                }
            }
            DestroyWindow(hwnd);
            return Err(format!("トレイアイコンを登録できません: {error}"));
        }
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &data);
        let _ = ready.send(Ok(()));

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    // SAFETY: GWLP_USERDATA は作成時に格納した TrayCtx を指す。
    let ctx = unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayCtx;
        pointer.as_mut()
    };
    match message {
        CALLBACK_MESSAGE => {
            let event = (lparam as u32) & 0xFFFF;
            if let Some(ctx) = ctx {
                match event {
                    WM_LBUTTONDBLCLK => {
                        let _ = ctx.proxy.send_event(UiEvent::ShowSettings);
                    }
                    WM_RBUTTONUP => unsafe {
                        show_menu(hwnd, ctx);
                    },
                    _ => {}
                }
            }
            return 0;
        }
        WM_COMMAND => {
            if let Some(ctx) = ctx {
                match wparam & 0xFFFF {
                    MENU_SETTINGS => {
                        let _ = ctx.proxy.send_event(UiEvent::ShowSettings);
                    }
                    MENU_RELOAD => {
                        let _ = ctx.proxy.send_event(UiEvent::ReloadDictionaries);
                    }
                    MENU_EXIT => {
                        let _ = ctx.proxy.send_event(UiEvent::Quit);
                    }
                    _ => {}
                }
            }
            return 0;
        }
        WM_DESTROY => {
            // SAFETY: 登録したアイコンと Ctx を解放する。
            unsafe {
                let mut data: NOTIFYICONDATAW = std::mem::zeroed();
                data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
                data.hWnd = hwnd;
                data.uID = ICON_ID;
                Shell_NotifyIconW(NIM_DELETE, &data);
                let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayCtx;
                if !pointer.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let ctx = Box::from_raw(pointer);
                    if !ctx.icon.is_null() {
                        DestroyIcon(ctx.icon);
                    }
                }
                TRAY_HWND.store(0, Ordering::SeqCst);
                PostQuitMessage(0);
            }
            return 0;
        }
        _ => {}
    }
    // SAFETY: 既定処理へ委譲する。
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

unsafe fn show_menu(hwnd: HWND, ctx: &TrayCtx) {
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: point は有効な出力先。
    unsafe {
        GetCursorPos(&mut point);
    }
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    let settings = wide("設定");
    let reload = wide("辞書の再読み込み");
    let exit = wide("終了");
    // SAFETY: メニューと文字列は呼び出し中有効。
    unsafe {
        AppendMenuW(menu, MF_STRING, MENU_SETTINGS, settings.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_RELOAD, reload.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_EXIT, exit.as_ptr());
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            point.x,
            point.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        DestroyMenu(menu);
        PostMessageW(hwnd, WM_NULL, 0, 0);
        match command as usize {
            MENU_SETTINGS => {
                let _ = ctx.proxy.send_event(UiEvent::ShowSettings);
            }
            MENU_RELOAD => {
                let _ = ctx.proxy.send_event(UiEvent::ReloadDictionaries);
            }
            MENU_EXIT => {
                let _ = ctx.proxy.send_event(UiEvent::Quit);
            }
            _ => {}
        }
    }
}

/// 埋め込み `app.ico` から 32×32 のトレイアイコンを作る。
fn create_icon() -> HICON {
    const ICON: &[u8] = include_bytes!("../app.ico");
    const TARGET: u16 = 32;
    let Some(data) = ico_entry(ICON, TARGET) else {
        return std::ptr::null_mut();
    };
    // SAFETY: data は ICO 内の 1 エントリ全体を指し、呼び出し中有効。
    unsafe {
        CreateIconFromResourceEx(
            data.as_ptr(),
            data.len() as u32,
            1,
            0x0003_0000,
            TARGET as i32,
            TARGET as i32,
            LR_DEFAULTCOLOR,
        )
    }
}

/// ICO の中から指定サイズに最も近いエントリの画像データを返す。
fn ico_entry(icon: &[u8], target: u16) -> Option<&[u8]> {
    if icon.len() < 6
        || u16::from_le_bytes([icon[0], icon[1]]) != 0
        || u16::from_le_bytes([icon[2], icon[3]]) != 1
    {
        return None;
    }
    let count = u16::from_le_bytes([icon[4], icon[5]]) as usize;
    if icon.len() < 6 + count * 16 {
        return None;
    }
    let mut best: Option<(u32, &[u8])> = None;
    for index in 0..count {
        let entry = 6 + index * 16;
        let width = match icon[entry] {
            0 => 256u16,
            value => u16::from(value),
        };
        let height = match icon[entry + 1] {
            0 => 256u16,
            value => u16::from(value),
        };
        if width != height {
            continue;
        }
        let size = u32::from_le_bytes([
            icon[entry + 8],
            icon[entry + 9],
            icon[entry + 10],
            icon[entry + 11],
        ]) as usize;
        let offset = u32::from_le_bytes([
            icon[entry + 12],
            icon[entry + 13],
            icon[entry + 14],
            icon[entry + 15],
        ]) as usize;
        let Some(end) = offset.checked_add(size) else {
            continue;
        };
        if size == 0 || end > icon.len() {
            continue;
        }
        let distance = u32::from(width.abs_diff(target));
        let replace = match best {
            Some((best_distance, _)) => distance < best_distance,
            None => true,
        };
        if replace {
            best = Some((distance, &icon[offset..end]));
        }
    }
    best.map(|(_, data)| data)
}
