//! タスクトレイ常駐 (`Shell_NotifyIconW`)。
//!
//! トレイスレッドが隠しウィンドウとアイコンを所有し、メニュー操作を
//! tao のイベントループへ転送する。

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use tao::event_loop::EventLoopProxy;
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, SpreadMode, Stroke,
    Transform,
};
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos, GetMessageW,
    GetWindowLongPtrW, HICON, ICONINFO, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

use crate::ui::UiEvent;
use crate::win::wide;

const CLASS_NAME: &str = "MkVoiceJpTray";
const CALLBACK_MESSAGE: u32 = WM_APP + 7;
const ICON_ID: u32 = 1;
const MENU_SETTINGS: usize = 1001;
const MENU_RELOAD: usize = 1002;
const MENU_EXIT: usize = 1003;

static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

/// トレイアイコンを削除してトレイスレッドを終了させる。
pub fn shutdown() {
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
pub fn spawn(proxy: EventLoopProxy<UiEvent>) -> Result<(), String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("mkvoicejp-tray".to_string())
        .spawn(move || {
            let _ = run(proxy, ready_tx);
        })
        .map_err(|exc| format!("トレイスレッドを開始できません: {exc}"))?;
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(result) => result,
        Err(_) => Err("トレイアイコンの初期化がタイムアウトしました".to_string()),
    }
}

struct TrayCtx {
    proxy: EventLoopProxy<UiEvent>,
    icon: HICON,
}

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
    let title = wide("mkvoicejp");
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
        let tooltip = wide("mkvoicejp — Push-to-Talk 音声入力");
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

/// 32×32 のグラデーション付きマイクアイコンを作る。
fn create_icon() -> HICON {
    const SIZE: u32 = 32;
    let Some(mut pixmap) = Pixmap::new(SIZE, SIZE) else {
        return std::ptr::null_mut();
    };
    draw_icon(&mut pixmap, SIZE as f32);

    // SAFETY: DIB とマスクを作り、アイコンへ変換する。
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE as i32,
                biHeight: -(SIZE as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..BITMAPINFOHEADER::default()
            },
            ..BITMAPINFO::default()
        };
        let screen = windows_sys::Win32::Graphics::Gdi::GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return std::ptr::null_mut();
        }
        let memory = CreateCompatibleDC(screen);
        if memory.is_null() {
            windows_sys::Win32::Graphics::Gdi::ReleaseDC(std::ptr::null_mut(), screen);
            return std::ptr::null_mut();
        }
        let mut bits: *mut c_void = std::ptr::null_mut();
        let color = CreateDIBSection(
            screen,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        windows_sys::Win32::Graphics::Gdi::ReleaseDC(std::ptr::null_mut(), screen);
        if color.is_null() || bits.is_null() {
            if !color.is_null() {
                DeleteObject(color);
            }
            if !memory.is_null() {
                DeleteDC(memory);
            }
            return std::ptr::null_mut();
        }
        let source = pixmap.data();
        let destination =
            std::slice::from_raw_parts_mut(bits.cast::<u8>(), (SIZE * SIZE * 4) as usize);
        for index in 0..(SIZE * SIZE) as usize {
            let offset = index * 4;
            destination[offset] = source[offset + 2];
            destination[offset + 1] = source[offset + 1];
            destination[offset + 2] = source[offset];
            destination[offset + 3] = source[offset + 3];
        }

        // 1bpp マスク: アルファ 0 のピクセルだけ透明にする。
        let stride = SIZE.div_ceil(16) * 2;
        let mut mask = vec![0u8; (stride * SIZE) as usize];
        for y in 0..SIZE {
            for x in 0..SIZE {
                let alpha = source[((y * SIZE + x) * 4 + 3) as usize];
                if alpha < 8 {
                    let bit = 7 - (x % 8);
                    mask[(y * stride + x / 8) as usize] |= 1u8 << bit;
                }
            }
        }
        let mask_bitmap = CreateBitmap(SIZE as i32, SIZE as i32, 1, 1, mask.as_ptr().cast());
        if mask_bitmap.is_null() {
            DeleteObject(color);
            DeleteDC(memory);
            return std::ptr::null_mut();
        }
        let icon_info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bitmap,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&icon_info);
        DeleteObject(mask_bitmap);
        DeleteObject(color);
        DeleteDC(memory);
        icon
    }
}

fn draw_icon(pixmap: &mut Pixmap, size: f32) {
    let mut builder = PathBuilder::new();
    let radius = size * 0.28;
    let k = 0.552_284_8 * radius;
    builder.move_to(radius, 0.0);
    builder.line_to(size - radius, 0.0);
    builder.cubic_to(size - radius + k, 0.0, size, radius - k, size, radius);
    builder.line_to(size, size - radius);
    builder.cubic_to(
        size,
        size - radius + k,
        size - radius + k,
        size,
        size - radius,
        size,
    );
    builder.line_to(radius, size);
    builder.cubic_to(radius - k, size, 0.0, size - radius + k, 0.0, size - radius);
    builder.line_to(0.0, radius);
    builder.cubic_to(0.0, radius - k, radius - k, 0.0, radius, 0.0);
    builder.close();
    let Some(background) = builder.finish() else {
        return;
    };
    let shader = LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, 0.0),
        tiny_skia::Point::from_xy(size, size),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(56, 189, 248, 255)),
            GradientStop::new(1.0, Color::from_rgba8(99, 102, 241, 255)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    );
    let mut paint = Paint::default();
    if let Some(shader) = shader {
        paint.shader = shader;
    } else {
        paint.set_color(Color::from_rgba8(56, 189, 248, 255));
    }
    pixmap.fill_path(
        &background,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let mut white = Paint::default();
    white.set_color(Color::from_rgba8(255, 255, 255, 245));
    let body = tiny_skia::Rect::from_xywh(size * 0.38, size * 0.2, size * 0.24, size * 0.38);
    if let Some(body) = body
        && let Some(path) = rounded_rect(body, size * 0.12)
    {
        pixmap.fill_path(
            &path,
            &white,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    let mut cradle = PathBuilder::new();
    cradle.move_to(size * 0.29, size * 0.5);
    cradle.cubic_to(
        size * 0.29,
        size * 0.7,
        size * 0.4,
        size * 0.78,
        size * 0.5,
        size * 0.78,
    );
    cradle.cubic_to(
        size * 0.6,
        size * 0.78,
        size * 0.71,
        size * 0.7,
        size * 0.71,
        size * 0.5,
    );
    if let Some(cradle) = cradle.finish() {
        pixmap.stroke_path(
            &cradle,
            &white,
            &Stroke {
                width: size * 0.07,
                ..Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }
    let mut stem = PathBuilder::new();
    stem.move_to(size * 0.5, size * 0.76);
    stem.line_to(size * 0.5, size * 0.86);
    stem.move_to(size * 0.38, size * 0.87);
    stem.line_to(size * 0.62, size * 0.87);
    if let Some(stem) = stem.finish() {
        pixmap.stroke_path(
            &stem,
            &white,
            &Stroke {
                width: size * 0.065,
                ..Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }
}

fn rounded_rect(rect: tiny_skia::Rect, radius: f32) -> Option<tiny_skia::Path> {
    let r = radius.min(rect.width() * 0.5).min(rect.height() * 0.5);
    let k = 0.552_284_8 * r;
    let mut builder = PathBuilder::new();
    builder.move_to(rect.left() + r, rect.top());
    builder.line_to(rect.right() - r, rect.top());
    builder.cubic_to(
        rect.right() - r + k,
        rect.top(),
        rect.right(),
        rect.top() + r - k,
        rect.right(),
        rect.top() + r,
    );
    builder.line_to(rect.right(), rect.bottom() - r);
    builder.cubic_to(
        rect.right(),
        rect.bottom() - r + k,
        rect.right() - r + k,
        rect.bottom(),
        rect.right() - r,
        rect.bottom(),
    );
    builder.line_to(rect.left() + r, rect.bottom());
    builder.cubic_to(
        rect.left() + r - k,
        rect.bottom(),
        rect.left(),
        rect.bottom() - r + k,
        rect.left(),
        rect.bottom() - r,
    );
    builder.line_to(rect.left(), rect.top() + r);
    builder.cubic_to(
        rect.left(),
        rect.top() + r - k,
        rect.left() + r - k,
        rect.top(),
        rect.left() + r,
        rect.top(),
    );
    builder.close();
    builder.finish()
}
