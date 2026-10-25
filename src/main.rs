// リリースビルドはコンソールを持たない GUI サブシステムとしてリンクし、
// 起動時の黒いウィンドウを出さない。ログは logs/app.log へ出力する。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Windows 向け Push-to-Talk 音声入力。
//!
//! 右 Alt を押している間だけ録音し、離した瞬間に SenseVoice で推論して
//! アクティブウィンドウへ Ctrl+V で貼り付ける。完全ローカル。
//! 常駐時はタスクトレイと設定画面 (WebView2) を提供する。

mod app;
mod audio;
mod config;
mod engine;
mod eval;
mod logging;
mod models;
mod overlay;
mod sound;
mod state;
mod stats;
mod text;
mod tray;
mod ui;
mod win;

use std::path::Path;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use config::{Config, Settings};

/// エントリポイント。CLI を解析し、エンジン → マイク → UI の順に起動する。
fn main() {
    let mut cli = match config::parse_args(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(exc) => {
            eprintln!("{exc}");
            std::process::exit(2);
        }
    };
    if cli.help {
        print!("{}", config::HELP);
        return;
    }
    cli.resolve_paths();
    logging::init(cli.debug);
    // リリースビルドは GUI サブシステムでコンソールを持たない。
    // デバッグビルドだけ従来どおり --console 指定時に表示する。
    if cfg!(debug_assertions) && !cli.console {
        win::hide_console();
    }
    config::use_data_root();

    if cli.sweep && cli.eval.is_none() {
        eprintln!("--sweep は --eval <JSON> と一緒に指定してください");
        logging::flush();
        std::process::exit(2);
    }

    let (settings, _existed) = Settings::load(Path::new(config::CONFIG_PATH));
    let mut config = Config::default();
    settings.apply_to(&mut config);
    config.apply_cli(&cli);
    if let Err(exc) = win::parse_hotkey(&config.hotkey) {
        warn!("設定ファイルのホットキーが不正です。alt_r を使います: {exc}");
        config.hotkey = "alt_r".to_string();
    }
    if config.model_base_url.trim().is_empty() {
        info!("モデル配布元: 未設定 (config.toml の model_base_url)");
    } else {
        info!(
            "モデル配布元: {}",
            config::redact_source(&config.model_base_url)
        );
    }

    if let Some(path) = &cli.eval {
        let code = eval::run(path, cli.sweep, &config);
        logging::flush();
        std::process::exit(code);
    }

    if cli.list_devices {
        let code = audio::list_input_devices();
        logging::flush();
        std::process::exit(code);
    }

    // 二重起動を防ぐ。2 個目が動くとキーボードフックとクリップボードを
    // 奪い合い、同じ発話が二重に貼り付いたり設定が競合したりする。
    let _instance = match win::SingleInstance::acquire() {
        Ok(instance) => instance,
        Err(exc) => {
            warn!("{exc}。二重起動を中止します");
            logging::flush();
            std::process::exit(0);
        }
    };

    info!(
        "BAREVoiceJP / プラットフォーム: Windows / ホットキー={} / 貼り付け={}+{}",
        config.hotkey, config.paste_modifier, config.paste_key
    );
    if config.paste_key != "v" || !config.paste_modifier.eq_ignore_ascii_case("ctrl") {
        warn!(
            "このビルドの貼り付けは Ctrl+V 固定です (指定: {}+{})",
            config.paste_modifier, config.paste_key
        );
    }

    // エンジン初期化より先にテンプレートを用意する (リファレンスと同じ順序)。
    text::ensure_hotwords_file(Path::new(text::HOTWORDS_PATH));
    text::ensure_words_file(Path::new(text::WORDS_PATH));

    let engine_result = engine::Engine::build(&config);
    let hotwords_supported = engine_result
        .as_ref()
        .map(|engine| engine.supports_hotwords())
        .unwrap_or(false);
    let (engine, engine_error) = match engine_result {
        Ok(engine) => (Some(engine), None),
        Err(exc) => {
            warn!(
                "推論エンジンを初期化できません。設定画面からモデルをダウンロードしてください。\n{exc}"
            );
            (None, Some(exc))
        }
    };

    let capacity =
        (config.sample_rate as f32 * config.ring_seconds).max(config::MIN_RING_SECONDS) as usize;
    let ring = Arc::new(Mutex::new(audio::RingBuffer::new(capacity)));
    let level = Arc::new(AtomicU32::new(0.0f32.to_bits()));
    let (capture, capture_error) =
        match audio::AudioCapture::start(&config, Arc::clone(&ring), Arc::clone(&level)) {
            Ok(capture) => (Some(capture), None),
            Err(exc) => {
                warn!("マイクを開けません。設定画面から選択し直せます。\n{exc}");
                (None, Some(exc))
            }
        };
    let xruns = capture
        .as_ref()
        .map(audio::AudioCapture::xruns)
        .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicU64::new(0)));

    let app = match app::App::new(
        config,
        engine,
        engine_error,
        ring,
        level,
        xruns,
        capture,
        capture_error,
        hotwords_supported,
    ) {
        Ok(app) => app,
        Err(exc) => {
            error!("{exc}");
            logging::flush();
            std::process::exit(1);
        }
    };

    app.run();
}
