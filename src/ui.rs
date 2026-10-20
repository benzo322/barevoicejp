//! 設定画面 (WebView2 / wry) とイベントループ。
//!
//! tao のイベントループがキーボードフックのメッセージポンプを兼ねる。
//! IPC は JSON 文字列で受け取り、状態は定期的に JS へ push する。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::WindowExtWindows;
use tao::window::{Window, WindowBuilder};
use wry::{WebView, WebViewBuilder};

use crate::app::App;
use crate::config::{self, MODEL_IDS, Settings};
use crate::models;
use crate::overlay::Overlay;
use crate::sound;
use crate::text;
use crate::tray;
use crate::win;

/// トレイ・ワーカー・IPC からメインスレッドへ送るイベント。
pub enum UiEvent {
    ShowSettings,
    ReloadDictionaries,
    Ipc(String),
    Tick,
    ModelsChanged,
    Quit,
}

struct Notice {
    id: u64,
    kind: &'static str,
    message: String,
}

/// 設定画面とトレイを起動し、終了までイベントループを回す。
pub fn run(mut app: App) -> ! {
    let mut settings = Settings::from_config(&app.shared().snapshot());
    let first_run = !Path::new(config::CONFIG_PATH).exists();

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
        .with_title("mkvoicejp — 設定")
        .with_inner_size(LogicalSize::new(1060.0, 760.0))
        .with_min_inner_size(LogicalSize::new(940.0, 660.0))
        .with_decorations(false)
        .with_resizable(true)
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
        app.level(),
        app.shared().visual_state(),
        Arc::clone(&enabled),
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

    let tick_proxy = proxy.clone();
    std::thread::Builder::new()
        .name("mkvoicejp-ui-tick".to_string())
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
    let mut first_shown = false;

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        if first_run && !first_shown && matches!(event, Event::NewEvents(_)) {
            first_shown = true;
            show_window(&window);
            push_state(&webview, &app, &settings, &notice);
            push_level(&webview, &app);
        }
        match event {
            Event::UserEvent(UiEvent::ShowSettings) => {
                show_window(&window);
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
                if window.is_visible() {
                    push_level(&webview, &app);
                    if last_state.elapsed() >= Duration::from_millis(500) {
                        last_state = Instant::now();
                        push_state(&webview, &app, &settings, &notice);
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                window.set_visible(false);
            }
            Event::LoopDestroyed => {
                app.shutdown();
                std::process::exit(0);
            }
            _ => {}
        }
    });
}

fn show_window(window: &Window) {
    window.set_visible(true);
    window.set_minimized(false);
    window.set_focus();
    win::force_foreground(window.hwnd() as *mut core::ffi::c_void);
}

fn set_notice(notice: &mut Option<Notice>, id: &mut u64, kind: &'static str, message: &str) {
    *id += 1;
    *notice = Some(Notice {
        id: *id,
        kind,
        message: message.to_string(),
    });
}

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
            sound::play(cue, settings.sound_volume);
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
    let mut applied = Settings::from_config(&config);

    app.apply_config(config.clone());
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
        applied = Settings::from_config(&config);
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

fn push_level(webview: &WebView, app: &App) {
    let level = f32::from_bits(app.level().load(Ordering::Relaxed));
    if let Err(exc) = webview.evaluate_script(&format!(
        "window.__mkvoice && window.__mkvoice.level({level:.4});"
    )) {
        crate::debug!("入力レベルを送れません: {exc}");
    }
}

fn push_state(webview: &WebView, app: &App, settings: &Settings, notice: &Option<Notice>) {
    let payload = snapshot(app, settings, notice);
    let text = payload
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    if let Err(exc) = webview.evaluate_script(&format!(
        "window.__mkvoice && window.__mkvoice.apply({text});"
    )) {
        crate::debug!("設定状態を送れません: {exc}");
    }
}

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

    let devices = crate::audio::input_device_names()
        .into_iter()
        .enumerate()
        .map(|(index, name)| json!({ "value": index.to_string(), "label": name }))
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
