//! 設定画面 (WebView2 / wry) とイベントループ。
//!
//! tao のイベントループがキーボードフックのメッセージポンプを兼ねる。
//! IPC は JSON 文字列で受け取り、状態は定期的に JS へ push する。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::WindowExtWindows;
use tao::window::{Window, WindowBuilder};
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA, SystemParametersInfoW,
};
use wry::{WebView, WebViewBuilder};

use crate::app::App;
use crate::config::{self, MODEL_IDS, Settings, SoundTheme};
use crate::models;
use crate::overlay::Overlay;
use crate::sound;
use crate::stats::Stats;
use crate::text;
use crate::tray;
use crate::win;

/// トレイ・ワーカー・IPC からメインスレッドへ送るイベント。
pub(crate) enum UiEvent {
    ShowSettings,
    /// 初回起動のオンボーディングで「はじめる」が押された。
    OnboardingDone,
    ReloadDictionaries,
    Ipc(String),
    Tick,
    ModelsChanged,
    Quit,
}

/// 埋め込みページが持つ文書 URL を組み立てる。
///
/// wry の `with_html` は `NavigateToString` を使うが、WebView2 の実装は
/// 内部で HTML を `data:text/html;charset=utf-8;base64,...` へ変換して
/// ナビゲーションする。`about:blank` だけを許可していると、この変換後の
/// 初回ナビゲーションが拒否されて設定画面が空になる。
fn embedded_page_url(html: &str) -> String {
    format!(
        "data:text/html;charset=utf-8;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(html)
    )
}

/// WebView が読み込んでよい URL か。
///
/// 設定画面とオンボーディングは `with_html` で埋め込んだ静的ページだけを
/// 表示する。`embedded` はそのページの文書 URL で、完全一致するものだけを
/// 許可する。任意の `data:` や `about:` を許可すると、そのページが IPC
/// (`window.ipc`) を叩ける状態になるため、内容が一致しない URL は拒否する。
fn is_builtin_page(url: &str, embedded: &str) -> bool {
    url.is_empty()
        || url == "about:blank"
        || url.starts_with("about:blank#")
        || url == embedded
        || url
            .strip_prefix(embedded)
            .is_some_and(|rest| rest.starts_with('#'))
}

/// 設定画面に一度だけ表示する通知。
struct Notice {
    id: u64,
    kind: &'static str,
    message: String,
}

/// 設定画面とトレイを起動し、終了までイベントループを回す。
pub(crate) fn run(mut app: App) -> ! {
    // `first_run` は保存済みの値を維持し、CLI で上書きされた実行時設定だけを取り込む。
    let (stored, _) = Settings::load(Path::new(config::CONFIG_PATH));
    let mut settings =
        Settings::from_config_with_first_run(&app.shared().snapshot(), stored.first_run);
    let first_run = settings.first_run;

    // 起動時にレジストリと設定を一致させる。
    if win::autostart_enabled() != settings.autostart
        && let Err(exc) = win::set_autostart(settings.autostart)
    {
        crate::warn!("自動起動を同期できません: {exc}");
    }

    let mut builder = EventLoopBuilder::<UiEvent>::with_user_event();
    let event_loop = builder.build();
    let proxy = event_loop.create_proxy();

    let window = match WindowBuilder::new()
        .with_title("BAREVoiceJP — 設定")
        .with_inner_size(LogicalSize::new(1080.0, 800.0))
        .with_min_inner_size(LogicalSize::new(1000.0, 720.0))
        .with_decorations(false)
        .with_resizable(false)
        .with_visible(false)
        .with_background_color((243, 247, 252, 255))
        .build(&event_loop)
    {
        Ok(window) => window,
        Err(exc) => {
            crate::error!("設定ウィンドウを作成できません: {exc}");
            app.shutdown();
            std::process::exit(1);
        }
    };

    place_window(&window);

    let ipc_proxy = proxy.clone();
    let html = include_str!("web/settings.html");
    let embedded = Arc::<str>::from(embedded_page_url(html));
    let nav_embedded = Arc::clone(&embedded);
    let script: &'static str = include_str!("web/settings.js");
    let webview = match WebViewBuilder::new()
        .with_html(html)
        .with_initialization_script(script)
        .with_background_color((243, 247, 252, 255))
        .with_devtools(cfg!(debug_assertions))
        .with_navigation_handler(move |url| is_builtin_page(&url, &nav_embedded))
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_permission_handler(|_| wry::PermissionResponse::Deny)
        .with_ipc_handler(move |request| {
            // 組み込みページ以外からの IPC は受け付けない。
            let source = request.uri().to_string();
            if !is_builtin_page(&source, &embedded) {
                crate::warn!("組み込みページ以外からの IPC を拒否しました: {source}");
                return;
            }
            let _ = ipc_proxy.send_event(UiEvent::Ipc(request.body().clone()));
        })
        .build(&window)
    {
        Ok(webview) => webview,
        Err(exc) => {
            crate::error!("WebView2 を初期化できません: {exc}");
            app.shutdown();
            std::process::exit(1);
        }
    };

    let enabled = Arc::new(AtomicBool::new(settings.floating_bar));
    let _overlay = match Overlay::new(
        Arc::clone(app.level()),
        Arc::clone(app.shared().visual_state()),
        Arc::clone(&enabled),
        proxy.clone(),
    ) {
        Ok(overlay) => Some(overlay),
        Err(exc) => {
            crate::warn!("フローティングバーを初期化できません: {exc}");
            None
        }
    };

    if let Err(exc) = tray::spawn(proxy.clone()) {
        crate::warn!("トレイアイコンを初期化できません: {exc}");
    }

    // 初回起動時だけ、設定画面とは別のオンボーディングを開く。
    // 「はじめる」までは設定画面を表示しない。
    let onboarding = if first_run {
        match build_onboarding(&event_loop, proxy.clone()) {
            Ok(pair) => Some(pair),
            Err(exc) => {
                crate::warn!("{exc}");
                None
            }
        }
    } else {
        None
    };

    let tick_proxy = proxy.clone();
    std::thread::Builder::new()
        .name("BAREVoiceJP-ui-tick".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(50));
                if tick_proxy.send_event(UiEvent::Tick).is_err() {
                    break;
                }
            }
        })
        .ok();

    let mut notice: Option<Notice> = None;
    let mut notice_id = 0u64;
    let mut last_state = Instant::now() - Duration::from_secs(1);

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UiEvent::ShowSettings) => {
                show_window(&window, &webview, "control");
                push_state(&webview, &app, &settings, &notice);
                push_level(&webview, &app);
            }
            Event::UserEvent(UiEvent::OnboardingDone) => {
                if settings.first_run {
                    settings.first_run = false;
                    if let Err(exc) = settings.save(Path::new(config::CONFIG_PATH)) {
                        crate::warn!("初回起動の完了状態を保存できません: {exc}");
                    }
                }
                if let Some((onboarding_window, _)) = &onboarding {
                    onboarding_window.set_visible(false);
                }
                // 初回はモデルが未ダウンロードなので、そのまま「モデル」タブへ誘導する。
                show_window(&window, &webview, "models");
                push_state(&webview, &app, &settings, &notice);
                push_level(&webview, &app);
            }
            Event::UserEvent(UiEvent::Quit) => {
                tray::shutdown();
                app.shutdown();
                std::process::exit(0);
            }
            Event::UserEvent(UiEvent::ReloadDictionaries) => {
                app.reload_dictionaries();
                set_notice(
                    &mut notice,
                    &mut notice_id,
                    "success",
                    "辞書を再読み込みしました",
                );
                push_state(&webview, &app, &settings, &notice);
            }
            Event::UserEvent(UiEvent::ModelsChanged) => {
                let config = app.shared().snapshot();
                if models::is_installed(&config, &settings.model) && !app.shared().engine_status().0
                {
                    app.reconfigure();
                }
                push_state(&webview, &app, &settings, &notice);
            }
            Event::UserEvent(UiEvent::Ipc(message)) => {
                handle_ipc(
                    &message,
                    &mut app,
                    &window,
                    &mut settings,
                    &enabled,
                    &proxy,
                    &mut notice,
                    &mut notice_id,
                );
                push_state(&webview, &app, &settings, &notice);
                push_level(&webview, &app);
            }
            Event::UserEvent(UiEvent::Tick) => {
                app.poll_hybrid();
                if window.is_visible() {
                    push_level(&webview, &app);
                    if last_state.elapsed() >= Duration::from_millis(500) {
                        last_state = Instant::now();
                        push_state(&webview, &app, &settings, &notice);
                    }
                }
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if onboarding
                    .as_ref()
                    .is_some_and(|(onboarding_window, _)| onboarding_window.id() == window_id)
                {
                    if let Some((onboarding_window, _)) = &onboarding {
                        onboarding_window.set_visible(false);
                    }
                } else {
                    window.set_visible(false);
                }
            }
            Event::LoopDestroyed => {
                app.shutdown();
                std::process::exit(0);
            }
            _ => {}
        }
    });
}

/// 設定画面をフローティングバーの直上へ出し、最前面へ持ってくる。
///
/// `tab` で開くタブを指定する。手動で開いたときは「操作」、オンボーディング完了後は「モデル」。
fn show_window(window: &Window, webview: &WebView, tab: &str) {
    window.set_minimized(false);
    place_window(window);
    window.set_visible(true);
    window.set_focus();
    win::force_foreground(window.hwnd() as *mut core::ffi::c_void);
    let script = format!(
        "window.__barevoice && window.__barevoice.openTab && window.__barevoice.openTab('{tab}');"
    );
    if let Err(exc) = webview.evaluate_script(&script) {
        crate::debug!("{tab} タブへ切り替えられません: {exc}");
    }
}

/// プライマリディスプレイのワークエリアを返す。
fn work_area() -> Option<RECT> {
    let mut work: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: work は有効な出力先。SPI_GETWORKAREA はプライマリのワークエリアを返す。
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0) };
    (ok != 0).then_some(work)
}

/// プライマリディスプレイの下端寄り (フローティングバーの直上) へ設定画面を移動する。
fn place_window(window: &Window) {
    let Some(work) = work_area() else {
        return;
    };
    let size = window.outer_size();
    let width = work.right - work.left;
    let x = work.left + (width - size.width as i32) / 2;
    // フローティングバーの可視部分 (下端から約 28.5 論理 px) のすぐ上に来るよう 38 論理 px 空ける。
    let gap = (38.0 * window.scale_factor()).round() as i32;
    let y = (work.bottom - size.height as i32 - gap).max(work.top);
    window.set_outer_position(PhysicalPosition::new(x, y));
}

/// プライマリディスプレイの中央へオンボーディングを配置する。
///
/// 初回起動時はフローティングバーの位置に関係なく、画面の X / Y とも完全な中央に出す。
fn place_window_centered(window: &Window) {
    // SAFETY: GetSystemMetrics は引数なしで副作用もない。プライマリの画面サイズを返す。
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if width <= 0 || height <= 0 {
        return;
    }
    let size = window.outer_size();
    let x = (width - size.width as i32) / 2;
    let y = (height - size.height as i32) / 2;
    window.set_outer_position(PhysicalPosition::new(x, y));
}

/// 初回起動用のオンボーディングを作成して表示する。
///
/// サイズは通常の設定画面と揃える。
fn build_onboarding(
    event_loop: &EventLoop<UiEvent>,
    proxy: EventLoopProxy<UiEvent>,
) -> Result<(Window, WebView), String> {
    let window = WindowBuilder::new()
        .with_title("BAREVoiceJP — ようこそ")
        .with_inner_size(LogicalSize::new(1080.0, 800.0))
        .with_min_inner_size(LogicalSize::new(1000.0, 720.0))
        .with_decorations(false)
        .with_resizable(false)
        .with_visible(false)
        .with_background_color((247, 250, 255, 255))
        .build(event_loop)
        .map_err(|exc| format!("オンボーディングウィンドウを作成できません: {exc}"))?;

    let html = include_str!("web/onboarding.html");
    let embedded = Arc::<str>::from(embedded_page_url(html));
    let ipc_embedded = Arc::clone(&embedded);
    let webview = WebViewBuilder::new()
        .with_html(html)
        .with_background_color((247, 250, 255, 255))
        .with_devtools(cfg!(debug_assertions))
        .with_navigation_handler(move |url| is_builtin_page(&url, &embedded))
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_permission_handler(|_| wry::PermissionResponse::Deny)
        .with_ipc_handler(move |request| {
            let source = request.uri().to_string();
            if !is_builtin_page(&source, &ipc_embedded) {
                crate::warn!("組み込みページ以外からの IPC を拒否しました: {source}");
                return;
            }
            let Ok(value) = serde_json::from_str::<Value>(request.body()) else {
                return;
            };
            if value.get("cmd").and_then(Value::as_str) == Some("get_started") {
                let _ = proxy.send_event(UiEvent::OnboardingDone);
            }
        })
        .build(&window)
        .map_err(|exc| format!("オンボーディング画面を初期化できません: {exc}"))?;

    place_window_centered(&window);
    window.set_visible(true);
    window.set_focus();
    win::force_foreground(window.hwnd() as *mut core::ffi::c_void);
    Ok((window, webview))
}

/// 連番付きの通知をセットする (同じ文面でも再表示できる)。
fn set_notice(notice: &mut Option<Notice>, id: &mut u64, kind: &'static str, message: &str) {
    *id += 1;
    *notice = Some(Notice {
        id: *id,
        kind,
        message: message.to_string(),
    });
}

/// 設定画面からの JSON コマンドを処理する。
#[allow(clippy::too_many_arguments)]
fn handle_ipc(
    message: &str,
    app: &mut App,
    window: &Window,
    settings: &mut Settings,
    enabled: &AtomicBool,
    proxy: &EventLoopProxy<UiEvent>,
    notice: &mut Option<Notice>,
    notice_id: &mut u64,
) {
    let Ok(value) = serde_json::from_str::<Value>(message) else {
        return;
    };
    let Some(command) = value.get("cmd").and_then(Value::as_str) else {
        return;
    };
    match command {
        "save_settings" => save_settings(
            value.get("settings").cloned().unwrap_or(Value::Null),
            app,
            settings,
            enabled,
            notice,
            notice_id,
        ),
        "download_model" => {
            let Some(id) = value.get("id").and_then(Value::as_str) else {
                return;
            };
            if !MODEL_IDS.contains(&id) {
                set_notice(notice, notice_id, "error", "不明なモデルです");
                return;
            }
            let state = models::download_state(id);
            if matches!(
                state.phase,
                models::Phase::Downloading | models::Phase::Verifying | models::Phase::Extracting
            ) {
                return;
            }
            let proxy = proxy.clone();
            let base_url = app.shared().snapshot().model_base_url;
            models::download(id, &base_url, move || {
                let _ = proxy.send_event(UiEvent::ModelsChanged);
            });
            set_notice(notice, notice_id, "info", "ダウンロードを開始しました");
        }
        "delete_model" => {
            let Some(id) = value.get("id").and_then(Value::as_str) else {
                return;
            };
            let config = app.shared().snapshot();
            match models::delete(&config, id) {
                Ok(bytes) => set_notice(
                    notice,
                    notice_id,
                    "success",
                    &format!("{} を解放しました", models::format_bytes(bytes)),
                ),
                Err(exc) => set_notice(notice, notice_id, "error", &exc),
            }
        }
        "add_word" => {
            let target = value.get("target").and_then(Value::as_str).unwrap_or("");
            let reading = value.get("reading").and_then(Value::as_str).unwrap_or("");
            match text::add_word(Path::new(text::WORDS_PATH), target, reading) {
                Ok(()) => {
                    app.reload_dictionaries();
                    set_notice(notice, notice_id, "success", "辞書に追加しました");
                }
                Err(exc) => set_notice(notice, notice_id, "error", &exc),
            }
        }
        "delete_word" => {
            let target = value.get("target").and_then(Value::as_str).unwrap_or("");
            let reading = value.get("reading").and_then(Value::as_str);
            match text::remove_word(Path::new(text::WORDS_PATH), target, reading) {
                Ok(0) => set_notice(notice, notice_id, "error", "単語が見つかりません"),
                Ok(_) => {
                    app.reload_dictionaries();
                    set_notice(notice, notice_id, "success", "辞書から削除しました");
                }
                Err(exc) => set_notice(notice, notice_id, "error", &exc),
            }
        }
        "open_words" => open_in_notepad(text::WORDS_PATH, notice, notice_id),
        "open_replacements" => open_in_notepad(text::REPLACEMENTS_PATH, notice, notice_id),
        "play_sound" => {
            let cue = match value.get("kind").and_then(Value::as_str) {
                Some("start") => sound::Cue::Start,
                _ => sound::Cue::Finish,
            };
            let theme = value
                .get("theme")
                .and_then(Value::as_str)
                .map(SoundTheme::parse)
                .unwrap_or_else(|| SoundTheme::parse(&settings.sound_theme));
            sound::play(theme, cue, settings.sound_volume);
        }
        "window" => match value.get("action").and_then(Value::as_str) {
            Some("minimize") => window.set_minimized(true),
            Some("close") => window.set_visible(false),
            Some("drag") => win::begin_drag(window.hwnd() as *mut core::ffi::c_void),
            _ => {}
        },
        "reload_dictionaries" => {
            app.reload_dictionaries();
            set_notice(notice, notice_id, "success", "辞書を再読み込みしました");
        }
        _ => {}
    }
}

/// 設定を検証して反映し、マイク・ホットキー・自動起動へ同期する。
fn save_settings(
    value: Value,
    app: &mut App,
    settings: &mut Settings,
    enabled: &AtomicBool,
    notice: &mut Option<Notice>,
    notice_id: &mut u64,
) {
    let Ok(mut next) = serde_json::from_value::<Settings>(value) else {
        set_notice(notice, notice_id, "error", "設定の形式が不正です");
        return;
    };
    if !MODEL_IDS.contains(&next.model.as_str()) {
        set_notice(notice, notice_id, "error", "不明なモデルです");
        return;
    }
    let hotkey = match win::parse_hotkey(&next.hotkey) {
        Ok(hotkey) => hotkey,
        Err(exc) => {
            set_notice(notice, notice_id, "error", &exc);
            return;
        }
    };

    let previous = settings.clone();
    let mut config = app.shared().snapshot();
    // `model_base_url` はレンダラーへ渡していないため、保存要求には含まれない。
    // 空文字で上書きせず、現在の実行時設定を維持する。
    next.model_base_url.clone_from(&config.model_base_url);
    next.apply_to(&mut config);
    let mut applied = Settings::from_config_with_first_run(&config, next.first_run);

    if applied.hotkey != previous.hotkey {
        win::set_hotkey(hotkey);
    }
    if applied.floating_bar != previous.floating_bar {
        enabled.store(applied.floating_bar, Ordering::SeqCst);
    }
    if applied.autostart != previous.autostart
        && let Err(exc) = win::set_autostart(applied.autostart)
    {
        set_notice(notice, notice_id, "error", &exc);
    }

    if applied.device != previous.device
        && let Err(exc) = app.restart_capture(&config)
    {
        // 開けなかったデバイスを保存したままにせず、元のマイクへ戻す。
        config.device = previous.device.clone();
        app.apply_config(config.clone());
        let _ = app.restart_capture(&config);
        applied = Settings::from_config_with_first_run(&config, previous.first_run);
        let _ = applied.save(Path::new(config::CONFIG_PATH));
        *settings = applied;
        set_notice(
            notice,
            notice_id,
            "error",
            &format!("マイクを切り替えられませんでした。元のマイクに戻しました: {exc}"),
        );
        return;
    }
    // マイクを開き直せた場合だけ、共有設定へ反映する。
    app.apply_config(config);

    if let Err(exc) = applied.save(Path::new(config::CONFIG_PATH)) {
        set_notice(notice, notice_id, "error", &exc);
    }
    *settings = applied;
}

fn open_in_notepad(path: &str, notice: &mut Option<Notice>, notice_id: &mut u64) {
    // PATH やカレントディレクトリの探索順に依存せず、Windows 標準の
    // notepad.exe を明示的に起動する。
    let notepad = std::env::var_os("SystemRoot")
        .map(|root| {
            std::path::PathBuf::from(root)
                .join("System32")
                .join("notepad.exe")
        })
        .filter(|path| path.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from("notepad.exe"));
    match std::process::Command::new(notepad).arg(path).spawn() {
        Ok(_) => {}
        Err(exc) => set_notice(
            notice,
            notice_id,
            "error",
            &format!("エディタを起動できません: {exc}"),
        ),
    }
}

/// 入力レベルだけを JS へ送る (ティックごとの軽量更新)。
fn push_level(webview: &WebView, app: &App) {
    let level = f32::from_bits(app.level().load(Ordering::Relaxed));
    if let Err(exc) = webview.evaluate_script(&format!(
        "window.__barevoice && window.__barevoice.level({level:.4});"
    )) {
        crate::debug!("入力レベルを送れません: {exc}");
    }
}

/// 設定・モデル・辞書などの全体状態を JS へ送る。
fn push_state(webview: &WebView, app: &App, settings: &Settings, notice: &Option<Notice>) {
    let payload = snapshot(app, settings, notice);
    let text = payload
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    if let Err(exc) = webview.evaluate_script(&format!(
        "window.__barevoice && window.__barevoice.apply({text});"
    )) {
        crate::debug!("設定状態を送れません: {exc}");
    }
}

/// JS の `apply()` に渡す JSON を組み立てる。
fn snapshot(app: &App, settings: &Settings, notice: &Option<Notice>) -> Value {
    let shared = app.shared();
    let config = shared.snapshot();
    let (engine_ready, engine_building, engine_error) = shared.engine_status();
    let level = f32::from_bits(app.level().load(Ordering::Relaxed));

    let model_list = models::SPECS
        .iter()
        .map(|spec| {
            let installed = models::is_installed(&config, spec.id);
            let bytes = if installed {
                models::installed_bytes(&config.model_dir_for(spec.id))
            } else {
                0
            };
            let download = models::download_state(spec.id);
            json!({
                "id": spec.id,
                "name": spec.name,
                "subtitle": spec.subtitle,
                "size": spec.size_label,
                "speed": spec.speed,
                "accuracy": spec.accuracy,
                "recommended": spec.recommended,
                "installed": installed,
                "installedBytes": bytes,
                "installedLabel": models::format_bytes(bytes),
                "download": {
                    "phase": match download.phase {
                        models::Phase::Idle => "idle",
                        models::Phase::Downloading => "downloading",
                        models::Phase::Verifying => "verifying",
                        models::Phase::Extracting => "extracting",
                        models::Phase::Done => "done",
                        models::Phase::Failed => "failed",
                    },
                    "downloaded": download.downloaded,
                    "total": download.total,
                    "error": download.error,
                }
            })
        })
        .collect::<Vec<_>>();

    let device_list = crate::audio::input_device_list();
    let default_device = device_list.default_name;
    let devices = device_list
        .devices
        .into_iter()
        .enumerate()
        .map(|(index, device)| {
            json!({
                "value": index.to_string(),
                "label": device.name,
                "isDefault": device.is_default,
            })
        })
        .collect::<Vec<_>>();

    let words = shared
        .word_entries()
        .into_iter()
        .map(|(target, reading)| json!({ "target": target, "reading": reading }))
        .collect::<Vec<_>>();

    let model_source = if config.model_base_url.trim().is_empty() {
        models::OFFICIAL_RELEASE.to_string()
    } else {
        config::redact_source(&config.model_base_url)
    };
    let paths = std::env::current_dir()
        .map(|dir| {
            json!({
                "words": dir.join(text::WORDS_PATH).display().to_string(),
                "config": dir.join(config::CONFIG_PATH).display().to_string(),
                "models": config::models_root().display().to_string(),
                "modelSource": &model_source,
            })
        })
        .unwrap_or_else(|_| json!({ "modelSource": &model_source }));

    json!({
        "settings": settings_for_ui(settings),
        "engine": {
            "ready": engine_ready,
            "building": engine_building,
            "error": engine_error,
            "model": config.model,
            "hotwords": config.has_hotwords_file(),
        },
        "models": model_list,
        "stats": stats_for_ui(&shared.stats()),
        "defaultDevice": default_device,
        "devices": devices,
        "words": words,
        "level": level,
        "visual": match crate::state::VisualState::from_u8(
            shared.visual_state().load(Ordering::Relaxed)
        ) {
            crate::state::VisualState::Idle => "idle",
            crate::state::VisualState::Recording => "recording",
            crate::state::VisualState::Processing => "processing",
            crate::state::VisualState::Pasting => "pasting",
        },
        "captureError": app.capture_error(),
        "autostart": settings.autostart,
        "paths": paths,
        "version": env!("CARGO_PKG_VERSION"),
        "notice": notice.as_ref().map(|notice| json!({
            "id": notice.id,
            "kind": notice.kind,
            "message": notice.message,
        })),
    })
}

/// 累積実績の UI DTO。
///
/// 本文・音声・日付は渡さず、表示に必要な数値だけを含める。
fn stats_for_ui(stats: &Stats) -> Value {
    json!({
        "totalChars": stats.total_chars,
        "totalSessions": stats.total_sessions,
        "todayChars": stats.today_chars,
        "estimatedSavedMinutes": stats.estimated_saved_minutes(),
    })
}

/// 設定画面へ渡す設定 DTO。
///
/// `model_base_url` は認証情報を含み得るため、レンダラーへは渡さない。
/// 画面からは編集できない値なので、保存時に現在の実行時設定を維持する。
fn settings_for_ui(settings: &Settings) -> Value {
    json!({
        "first_run": settings.first_run,
        "model": &settings.model,
        "hotkey": &settings.hotkey,
        "autostart": settings.autostart,
        "device": &settings.device,
        "language": &settings.language,
        "num_threads": settings.num_threads,
        "sound_start": settings.sound_start,
        "sound_finish": settings.sound_finish,
        "sound_volume": settings.sound_volume,
        "sound_theme": &settings.sound_theme,
        "floating_bar": settings.floating_bar,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_only_embedded_pages() {
        let embedded = embedded_page_url(include_str!("web/settings.html"));
        for url in [
            "",
            "about:blank",
            "about:blank#models",
            embedded.as_str(),
            &format!("{embedded}#models"),
        ] {
            assert!(is_builtin_page(url, &embedded), "{url} を拒否した");
        }
        for url in [
            "about:config",
            "data:text/html,<script>alert(1)</script>",
            "https://example.com/",
            "file:///C:/Windows/System32/calc.exe",
            embedded_page_url("not the settings page").as_str(),
        ] {
            assert!(!is_builtin_page(url, &embedded), "{url} を許可した");
        }
    }

    #[test]
    fn embedded_url_matches_navigatetostring_data_url() {
        // WebView2 は NavigateToString を base64 の data URL として
        // ナビゲーションする。設定画面とオンボーディングの両方で、
        // その URL が組み込みページとして許可されること。
        for html in [
            include_str!("web/settings.html"),
            include_str!("web/onboarding.html"),
        ] {
            let embedded = embedded_page_url(html);
            assert!(
                embedded.starts_with("data:text/html;charset=utf-8;base64,"),
                "{}",
                &embedded[..40]
            );
            assert!(is_builtin_page(&embedded, &embedded));
            let altered = embedded_page_url(&format!("{html} "));
            assert!(!is_builtin_page(&altered, &embedded));
        }
    }

    #[test]
    fn stats_dto_exposes_only_numeric_metrics() {
        let stats = Stats {
            total_chars: 12_340,
            total_sessions: 7,
            today_chars: 120,
            today_date: "2026-10-25".to_string(),
        };
        let dto = stats_for_ui(&stats);
        assert_eq!(dto["totalChars"], 12_340);
        assert_eq!(dto["totalSessions"], 7);
        assert_eq!(dto["todayChars"], 120);
        let minutes = dto["estimatedSavedMinutes"].as_f64().expect("数値");
        assert!((minutes - 61.7).abs() < 0.01, "{minutes}");
        // 本文・日付などの追加情報を WebView へ渡していない。
        let object = dto.as_object().expect("オブジェクト");
        assert_eq!(object.len(), 4);
        assert!(object.values().all(serde_json::Value::is_number));
    }

    #[test]
    fn settings_dto_roundtrips_without_losing_values() {
        let original = Settings {
            first_run: false,
            model: "whisper-small".to_string(),
            hotkey: "ctrl_r".to_string(),
            autostart: true,
            device: Some("3".to_string()),
            language: "ja".to_string(),
            num_threads: 4,
            sound_start: false,
            sound_finish: true,
            sound_volume: 0.39,
            sound_theme: "chime".to_string(),
            floating_bar: false,
            model_base_url: "https://models.example/?sig=secret".to_string(),
        };
        let dto = settings_for_ui(&original);
        let text = dto.to_string();
        assert!(!text.contains("sig=secret"), "{text}");
        assert!(!text.contains("model_base_url"), "{text}");
        let mut parsed: Settings = serde_json::from_value(dto).expect("DTO を復元できる");
        assert_eq!(parsed.model_base_url, "", "DTO に配布元は含まれない");
        // 画面からは編集しない値なので、保存時に現在値を維持する。
        parsed.model_base_url.clone_from(&original.model_base_url);
        assert_eq!(parsed, original);
    }
}
