//! 文字なしフローティングピルバー。
//!
//! Win32 のレイヤードウィンドウに tiny-skia で描画する。WebView2 を
//! 使わないため、フォーカスを奪わず 180×40 の小さなバーを 60fps で
//! 更新できる。波形はマイクの RMS に追従し、推論中は中央へ収束する。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::Instant;

use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pixmap, Rect, Shader,
    SpreadMode, Stroke, Transform,
};
use windows_sys::Win32::Foundation::{GetLastError, HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP,
    HDC, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW,
    HTTRANSPARENT, HWND_TOPMOST, KillTimer, MA_NOACTIVATE, RegisterClassW, SPI_GETWORKAREA,
    SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetTimer, SetWindowLongPtrW, SetWindowPos,
    SystemParametersInfoW, ULW_ALPHA, UpdateLayeredWindow, WM_DESTROY, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_TIMER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::state::VisualState;
use crate::win::wide;

const CLASS_NAME: &str = "MkVoiceJpOverlay";
const TIMER_ID: usize = 1;
/// 影を含めたウィンドウの論理サイズ。
const WINDOW_WIDTH: f32 = 220.0;
const WINDOW_HEIGHT: f32 = 80.0;
/// ピルの論理サイズ (仕様: 約 180×40)。
const PILL_WIDTH: f32 = 180.0;
const PILL_HEIGHT: f32 = 40.0;
const BAR_COUNT: usize = 5;

/// マイクレベル・表示状態・有効フラグを共有してピルバーを作る。
pub struct Overlay {
    hwnd: HWND,
}

impl Overlay {
    pub fn new(
        level: Arc<AtomicU32>,
        visual: Arc<AtomicU8>,
        enabled: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let class = wide(CLASS_NAME);
        let title = wide("mkvoicejp");
        // SAFETY: クラス名は NUL 終端で、コールバックは 'static 関数。
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
                    return Err(format!(
                        "オーバーレイのウィンドウクラスを登録できません: {error}"
                    ));
                }
            }

            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return Err(format!(
                    "オーバーレイウィンドウを作成できません (error={})",
                    GetLastError()
                ));
            }

            let ctx = Box::new(Ctx::new(hwnd, level, visual, enabled));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ctx) as isize);
            SetTimer(hwnd, TIMER_ID, 16, None);
            Ok(Self { hwnd })
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: この HWND は new() が作成したもの。
        unsafe {
            DestroyWindow(self.hwnd);
        }
    }
}

struct Ctx {
    hwnd: HWND,
    level: Arc<AtomicU32>,
    visual: Arc<AtomicU8>,
    enabled: Arc<AtomicBool>,
    scale: f32,
    surface: Option<Surface>,
    pixmap: Option<Pixmap>,
    pill: Option<Path>,
    shadows: Vec<Path>,
    unit_circle: Option<Path>,
    bar_shader: Shader<'static>,
    glow_shader: Shader<'static>,
    background_shader: Shader<'static>,
    heights: [f32; BAR_COUNT],
    visible: bool,
    alpha: f32,
    last_state: VisualState,
    state_since: Instant,
}

impl Ctx {
    fn new(
        hwnd: HWND,
        level: Arc<AtomicU32>,
        visual: Arc<AtomicU8>,
        enabled: Arc<AtomicBool>,
    ) -> Self {
        let scale = 1.0;
        let pill_rect = pill_rect(scale);
        let unit_circle = PathBuilder::from_circle(0.0, 0.0, 1.0);
        let mut ctx = Self {
            hwnd,
            level,
            visual,
            enabled,
            scale,
            surface: None,
            pixmap: None,
            pill: rounded_rect(pill_rect, PILL_HEIGHT * scale * 0.5),
            shadows: shadow_paths(scale),
            unit_circle,
            bar_shader: bar_gradient(),
            glow_shader: glow_gradient(),
            background_shader: background_gradient(),
            heights: [7.0; BAR_COUNT],
            visible: false,
            alpha: 0.0,
            last_state: VisualState::Idle,
            state_since: Instant::now(),
        };
        ctx.reposition();
        ctx
    }

    fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() < 0.01 {
            return;
        }
        self.scale = scale;
        self.pill = rounded_rect(pill_rect(scale), PILL_HEIGHT * scale * 0.5);
        self.shadows = shadow_paths(scale);
        self.surface = None;
        self.pixmap = None;
    }

    fn reposition(&mut self) {
        // SAFETY: SCALE は初期化済み。DPI が取得できなければ 96 とみなす。
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        self.set_scale(scale);

        let mut work: RECT = unsafe { std::mem::zeroed() };
        // SAFETY: 出力先は有効な RECT。
        let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0) };
        let (width, height) = pixel_size(scale);
        let (x, y) = if ok != 0 {
            (
                work.left + ((work.right - work.left) - width) / 2,
                work.bottom - height - (10.0 * scale) as i32,
            )
        } else {
            (0, 0)
        };
        // SAFETY: 作成済みウィンドウの位置とサイズを更新する。
        unsafe {
            SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE);
        }
    }

    fn tick(&mut self) {
        let state = VisualState::from_u8(self.visual.load(Ordering::Relaxed));
        if state != self.last_state {
            self.last_state = state;
            self.state_since = Instant::now();
        }
        let enabled = self.enabled.load(Ordering::Relaxed);

        if !enabled || state == VisualState::Idle {
            if self.visible {
                self.alpha -= 0.075;
                if self.alpha <= 0.0 {
                    self.alpha = 0.0;
                    self.visible = false;
                    crate::debug!("オーバーレイを非表示にしました");
                    // SAFETY: 作成済みウィンドウを隠す。
                    unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(self.hwnd, SW_HIDE);
                    }
                } else {
                    self.render(state);
                }
            }
            return;
        }

        if !self.visible {
            self.reposition();
            self.visible = true;
            self.alpha = 0.0;
            crate::debug!("オーバーレイを表示します (state={state:?})");
            // SAFETY: フォーカスを奪わず最前面に表示する。
            unsafe {
                let (width, height) = pixel_size(self.scale);
                SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    self.window_x(width),
                    self.window_y(height),
                    width,
                    height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
        self.alpha = (self.alpha + 0.16).min(1.0);
        self.render(state);
    }

    fn window_x(&self, width: i32) -> i32 {
        let mut work: RECT = unsafe { std::mem::zeroed() };
        // SAFETY: 出力先は有効な RECT。
        let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0) };
        if ok == 0 {
            0
        } else {
            work.left + ((work.right - work.left) - width) / 2
        }
    }

    fn window_y(&self, height: i32) -> i32 {
        let mut work: RECT = unsafe { std::mem::zeroed() };
        // SAFETY: 出力先は有効な RECT。
        let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0) };
        if ok == 0 {
            0
        } else {
            work.bottom - height - (10.0 * self.scale) as i32
        }
    }

    fn render(&mut self, state: VisualState) {
        let (width, height) = pixel_size(self.scale);
        let (width_u32, height_u32) = (width as u32, height as u32);
        if self
            .surface
            .as_ref()
            .is_none_or(|surface| surface.width != width_u32 || surface.height != height_u32)
        {
            self.surface = Surface::new(width_u32, height_u32);
            self.pixmap = Pixmap::new(width_u32, height_u32);
        }
        let (Some(surface), Some(pixmap)) = (self.surface.as_ref(), self.pixmap.as_mut()) else {
            crate::warn!("オーバーレイの描画面を確保できません");
            return;
        };
        pixmap.fill(Color::TRANSPARENT);

        let scale = self.scale;
        let rect = pill_rect(scale);

        // 柔らかいアンビエントシャドウ (多層の丸角でブラーを近似)。
        for (index, path) in self.shadows.iter().enumerate() {
            let alpha = 6 + index as u8 * 2;
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(30, 41, 59, alpha));
            pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
        }

        // すりガラス調の半透明ホワイト。
        if let Some(pill) = &self.pill {
            let fill = Paint {
                shader: self.background_shader.clone(),
                anti_alias: true,
                ..Paint::default()
            };
            pixmap.fill_path(pill, &fill, FillRule::Winding, Transform::identity(), None);

            // ガラスの縁。外側は淡いブルー、内側は白いハイライト。
            let mut edge = Paint::default();
            edge.set_color(Color::from_rgba8(186, 210, 236, 96));
            pixmap.stroke_path(
                pill,
                &edge,
                &Stroke {
                    width: 1.2 * scale,
                    ..Stroke::default()
                },
                Transform::identity(),
                None,
            );
            let mut highlight = Paint::default();
            highlight.set_color(Color::from_rgba8(255, 255, 255, 210));
            pixmap.stroke_path(
                pill,
                &highlight,
                &Stroke {
                    width: 0.8 * scale,
                    ..Stroke::default()
                },
                Transform::from_translate(0.0, 0.65 * scale),
                None,
            );
        }

        // 上辺のきらめき。
        let mut glint = Paint::default();
        glint.set_color(Color::from_rgba8(255, 255, 255, 135));
        let glint_rect = Rect::from_xywh(
            rect.left() + 26.0 * scale,
            rect.top() + 1.6 * scale,
            rect.width() - 52.0 * scale,
            0.9 * scale,
        );
        if let Some(glint_rect) = glint_rect {
            pixmap.fill_rect(glint_rect, &glint, Transform::identity(), None);
        }

        Self::render_bars(
            pixmap,
            state,
            rect,
            scale,
            &mut self.heights,
            &self.unit_circle,
            &self.bar_shader,
            &self.glow_shader,
            self.state_since,
            &self.level,
        );

        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (self.alpha.clamp(0.0, 1.0) * 255.0) as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        surface.update(self.hwnd, pixmap, &blend);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_bars(
        pixmap: &mut Pixmap,
        state: VisualState,
        rect: Rect,
        scale: f32,
        heights: &mut [f32; BAR_COUNT],
        unit_circle: &Option<Path>,
        bar_shader: &Shader<'static>,
        glow_shader: &Shader<'static>,
        state_since: Instant,
        level: &AtomicU32,
    ) {
        let center_y = rect.top() + rect.height() * 0.5;
        let bar_width = 4.0 * scale;
        let gap = 5.2 * scale;
        let group_width = BAR_COUNT as f32 * bar_width + (BAR_COUNT as f32 - 1.0) * gap;
        let start_x = rect.left() + (rect.width() - group_width) * 0.5;
        let weights = [0.58, 0.84, 1.0, 0.78, 0.62];

        if state == VisualState::Recording {
            let rms = f32::from_bits(level.load(Ordering::Relaxed));
            let normalized = (rms.max(0.0) * 7.0).sqrt().clamp(0.0, 1.0);
            let elapsed = state_since.elapsed().as_secs_f32();
            for (index, weight) in weights.iter().enumerate() {
                let wobble = ((elapsed * 5.2 + index as f32 * 0.9).sin() * 0.5 + 0.5) * 2.2;
                let target = (5.0 + normalized * 17.5 + wobble) * scale * weight;
                let current = &mut heights[index];
                let coefficient = if target > *current { 0.42 } else { 0.12 };
                *current += (target - *current) * coefficient;
            }
        } else {
            let elapsed = state_since.elapsed().as_secs_f32();
            let converge = (elapsed * 5.5).clamp(0.0, 1.0);
            let pulse = (elapsed * 9.0).sin() * 0.5 + 0.5;
            for height in heights.iter_mut() {
                let target = (4.0 + pulse * 4.0) * scale;
                *height += (target - *height) * 0.24;
            }
            let center_x = rect.left() + rect.width() * 0.5;
            if converge > 0.0
                && let Some(circle) = unit_circle
            {
                let mut glow = Paint::default();
                glow.set_color(Color::from_rgba8(99, 102, 241, 22));
                let radius = (9.0 + pulse * 4.0) * scale;
                pixmap.fill_path(
                    circle,
                    &glow,
                    FillRule::Winding,
                    Transform::from_scale(radius, radius).post_translate(center_x, center_y),
                    None,
                );
                glow.set_color(Color::from_rgba8(129, 140, 248, 42));
                let radius = (5.0 + pulse * 2.0) * scale;
                pixmap.fill_path(
                    circle,
                    &glow,
                    FillRule::Winding,
                    Transform::from_scale(radius, radius).post_translate(center_x, center_y),
                    None,
                );
            }
        }

        let paint = Paint {
            shader: bar_shader.clone(),
            anti_alias: true,
            ..Paint::default()
        };

        let converge = if state == VisualState::Recording {
            0.0
        } else {
            (state_since.elapsed().as_secs_f32() * 5.5).clamp(0.0, 1.0)
        };
        let center_x = rect.left() + rect.width() * 0.5;

        for (index, height) in heights.iter().enumerate().take(BAR_COUNT) {
            let original_x = start_x + index as f32 * (bar_width + gap);
            let x = original_x + (center_x - original_x) * converge;
            let height = height.max(3.0 * scale).min(30.0 * scale);
            let top = center_y - height * 0.5;
            if let Some(path) = rounded_rect(
                Rect::from_xywh(x, top, bar_width, height).unwrap_or(rect),
                bar_width * 0.5,
            ) {
                pixmap.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            // ごく淡い発光を重ね、透明感を出す。
            let glow = Paint {
                shader: glow_shader.clone(),
                ..Paint::default()
            };
            let glow_rect = Rect::from_xywh(
                x - 1.4 * scale,
                top - 1.1 * scale,
                bar_width + 2.8 * scale,
                height + 2.2 * scale,
            );
            if converge < 0.9
                && let Some(glow_rect) = glow_rect
                && let Some(path) = rounded_rect(glow_rect, (bar_width + 2.8 * scale) * 0.5)
            {
                pixmap.fill_path(&path, &glow, FillRule::Winding, Transform::identity(), None);
            }
        }
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        if let Some(surface) = self.surface.take() {
            drop(surface);
        }
    }
}

/// 32bit BGRA の DIB とメモリ DC。
struct Surface {
    mem_dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}

impl Surface {
    fn new(width: u32, height: u32) -> Option<Self> {
        // SAFETY: 画面 DC から互換 DC と DIB を作る。
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return None;
            }
            let mem_dc = CreateCompatibleDC(screen);
            if mem_dc.is_null() {
                ReleaseDC(std::ptr::null_mut(), screen);
                return None;
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..BITMAPINFOHEADER::default()
                },
                ..BITMAPINFO::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(
                screen,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            ReleaseDC(std::ptr::null_mut(), screen);
            if bitmap.is_null() || bits.is_null() {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                DeleteDC(mem_dc);
                return None;
            }
            let previous = SelectObject(mem_dc, bitmap);
            Some(Self {
                mem_dc,
                bitmap,
                previous,
                bits: bits.cast(),
                width,
                height,
            })
        }
    }

    fn update(&self, hwnd: HWND, pixmap: &Pixmap, blend: &BLENDFUNCTION) {
        let source = pixmap.data();
        let pixels = self.width as usize * self.height as usize;
        if source.len() < pixels * 4 {
            return;
        }
        // tiny-skia は RGBA、UpdateLayeredWindow は BGRA を期待する。
        // SAFETY: DIB のビット列は width*height*4 バイト確保済み。
        let destination = unsafe { std::slice::from_raw_parts_mut(self.bits, pixels * 4) };
        for index in 0..pixels {
            let offset = index * 4;
            destination[offset] = source[offset + 2];
            destination[offset + 1] = source[offset + 1];
            destination[offset + 2] = source[offset];
            destination[offset + 3] = source[offset + 3];
        }

        let size = SIZE {
            cx: self.width as i32,
            cy: self.height as i32,
        };
        let point = POINT { x: 0, y: 0 };
        // SAFETY: すべてのハンドルとバッファは有効。
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return;
            }
            let ok = UpdateLayeredWindow(
                hwnd,
                screen,
                std::ptr::null(),
                &size,
                self.mem_dc,
                &point,
                0,
                blend,
                ULW_ALPHA,
            );
            let error = GetLastError();
            ReleaseDC(std::ptr::null_mut(), screen);
            if ok == 0 {
                crate::debug!("UpdateLayeredWindow に失敗しました (error={error})");
            }
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: この Surface が作成した GDI オブジェクトだけを解放する。
        unsafe {
            SelectObject(self.mem_dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.mem_dc);
        }
    }
}

fn pixel_size(scale: f32) -> (i32, i32) {
    (
        (WINDOW_WIDTH * scale).round() as i32,
        (WINDOW_HEIGHT * scale).round() as i32,
    )
}

fn pill_rect(scale: f32) -> Rect {
    Rect::from_xywh(
        (WINDOW_WIDTH - PILL_WIDTH) * 0.5 * scale,
        (WINDOW_HEIGHT - PILL_HEIGHT) * 0.5 * scale,
        PILL_WIDTH * scale,
        PILL_HEIGHT * scale,
    )
    .unwrap_or_else(|| Rect::from_xywh(0.0, 0.0, 1.0, 1.0).expect("1x1 の有効な矩形"))
}

fn shadow_paths(scale: f32) -> Vec<Path> {
    let rect = pill_rect(scale);
    let mut paths = Vec::with_capacity(8);
    for step in 0..8 {
        let expand = (7.0 - step as f32) * 1.15 * scale;
        let Some(expanded) = Rect::from_xywh(
            rect.left() - expand,
            rect.top() - expand + 2.4 * scale,
            rect.width() + expand * 2.0,
            rect.height() + expand * 2.0,
        ) else {
            continue;
        };
        if let Some(path) = rounded_rect(expanded, (PILL_HEIGHT * scale + expand * 2.0) * 0.5) {
            paths.push(path);
        }
    }
    paths
}

/// 角丸長方形。`PathBuilder` の `round_rect` が無いため自前で組み立てる。
fn rounded_rect(rect: Rect, radius: f32) -> Option<Path> {
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

fn bar_gradient() -> Shader<'static> {
    let stops = vec![
        GradientStop::new(0.0, Color::from_rgba8(56, 189, 248, 255)),
        GradientStop::new(0.52, Color::from_rgba8(96, 165, 250, 255)),
        GradientStop::new(1.0, Color::from_rgba8(99, 102, 241, 255)),
    ];
    LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, 0.0),
        tiny_skia::Point::from_xy(PILL_WIDTH, 0.0),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(Color::from_rgba8(99, 102, 241, 255)))
}

fn glow_gradient() -> Shader<'static> {
    let stops = vec![
        GradientStop::new(0.0, Color::from_rgba8(56, 189, 248, 34)),
        GradientStop::new(1.0, Color::from_rgba8(99, 102, 241, 22)),
    ];
    LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, 0.0),
        tiny_skia::Point::from_xy(PILL_WIDTH, 0.0),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(Color::from_rgba8(99, 102, 241, 24)))
}

fn background_gradient() -> Shader<'static> {
    let stops = vec![
        GradientStop::new(0.0, Color::from_rgba8(255, 255, 255, 236)),
        GradientStop::new(0.55, Color::from_rgba8(250, 252, 255, 216)),
        GradientStop::new(1.0, Color::from_rgba8(236, 245, 255, 198)),
    ];
    LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, 0.0),
        tiny_skia::Point::from_xy(0.0, PILL_HEIGHT),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(Color::from_rgba8(255, 255, 255, 220)))
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match message {
        WM_NCHITTEST => return HTTRANSPARENT as isize,
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as isize,
        WM_ERASEBKGND => return 1,
        WM_TIMER => {
            // SAFETY: ウィンドウ生成時に Ctx を GWLP_USERDATA へ格納している。
            if let Some(ctx) = unsafe { ctx(hwnd) } {
                ctx.tick();
            }
            return 0;
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            // SAFETY: 同上。
            if let Some(ctx) = unsafe { ctx(hwnd) } {
                ctx.reposition();
            }
            return 0;
        }
        WM_DESTROY => {
            // SAFETY: SetTimer で作成したタイマーを破棄し、Ctx を回収する。
            unsafe {
                KillTimer(hwnd, TIMER_ID);
                let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
                if !pointer.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(pointer));
                }
            }
            return 0;
        }
        _ => {}
    }
    let _ = wparam;
    let _ = lparam;
    // SAFETY: 既定処理へ委譲する。
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

unsafe fn ctx(hwnd: HWND) -> Option<&'static mut Ctx> {
    // SAFETY: GWLP_USERDATA は生成時に設定した Box<Ctx> のポインタ。
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Ctx;
    // SAFETY: ポインタはウィンドウが存続する間だけ有効。
    unsafe { pointer.as_mut() }
}
