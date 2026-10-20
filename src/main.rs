//! Windows 向け Push-to-Talk 音声入力 CLI。
//!
//! 右 Alt を押している間だけ録音し、離した瞬間に SenseVoice で推論して
//! アクティブウィンドウへ Ctrl+V で貼り付ける。完全ローカル。

mod app;
mod audio;
mod config;
mod engine;
mod eval;
mod logging;
mod state;
mod text;
mod win;

use std::path::Path;

use config::Config;

/// エントリポイント。CLI を解析し、エンジン → マイク → フックの順に起動する。
fn main() {
    let cli = match config::parse_args(std::env::args().skip(1)) {
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
    logging::init(cli.debug);

    if cli.sweep && cli.eval.is_none() {
        eprintln!("--sweep は --eval <JSON> と一緒に指定してください");
        std::process::exit(2);
    }

    let mut config = Config::default();
    config.apply_cli(&cli);

    if let Some(path) = &cli.eval {
        std::process::exit(eval::run(path, cli.sweep, &config));
    }

    if cli.list_devices {
        std::process::exit(audio::list_input_devices());
    }

    info!(
        "プラットフォーム: Windows / ホットキー={} / 貼り付け={}+{}",
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

    let engine = match engine::Engine::build(&config) {
        Ok(engine) => engine,
        Err(exc) => {
            error!("{exc}");
            std::process::exit(1);
        }
    };
    let hotwords_supported = engine.supports_hotwords();

    let capture = match audio::AudioCapture::start(&config) {
        Ok(capture) => capture,
        Err(exc) => {
            error!("{exc}");
            std::process::exit(1);
        }
    };

    let app = match app::App::new(
        config,
        engine,
        capture.ring(),
        capture.xruns(),
        hotwords_supported,
    ) {
        Ok(app) => app,
        Err(exc) => {
            error!("{exc}");
            capture.stop();
            std::process::exit(1);
        }
    };

    let code = app.run();
    capture.stop();
    std::process::exit(code);
}
