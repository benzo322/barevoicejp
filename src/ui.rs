//! 設定画面 (WebView2 / wry) とイベントループ。
//!
//! tao のイベントループがキーボードフックのメッセージポンプを兼ねる。
//! IPC は JSON 文字列で受け取り、状態は定期的に JS へ push する。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::WindowExtWindows;
use tao::window::{Window, WindowBuilder};
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};
use wry::{WebView, WebViewBuilder};

use crate::app::App;
use crate::config::{self, MODEL_IDS, Settings, SoundTheme};
use crate::models;
use crate::overlay::Overlay;
use crate::sound;
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
    let script: &'static str = include_str!("web/settings.js");
    let webview = match WebViewBuilder::new()
        .with_html(include_str!("web/settings.html"))
        .with_initialization_script(script)
        .with_background_color((243, 247, 252, 255))
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |request| {
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
                show_window(&window, &webview);
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
/// 手動で開いたときは常に「操作」タブから始める。
fn show_window(window: &Window, webview: &WebView) {
    window.set_minimized(false);
    place_window(window);
    window.set_visible(true);
    window.set_focus();
    win::force_foreground(window.hwnd() as *mut core::ffi::c_void);
    if let Err(exc) = webview.evaluate_script(
        "window.__barevoice && window.__barevoice.openTab && window.__barevoice.openTab('control');",
    ) {
        crate::debug!("操作タブへ切り替えられません: {exc}");
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

/// プライマリディスプレイのワークエリア中央へオンボーディングを配置する。
///
/// 初回起動時はフローティングバーの位置に関係なく、画面の X / Y とも完全な中央に出す。
fn place_window_centered(window: &Window) {
    let Some(work) = work_area() else {
        return;
    };
    let size = window.outer_size();
    let x = work.left + ((work.right - work.left) - size.width as i32) / 2;
    let y = work.top + ((work.bottom - work.top) - size.height as i32) / 2;
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

    let webview = WebViewBuilder::new()
        .with_html(include_str!("web/onboarding.html"))
        .with_background_color((247, 250, 255, 255))
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |request| {
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
                models::Phase::Downloading | models::Phase::Extracting
            ) {
                return;
            }
            let proxy = proxy.clone();
            models::download(id, move || {
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
    let Ok(next) = serde_json::from_value::<Settings>(value) else {
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
    match std::process::Command::new("notepad.exe").arg(path).spawn() {
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

    let paths = std::env::current_dir()
        .map(|dir| {
            json!({
                "words": dir.join(text::WORDS_PATH).display().to_string(),
                "replacements": dir.join(text::REPLACEMENTS_PATH).display().to_string(),
                "config": dir.join(config::CONFIG_PATH).display().to_string(),
                "models": config::models_root().display().to_string(),
            })
        })
        .unwrap_or_else(|_| json!({}));

    json!({
        "settings": settings,
        "engine": {
            "ready": engine_ready,
            "building": engine_building,
            "error": engine_error,
            "model": config.model,
            "hotwords": config.hotwords_file().is_some(),
        },
        "models": model_list,
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
