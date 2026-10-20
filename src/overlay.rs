//! 画面下部のフローティングバー。
//!
//! 待機時は細い半透明ライン、録音・推論時は小さなピルへ滑らかに展開する。
//! Win32 のレイヤードウィンドウに tiny-skia で描画するため、フォーカスを
//! 奪わず、クリックも透過する。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::Instant;

use tao::event_loop::EventLoopProxy;
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Paint, Path, PathBuilder,
    Pixmap, Rect, Shader, SpreadMode, Stroke, Transform,
};
use windows_sys::Win32::Foundation::{GetLastError, HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP,
    HDC, HGDIOBJ, ReleaseDC, ScreenToClient, SelectObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HTCLIENT,
    HTTRANSPARENT, HWND_TOPMOST, IDC_ARROW, IDC_HAND, KillTimer, LoadCursorW, MA_NOACTIVATE,
    RegisterClassW, SPI_GETWORKAREA, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetCursor, SetTimer,
    SetWindowLongPtrW, SetWindowPos, SystemParametersInfoW, ULW_ALPHA, UpdateLayeredWindow,
    WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCHITTEST, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::state::VisualState;
use crate::ui::UiEvent;
use crate::win::wide;

/// `Win32_UI_Controls` を追加せずに使うための定数。
const WM_MOUSELEAVE: u32 = 0x02A3;

const CLASS_NAME: &str = "MkVoiceJpOverlay";
const TIMER_ID: usize = 1;
/// 影を含めたウィンドウの論理サイズ。
const WINDOW_WIDTH: f32 = 170.0;
const WINDOW_HEIGHT: f32 = 56.0;
/// 待機時の細いライン。
const IDLE_WIDTH: f32 = 70.0;
const IDLE_HEIGHT: f32 = 3.0;
/// 録音・推論時の極小ピル。
const ACTIVE_WIDTH: f32 = 130.0;
const ACTIVE_HEIGHT: f32 = 28.0;
/// ピル内に高密度で並べるバー本数。
const BAR_COUNT: usize = 24;
/// バー間の目標ギャップ (論理 px)。
const BAR_GAP: f32 = 2.25;
/// ピル左右の最小余白 (論理 px)。
const BAR_SIDE_PAD: f32 = 3.0;
/// バーの最大高さ (論理 px)。
const BAR_MAX_HEIGHT: f32 = 20.5;

/// マイクレベル・表示状態・有効フラグを共有してバーを作る。
pub struct Overlay {
    hwnd: HWND,
}

impl Overlay {
    pub fn new(
        level: Arc<AtomicU32>,
        visual: Arc<AtomicU8>,
        enabled: Arc<AtomicBool>,
        proxy: EventLoopProxy<UiEvent>,
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

            let ctx = Box::new(Ctx::new(hwnd, level, visual, enabled, proxy));
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
    proxy: EventLoopProxy<UiEvent>,
    scale: f32,
    surface: Option<Surface>,
    pixmap: Option<Pixmap>,
    unit_circle: Option<Path>,
    /// 現在の論理サイズ。待機ラインと録音ピルの間を補間する。
    width: f32,
    height: f32,
    heights: [f32; BAR_COUNT],
    visible: bool,
    hover: bool,
    hover_t: f32,
    tracking_leave: bool,
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
        proxy: EventLoopProxy<UiEvent>,
    ) -> Self {
        let mut ctx = Self {
            hwnd,
            level,
            visual,
            enabled,
            proxy,
            scale: 1.0,
            surface: None,
            pixmap: None,
            unit_circle: PathBuilder::from_circle(0.0, 0.0, 1.0),
            width: IDLE_WIDTH,
            height: IDLE_HEIGHT,
            heights: [2.2; BAR_COUNT],
            visible: false,
            hover: false,
            hover_t: 0.0,
            tracking_leave: false,
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
                work.bottom - height - (6.0 * scale) as i32,
            )
        } else {
            (0, 0)
        };
        // SAFETY: 作成済みウィンドウの位置とサイズを更新する。
        unsafe {
            SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE);
        }
    }

    fn hit_rect(&self) -> Rect {
        let rect = content_rect(self.width, self.height, self.scale);
        let pad_y = if self.height < 8.0 {
            4.5 * self.scale
        } else {
            2.0 * self.scale
        };
        let pad_x = 3.0 * self.scale;
        Rect::from_xywh(
            rect.left() - pad_x,
            rect.top() - pad_y,
            rect.width() + pad_x * 2.0,
            rect.height() + pad_y * 2.0,
        )
        .unwrap_or(rect)
    }

    fn is_close(&self, x: f32, y: f32) -> bool {
        let rect = content_rect(self.width, self.height, self.scale);
        let (cx, cy) = close_center(rect, self.scale);
        let radius = 7.0 * self.scale;
        let dx = x - cx;
        let dy = y - cy;
        dx * dx + dy * dy <= radius * radius
    }

    fn is_hit(&self, x: f32, y: f32) -> bool {
        let hit = self.hit_rect();
        let inside_rect =
            x >= hit.left() && x <= hit.right() && y >= hit.top() && y <= hit.bottom();
        inside_rect || self.is_close(x, y)
    }

    fn on_mouse_move(&mut self, x: f32, y: f32) {
        let hit = self.is_hit(x, y);
        self.hover = hit;
        if hit && !self.tracking_leave {
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: hwnd はこのウィンドウ。track は有効な出力先。
            unsafe {
                TrackMouseEvent(&mut track);
            }
            self.tracking_leave = true;
        }
        // SAFETY: 標準カーソルを読み込むだけ。
        unsafe {
            let cursor = if hit { IDC_HAND } else { IDC_ARROW };
            SetCursor(LoadCursorW(std::ptr::null_mut(), cursor));
        }
    }

    fn on_mouse_leave(&mut self) {
        self.hover = false;
        self.tracking_leave = false;
    }

    fn on_click(&self, x: f32, y: f32) {
        if self.is_close(x, y) {
            let _ = self.proxy.send_event(UiEvent::Quit);
        } else if self.is_hit(x, y) {
            let _ = self.proxy.send_event(UiEvent::ShowSettings);
        }
    }

    fn tick(&mut self) {
        let state = VisualState::from_u8(self.visual.load(Ordering::Relaxed));
        if state != self.last_state {
            self.last_state = state;
            self.state_since = Instant::now();
        }
        let enabled = self.enabled.load(Ordering::Relaxed);
        let hover_target = if enabled && self.visible && self.hover {
            1.0
        } else {
            0.0
        };
        self.hover_t += (hover_target - self.hover_t) * 0.28;
        if (hover_target - self.hover_t).abs() < 0.01 {
            self.hover_t = hover_target;
        }

        if !enabled {
            if self.visible {
                self.alpha -= 0.09;
                if self.alpha <= 0.0 {
                    self.alpha = 0.0;
                    self.visible = false;
                    // SAFETY: 作成済みウィンドウを隠す。
                    unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(self.hwnd, SW_HIDE);
                    }
                } else {
                    self.render(VisualState::Idle);
                }
            }
            return;
        }

        if !self.visible {
            self.reposition();
            self.visible = true;
            self.alpha = 0.0;
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
        let active = state != VisualState::Idle;
        let target_width = if active { ACTIVE_WIDTH } else { IDLE_WIDTH };
        let target_height = if active { ACTIVE_HEIGHT } else { IDLE_HEIGHT };
        let width_speed = if target_width > self.width {
            0.34
        } else {
            0.20
        };
        let height_speed = if target_height > self.height {
            0.52
        } else {
            0.26
        };
        self.width += (target_width - self.width) * width_speed;
        self.height += (target_height - self.height) * height_speed;
        if (target_width - self.width).abs() < 0.05 {
            self.width = target_width;
        }
        if (target_height - self.height).abs() < 0.05 {
            self.height = target_height;
        }
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
            work.bottom - height - (6.0 * self.scale) as i32
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
        let base_rect = content_rect(self.width, self.height, scale);
        let lift = self.hover_t * 2.2 * scale;
        let rect = Rect::from_xywh(
            base_rect.left(),
            base_rect.top() - lift,
            base_rect.width(),
            base_rect.height(),
        )
        .unwrap_or(base_rect);
        let gate = ((self.height - IDLE_HEIGHT) / (ACTIVE_HEIGHT - IDLE_HEIGHT)).clamp(0.0, 1.0);

        // 柔らかいアンビエントシャドウ。待機ラインではかなり薄くする。
        for (index, path) in shadow_paths(rect, scale).iter().enumerate() {
            let alpha = ((2.0 + index as f32 * 1.5) * (0.28 + 0.72 * gate)) as u8;
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(30, 41, 59, alpha));
            pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
        }

        if let Some(pill) = rounded_rect(rect, rect.height() * 0.5) {
            let fill = Paint {
                shader: background_shader(rect),
                anti_alias: true,
                ..Paint::default()
            };
            pixmap.fill_path(&pill, &fill, FillRule::Winding, Transform::identity(), None);

            // 待機時は淡いブルーのラインとして視認できるよう色を足す。
            if gate < 0.98 {
                let mut tint = Paint::default();
                tint.set_color(Color::from_rgba8(
                    201,
                    221,
                    244,
                    ((1.0 - gate) * 120.0) as u8,
                ));
                pixmap.fill_path(&pill, &tint, FillRule::Winding, Transform::identity(), None);
            }

            let mut edge = Paint::default();
            edge.set_color(Color::from_rgba8(
                186,
                210,
                236,
                (100.0 + 80.0 * gate) as u8,
            ));
            pixmap.stroke_path(
                &pill,
                &edge,
                &Stroke {
                    width: 1.0 * scale,
                    ..Stroke::default()
                },
                Transform::identity(),
                None,
            );

            if rect.height() > 7.0 * scale {
                let mut highlight = Paint::default();
                highlight.set_color(Color::from_rgba8(
                    255,
                    255,
                    255,
                    (120.0 + 80.0 * gate) as u8,
                ));
                pixmap.stroke_path(
                    &pill,
                    &highlight,
                    &Stroke {
                        width: 0.7 * scale,
                        ..Stroke::default()
                    },
                    Transform::from_translate(0.0, 0.55 * scale),
                    None,
                );
            }
        }

        // 上辺のきらめきは録音時だけ。
        if gate > 0.18 {
            let mut glint = Paint::default();
            glint.set_color(Color::from_rgba8(255, 255, 255, (110.0 * gate) as u8));
            let glint_rect = Rect::from_xywh(
                rect.left() + rect.width() * 0.18,
                rect.top() + 1.2 * scale,
                rect.width() * 0.64,
                0.8 * scale,
            );
            if let Some(glint_rect) = glint_rect {
                pixmap.fill_rect(glint_rect, &glint, Transform::identity(), None);
            }
        }

        Self::render_bars(
            pixmap,
            state,
            rect,
            scale,
            gate,
            &mut self.heights,
            &self.unit_circle,
            self.state_since,
            &self.level,
        );

        if self.hover_t > 0.04 {
            let (cx, cy) = close_center(rect, scale);
            let radius = 6.5 * scale;
            let mut circle = Paint::default();
            circle.set_color(Color::from_rgba8(
                255,
                255,
                255,
                (180.0 * self.hover_t) as u8,
            ));
            if let Some(circle_path) = PathBuilder::from_circle(cx, cy, radius) {
                pixmap.fill_path(
                    &circle_path,
                    &circle,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
                let mut edge = Paint::default();
                edge.set_color(Color::from_rgba8(
                    148,
                    163,
                    184,
                    (95.0 * self.hover_t) as u8,
                ));
                pixmap.stroke_path(
                    &circle_path,
                    &edge,
                    &Stroke {
                        width: 0.8 * scale,
                        ..Stroke::default()
                    },
                    Transform::identity(),
                    None,
                );
            }

            let mut cross = PathBuilder::new();
            let r = 2.6 * scale;
            cross.move_to(cx - r, cy - r);
            cross.line_to(cx + r, cy + r);
            cross.move_to(cx + r, cy - r);
            cross.line_to(cx - r, cy + r);
            if let Some(cross) = cross.finish() {
                let mut paint = Paint::default();
                paint.set_color(Color::from_rgba8(71, 85, 105, (215.0 * self.hover_t) as u8));
                paint.anti_alias = true;
                pixmap.stroke_path(
                    &cross,
                    &paint,
                    &Stroke {
                        width: 1.25 * scale,
                        line_cap: LineCap::Round,
                        line_join: LineJoin::Round,
                        ..Stroke::default()
                    },
                    Transform::identity(),
                    None,
                );
            }
        }

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
        gate: f32,
        heights: &mut [f32; BAR_COUNT],
        unit_circle: &Option<Path>,
        state_since: Instant,
        level: &AtomicU32,
    ) {
        if gate <= 0.02 {
            return;
        }

        let center_y = rect.top() + rect.height() * 0.5;
        let center_x = rect.left() + rect.width() * 0.5;
        // 24本をほぼ隙間なく並べる。ピルがまだ細い間は外へはみ出すため描かない。
        let available = (rect.width() - BAR_SIDE_PAD * 2.0 * scale).max(1.0);
        let target_gap = BAR_GAP * scale;
        let bar_width = ((available - target_gap * (BAR_COUNT as f32 - 1.0)) / BAR_COUNT as f32)
            .clamp(2.0 * scale, 3.0 * scale);
        let gap = (available - bar_width * BAR_COUNT as f32) / (BAR_COUNT as f32 - 1.0);
        if gap < 2.0 * scale {
            return;
        }
        let group_width = BAR_COUNT as f32 * bar_width + (BAR_COUNT as f32 - 1.0) * gap;
        let start_x = rect.left() + (rect.width() - group_width) * 0.5;

        let elapsed = state_since.elapsed().as_secs_f32();
        match state {
            VisualState::Recording => {
                // RMS を強めに持ち上げ、声の立ち上がりへ俊敏に追従させる。
                let rms = f32::from_bits(level.load(Ordering::Relaxed));
                let normalized = (rms.max(0.0) * 14.0).sqrt().clamp(0.0, 1.0);
                let phase = elapsed * 7.4;
                for (index, current) in heights.iter_mut().enumerate() {
                    let x = index as f32 / (BAR_COUNT as f32 - 1.0) * 2.0 - 1.0;
                    let bell = (-2.8 * x * x).exp();
                    // 中央から左右へ流れる成分と、微小な揺らぎを重ねる。
                    let ripple = (phase - index as f32 * 0.42).sin();
                    let shimmer = noise(elapsed * 9.0 + index as f32 * 2.17);
                    let target = (2.4
                        + normalized * (4.5 + 13.5 * bell)
                        + ripple * 0.35 * (0.35 + 0.65 * bell)
                        + shimmer * 0.30 * (0.25 + 0.75 * bell))
                        .clamp(2.0, BAR_MAX_HEIGHT);
                    let coefficient = if target > *current { 0.82 } else { 0.42 };
                    *current += (target - *current) * coefficient;
                }
            }
            VisualState::Processing | VisualState::Pasting => {
                let pulse = (elapsed * 9.0).sin() * 0.5 + 0.5;
                for height in heights.iter_mut() {
                    let target = 2.0 + pulse * 2.5;
                    *height += (target - *height) * 0.32;
                }
            }
            VisualState::Idle => {
                for height in heights.iter_mut() {
                    *height += (0.0 - *height) * 0.35;
                }
            }
        }

        let converge = match state {
            VisualState::Processing | VisualState::Pasting => (elapsed * 5.5).clamp(0.0, 1.0),
            _ => 0.0,
        };

        if converge > 0.0
            && gate > 0.1
            && let Some(circle) = unit_circle
        {
            let pulse = (elapsed * 9.0).sin() * 0.5 + 0.5;
            let mut glow = Paint::default();
            glow.set_color(Color::from_rgba8(99, 102, 241, (22.0 * gate) as u8));
            let radius = (9.0 + pulse * 4.0) * scale * gate.max(0.35);
            pixmap.fill_path(
                circle,
                &glow,
                FillRule::Winding,
                Transform::from_scale(radius, radius).post_translate(center_x, center_y),
                None,
            );
            glow.set_color(Color::from_rgba8(129, 140, 248, (42.0 * gate) as u8));
            let radius = (5.0 + pulse * 2.0) * scale * gate.max(0.35);
            pixmap.fill_path(
                circle,
                &glow,
                FillRule::Winding,
                Transform::from_scale(radius, radius).post_translate(center_x, center_y),
                None,
            );
        }

        let paint = Paint {
            shader: bar_shader(start_x, start_x + group_width),
            anti_alias: true,
            ..Paint::default()
        };
        let glow = Paint {
            shader: glow_shader(start_x, start_x + group_width),
            ..Paint::default()
        };

        for (index, height) in heights.iter().enumerate() {
            let original_x = start_x + index as f32 * (bar_width + gap);
            let x = original_x + (center_x - original_x) * converge;
            let height = (*height * gate).clamp(0.0, BAR_MAX_HEIGHT * scale);
            if height < 1.0 * scale {
                continue;
            }
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
            if converge < 0.9 && gate > 0.2 {
                let glow_rect = Rect::from_xywh(
                    x - 0.9 * scale,
                    top - 0.7 * scale,
                    bar_width + 1.8 * scale,
                    height + 1.4 * scale,
                );
                if let Some(glow_rect) = glow_rect
                    && let Some(path) = rounded_rect(glow_rect, (bar_width + 1.8 * scale) * 0.5)
                {
                    pixmap.fill_path(&path, &glow, FillRule::Winding, Transform::identity(), None);
                }
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

/// 決定的な疑似ノイズ (-1.0..=1.0)。乱数クレートを増やさず微小な揺らぎを作る。
fn noise(seed: f32) -> f32 {
    let value = (seed * 12.9898).sin() * 43_758.547;
    (value - value.floor()) * 2.0 - 1.0
}

fn close_center(rect: Rect, scale: f32) -> (f32, f32) {
    (rect.right() + 9.0 * scale, rect.top() + rect.height() * 0.5)
}

fn pixel_size(scale: f32) -> (i32, i32) {
    (
        (WINDOW_WIDTH * scale).round() as i32,
        (WINDOW_HEIGHT * scale).round() as i32,
    )
}

fn content_rect(width: f32, height: f32, scale: f32) -> Rect {
    let width = (width * scale).max(1.0);
    let height = (height * scale).max(1.0);
    Rect::from_xywh(
        (WINDOW_WIDTH * scale - width) * 0.5,
        (WINDOW_HEIGHT * scale - height) * 0.5,
        width,
        height,
    )
    .unwrap_or_else(|| Rect::from_xywh(0.0, 0.0, 1.0, 1.0).expect("1x1 の有効な矩形"))
}

fn shadow_paths(rect: Rect, scale: f32) -> Vec<Path> {
    let mut paths = Vec::with_capacity(7);
    for step in 0..7 {
        let expand = (6.0 - step as f32) * 1.15 * scale;
        let Some(expanded) = Rect::from_xywh(
            rect.left() - expand,
            rect.top() - expand + 2.0 * scale,
            rect.width() + expand * 2.0,
            rect.height() + expand * 2.0,
        ) else {
            continue;
        };
        if let Some(path) = rounded_rect(expanded, expanded.height() * 0.5) {
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

fn background_shader(rect: Rect) -> Shader<'static> {
    let stops = vec![
        GradientStop::new(0.0, Color::from_rgba8(255, 255, 255, 236)),
        GradientStop::new(0.55, Color::from_rgba8(250, 252, 255, 214)),
        GradientStop::new(1.0, Color::from_rgba8(234, 244, 255, 196)),
    ];
    LinearGradient::new(
        tiny_skia::Point::from_xy(rect.left(), rect.top()),
        tiny_skia::Point::from_xy(rect.left(), rect.bottom()),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(Color::from_rgba8(255, 255, 255, 220)))
}

fn bar_shader(start: f32, end: f32) -> Shader<'static> {
    let stops = vec![
        GradientStop::new(0.0, Color::from_rgba8(56, 189, 248, 255)),
        GradientStop::new(0.52, Color::from_rgba8(96, 165, 250, 255)),
        GradientStop::new(1.0, Color::from_rgba8(99, 102, 241, 255)),
    ];
    LinearGradient::new(
        tiny_skia::Point::from_xy(start, 0.0),
        tiny_skia::Point::from_xy(end, 0.0),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(Color::from_rgba8(99, 102, 241, 255)))
}

fn glow_shader(start: f32, end: f32) -> Shader<'static> {
    let stops = vec![
        GradientStop::new(0.0, Color::from_rgba8(56, 189, 248, 10)),
        GradientStop::new(1.0, Color::from_rgba8(99, 102, 241, 7)),
    ];
    LinearGradient::new(
        tiny_skia::Point::from_xy(start, 0.0),
        tiny_skia::Point::from_xy(end, 0.0),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(Color::from_rgba8(99, 102, 241, 22)))
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    let (mouse_x, mouse_y) = client_point(lparam);
    match message {
        WM_NCHITTEST => {
            // WM_NCHITTEST の座標だけはスクリーン座標。クライアント座標へ直す。
            let mut local = POINT {
                x: mouse_x as i32,
                y: mouse_y as i32,
            };
            // SAFETY: local は有効な出力先。
            unsafe {
                ScreenToClient(hwnd, &mut local);
            }
            // SAFETY: GWLP_USERDATA はこのウィンドウの Ctx を指す。
            if let Some(ctx) = unsafe { ctx(hwnd) }
                && ctx.enabled.load(Ordering::Relaxed)
                && ctx.visible
                && ctx.is_hit(local.x as f32, local.y as f32)
            {
                return HTCLIENT as isize;
            }
            if let Some(ctx) = unsafe { ctx(hwnd) } {
                ctx.on_mouse_leave();
            }
            return HTTRANSPARENT as isize;
        }
        WM_MOUSEMOVE => {
            // SAFETY: 同上。
            if let Some(ctx) = unsafe { ctx(hwnd) } {
                ctx.on_mouse_move(mouse_x, mouse_y);
            }
            return 0;
        }
        WM_MOUSELEAVE => {
            // SAFETY: 同上。
            if let Some(ctx) = unsafe { ctx(hwnd) } {
                ctx.on_mouse_leave();
            }
            return 0;
        }
        WM_LBUTTONDOWN => return 0,
        WM_LBUTTONUP => {
            // SAFETY: 同上。
            if let Some(ctx) = unsafe { ctx(hwnd) } {
                ctx.on_click(mouse_x, mouse_y);
            }
            return 0;
        }
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

fn client_point(lparam: isize) -> (f32, f32) {
    let x = (lparam & 0xFFFF) as u16 as i16 as f32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as f32;
    (x, y)
}

unsafe fn ctx(hwnd: HWND) -> Option<&'static mut Ctx> {
    // SAFETY: GWLP_USERDATA は生成時に設定した Box<Ctx> のポインタ。
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Ctx;
    // SAFETY: ポインタはウィンドウが存続する間だけ有効。
    unsafe { pointer.as_mut() }
}
