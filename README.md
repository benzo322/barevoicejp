# BAREVoiceJP

右 Alt を押している間だけ録音し、離した瞬間にローカルで文字起こしして、アクティブなウィンドウへ `Ctrl+V` で貼り付ける Windows 用 Push-to-Talk 音声入力。

推論はすべてローカル ([sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) / ONNX Runtime) で完結し、音声が外部へ送信されることはない。常駐してタスクトレイと設定画面 (WebView2) を提供する。

```
右 Alt 押下 ──▶ 録音 (押下直前 200ms も含める)
右 Alt 解放 ──▶ 無音判定・トリム ──▶ ASR ──▶ テキスト整形 ──▶ クリップボード ──▶ Ctrl+V
```

## 特徴

- **完全ローカル推論** — クラウドAPI・アカウント登録は不要
- **押して話すだけ** — ホットキーは変更可能 (既定 `alt_r`)。200ms 未満の押下は誤操作として破棄
- **5種類のモデル** — 初回起動のオンボーディングまたは設定画面からダウンロード (アプリ本体には同梱しない)
- **日本語向けテキスト整形** — タグ除去、空白正規化、辞書置換、英日境界のスペース補正、`SenseVoice` の ITN
- **辞書のホットリロード** — `words.txt` / `replacements.json` / `hotwords.txt` は保存すると次回の発話から反映
- **常駐UI** — トレイ常駐、設定画面 (操作 / モデル / マイク / 辞書 / 通知音 / ガイド / 技術スタック)、初回オンボーディング
- **フローティングバー** — 録音レベルと状態を表示 (`tiny-skia` で描画)
- **通知音** — 4テーマ (soft / pop / chime / subtle)、音量調整可
- **クリップボード保護** — 貼り付け後に元のクリップボード内容を復元するオプション
- **自動起動** — `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` に登録

## 動作環境

| 項目 | 要件 |
|---|---|
| OS | Windows 10 / 11 (x64) |
| ランタイム | WebView2 Runtime (Windows 11 は標準搭載、Windows 10 は Evergreen 版が必要) |
| マイク | 入力デバイスが1つ以上 |
| ディスク | モデル用に 160 MB 〜 700 MB |

## インストール

[Releases](../../releases) の `BAREVoiceJP_Setup.exe` を実行する。管理者権限は不要で、`%LOCALAPPDATA%\Programs\BAREVoiceJP` にインストールされる。

1. 初回起動でオンボーディングが表示される
2. 使用するモデルを選択してダウンロード (進捗は設定画面に表示)
3. 右 Alt を押しながら話し、離すとテキストが貼り付く

アンインストールは「アプリと機能」またはスタートメニューの `Uninstall BAREVoiceJP` から。

## ソースからビルド

### 前提

- Rust stable (**edition 2024** / Rust 1.85 以降)
- Visual Studio Build Tools (MSVC ツールチェーン)
- [sherpa-onnx v1.13.8 win-x64 shared MD Release](https://github.com/k2-fsa/sherpa-onnx/releases/tag/v1.13.8) を展開し、リポジトリ直下に `sherpa-onnx-v1.13.8-win-x64-shared-MD-Release/` として配置

`sherpa-onnx-v1.13.8-win-x64-shared-MD-Release/` と `models/` は `.gitignore` 対象。`build.rs` が以下を行う。

- `app.ico` を exe のリソースとして埋め込み
- `lib/sherpa-onnx-c-api.lib` をリンク
- `sherpa-onnx-c-api.dll` / `onnxruntime*.dll` を `target/<profile>/` と `deps/` へコピー

### ビルドと実行

```powershell
cargo build --release
.\target\release\BAREVoiceJP.exe
```

モデルは初回起動時に設定画面からダウンロードする。手動で配置する場合は `models/<モデル名>/` に展開する。

### インストーラ

[Inno Setup 6](https://jrsoftware.org/isinfo.php) をインストールし、リポジトリ直下で:

```powershell
iscc installer.iss
```

`Output\BAREVoiceJP_Setup.exe` が生成される。インストーラは `target\release\BAREVoiceJP.exe` と `*.dll`、辞書テンプレート (`words.txt` / `replacements.json`) を同梱する。辞書はアップグレード時に上書きしない (`onlyifdoesntexist`)。

## 使い方

- **録音**: 右 Alt を押している間。離すと推論して `Ctrl+V` を送信
- **設定**: トレイアイコンを右クリック →「設定」
- **終了**: トレイメニューから終了

このビルドの貼り付けは `Ctrl+V` 固定。`--paste-modifier` に `ctrl` 以外を指定すると警告が出る。

### データの置き場所

モデル・辞書・設定はすべて **exe と同じディレクトリ** (`models/` の親) に置かれる。自動起動時もカレントディレクトリに依存しない。

```
BAREVoiceJP.exe
config.toml          # 設定 (UI が自動生成・保存)
words.txt            # 辞書 (正式表記 : 代表読み)
replacements.json    # 完全一致置換 (かな → 英字)
hotwords.txt         # SenseVoice ホットワード (1行1語)
models/
  sherpa-onnx-.../
```

## モデル

| ID | 名称 | サイズ | 速度 | 精度 | 備考 |
|---|---|---|---|---|---|
| `sensevoice` | SenseVoice | 約 155 MB | 5 | 4 | 日英中韓広東語。既定・推奨 |
| `reazonspeech` | ReazonSpeech | 約 680 MB | 4 | 5 | 日本語特化。長い発話に強い |
| `whisper-base` | Whisper Base | 約 198 MB | 5 | 3 | 軽量・短いメモ向け |
| `whisper-small` | Whisper Small | 約 610 MB | 3 | 4 | 速度と精度のバランス |
| `whisper-turbo` | Whisper Turbo | 約 538 MB | 2 | 5 | 高精度。マシンパワーが必要 |

モデルは [sherpa-onnx の ASR モデル配布](https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models) から取得し、`models/` 直下へ展開する。Whisper は日本語 (`ja`) 固定、SenseVoice は `--language` で切り替えられる。

## 辞書のカスタマイズ

いずれも保存すれば次回の発話から反映される (mtime を監視して再読み込み)。

### `words.txt`

```
# 正式表記 : 代表読み
GitHub Actions : ギットハブアクションズ
Docker : ドッカー
```

- 正式表記と読みがどちらも4文字以上の項目はファジーマッチ対象 (`--fuzzy` で有効化、既定は完全一致)
- 読みが6文字以上の場合はしきい値が緩和される
- 助詞 (`で` `に` `を` `は` `が` `の` `と` `へ` `も`) の誤爆を避けるよう境界を判定する

### `replacements.json`

ASR が返す読みを完全一致で置換する。

```json
{
  "えーぴーあい": "API",
  "ぎっと": "git"
}
```

### `hotwords.txt`

1行1語。ホットワード対応モデル (SenseVoice) の認識を寄せる。スコアは `--hotwords-score` (既定 2.0)。

## 設定ファイル (`config.toml`)

| キー | 既定値 | 内容 |
|---|---|---|
| `first_run` | `true` | オンボーディングを表示するか |
| `model` | `sensevoice` | 使用モデル ID |
| `hotkey` | `alt_r` | 押している間だけ録音するキー |
| `autostart` | `false` | Windows ログオン時に自動起動 |
| `device` | (なし) | 入力デバイスの番号または名前の一部 |
| `language` | `ja` | SenseVoice の認識言語 (`ja` / `en` / `zh` / `ko` / `yue`) |
| `num_threads` | `2` | CPU 推論スレッド数 (1〜16) |
| `sound_start` / `sound_finish` | `true` | 録音開始・終了の通知音 |
| `sound_volume` | `0.55` | 通知音の音量 (0.0〜1.0) |
| `sound_theme` | `soft` | `soft` / `pop` / `chime` / `subtle` |
| `floating_bar` | `true` | フローティングバーを表示 |

設定画面での変更は即座に保存・反映される。CLI 引数は `config.toml` の値をさらに上書きする (保存はしない)。

## コマンドライン

```
BAREVoiceJP [オプション]
```

| オプション | 既定値 | 内容 |
|---|---|---|
| `--model NAME` | `sensevoice` | `sensevoice` / `reazonspeech` / `whisper-base` / `whisper-small` / `whisper-turbo` |
| `--model-dir DIR` | `models/<name>` | モデルのディレクトリ |
| `--hotkey KEY` | `alt_r` | 録音キー |
| `--device SPEC` | 既定デバイス | 入力デバイス番号または名前の一部 |
| `--language LANG` | `ja` | SenseVoice の言語 (`auto` も指定可) |
| `--num-threads N` | `2` | CPU スレッド数 |
| `--provider NAME` | `cpu` | ONNX Runtime execution provider |
| `--hotwords-score F` | `2.0` | ホットワードスコア |
| `--highpass` / `--no-highpass` | OFF | 80 Hz ハイパスフィルタ |
| `--denoise` / `--no-denoise` | OFF | RNNoise ノイズリダクション |
| `--pre-roll-ms MS` | `200` | 押下直前の音声を含める長さ |
| `--min-duration-ms MS` | `200` | これ未満の押下は破棄 |
| `--paste-delay-ms MS` | `50` | クリップボード設定から貼り付けまでの待機 |
| `--restore-clipboard` / `--no-restore-clipboard` | OFF | 貼り付け後に元のクリップボードを復元 |
| `--itn` / `--no-itn` | ON | SenseVoice の逆テキスト正規化 |
| `--fuzzy` | OFF | 辞書のあいまい一致を有効化 |
| `--list-devices` | — | 入力デバイス一覧を表示して終了 |
| `--eval FILE` | — | テキスト整形の評価ケースを実行して終了 |
| `--sweep` | — | `--eval` 時にファジーしきい値 70/75/80/85 を比較 |
| `--debug` | — | デバッグログを表示 |
| `--console` | — | コンソールを表示したまま起動 |

## テストと評価

```powershell
cargo test
```

ユニットテストはテキスト整形・設定・音声・状態管理などをカバーする。

テキスト整形だけを音声なしで評価できる (`tests/text_cases.json`)。

```powershell
cargo run --release -- --eval tests/text_cases.json
cargo run --release -- --eval tests/text_cases.json --sweep
```

カテゴリ別の正解率、失敗ケース、ファジーしきい値ごとの誤爆・表記揺れ救出数を出力する。`--eval` ではマイクと ASR は初期化されない。

## プロジェクト構成

```
src/
  main.rs       エントリポイント (CLI → 設定 → エンジン → マイク → UI)
  config.rs     CLI解析・設定・チューニング定数
  engine.rs     sherpa-onnx の FFI ラッパーとモデル解決
  models.rs     モデル定義とダウンロード (tar.bz2 展開)
  audio.rs      録音 (cpal)、リングバッファ、無音判定
  text.rs       テキスト整形と辞書 (words / replacements / hotwords)
  app.rs        録音 → 推論 → 貼り付けのパイプライン
  state.rs      状態管理
  win.rs        Win32 (ホットキー、SendInput、クリップボード、自動起動)
  overlay.rs    フローティングバー (tiny-skia)
  tray.rs       タスクトレイ
  ui.rs         WebView2 設定画面
  sound.rs      通知音の合成
  eval.rs       --eval / --sweep ハーネス
  logging.rs    ログ
  web/          設定画面・オンボーディング (HTML/CSS/JS)
tests/text_cases.json   テキスト整形の評価ケース
tools/make_icon.py      アイコン生成スクリプト
voice_input.py          旧 Python プロトタイプ (実行時には不要)
```

`voice_input.py` は Rust 版以前のクロスプラットフォーム・プロトタイプで、整形ロジックの移植元。Windows アプリのビルドや実行には使わない。

## 既知の制限

- Windows x64 専用 (Win32 API と sherpa-onnx の DLL に依存)
- 貼り付けは `Ctrl+V` 固定
- モデル本体はリポジトリにもインストーラにも含まれない (初回起動時に取得)

## ライセンス

現時点でライセンスファイルは置いていない (社内配布用)。

同梱・依存する外部コンポーネントにはそれぞれのライセンスが適用される: sherpa-onnx、ONNX Runtime、WebView2 Runtime、および各 ASR モデル。再配布する場合は各モデルのライセンスを確認すること。
