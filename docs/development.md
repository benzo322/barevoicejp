# ビルドとリリース

## 前提

- Rust stable（edition 2024 / **Rust 1.88 以降**）。CI の MSRV ジョブも 1.88 で回している
- Visual Studio Build Tools（MSVC ツールチェーン）
- [sherpa-onnx v1.13.8 win-x64 shared MD Release](https://github.com/k2-fsa/sherpa-onnx/releases/tag/v1.13.8) を展開し、リポジトリ直下に `sherpa-onnx-v1.13.8-win-x64-shared-MD-Release/` として置く

`sherpa-onnx-v1.13.8-win-x64-shared-MD-Release/` と `models/` は `.gitignore` 済み。

`build.rs` の仕事は3つ。

- `app.ico` を exe のリソースとして埋め込む
- `sherpa-onnx-c-api.dll` / `onnxruntime*.dll` を `target/<profile>/` と `deps/` へコピーする
- `lib/sherpa-onnx-c-api.lib` の**存在だけ確認する**（リンクはしない）

> **インポートライブラリをリンクしない理由**
>
> リンクするとプロセス起動時点で `sherpa-onnx-c-api.dll` が静的に束縛され、その時点で `onnxruntime.dll` の名前解決が走る。Windows 11 25H2 以降、`C:\Windows\System32\onnxruntime.dll`（OS 同梱の 1.17 系）が exe 隣の DLL より優先される事例があり、1.17 を掴むと `OrtGetApiBase()->GetApi(28)` が NULL を返してモデル読み込み時にアクセス違反で落ちる。
>
> そのためシンボルは実行時に `src/dll.rs` が **ORT → sherpa** の順でロードして解決する。`src/dll.rs` の `bundled_onnxruntime_supports_api_v28` テストが、この退行を検出する。

## ビルド

```powershell
cargo build --release
.\target\release\BAREVoiceJP.exe
```

リリースビルドは GUI サブシステムでリンクするので、起動時にコンソールは開かない。ログは `%APPDATA%\BAREVoiceJP\logs\app.log` に出る。データの保存先を変えて開発する場合は、環境変数 `BAREVOICEJP_DATA_DIR` に任意のフォルダを指定する。

モデルは初回起動時に設定画面から落とす。手で置くなら `%APPDATA%\BAREVoiceJP\models\<モデル名>\` に展開する。

## テスト

```powershell
# Cargo 1.100 以降、テスト実行ファイルが target\debug\build\... に移った。
# build.rs が DLL を置く target\debug を PATH に足しておく。
$env:PATH = "$PWD\target\debug;$env:PATH"
cargo test
```

`#[ignore]` 付きのテストは、実モデルと sherpa-onnx DLL が要るので既定では走らない。

テキスト整形だけを音声なしで評価できる。

```powershell
cargo run --release -- --eval tests/text_cases.json
cargo run --release -- --eval tests/text_cases.json --sweep
```

カテゴリ別の正解率、外したケース、ファジーしきい値ごとの誤爆と表記揺れの救出数が出る。`--eval` ではマイクも ASR も初期化しない。

`models::tests` は HTTP モックサーバー、UNC / 絶対パス、マニフェスト解析、SHA-256 の一致と不一致、ダウンロード→検証→展開→完了までを通す。

```powershell
cargo test models::tests
```

## リリース

`.github/workflows/release.yml` が `v*` タグの push（または手動実行）で Windows x64 の ZIP を作り、GitHub Release に添付する。

1. sherpa-onnx v1.13.8 の Windows x64 Release を取り、SHA256 を検証して展開
2. `cargo build --release --locked`
3. `BAREVoiceJP/` に exe・DLL・辞書テンプレート・`LICENSE`・`README.txt`・`THIRD-PARTY-LICENSES.txt` を入れる
4. `BAREVoiceJP-<tag>-win64.zip` と `.sha256` を添付

タグと `Cargo.toml` の `version` は揃えること。モデル本体は入れない。

**セルフアップデートの条件:** アプリは `releases/latest` の `BAREVoiceJP-<tag>-win64.zip` と `BAREVoiceJP-<tag>-win64.zip.sha256` を探す。ZIP のルートは `BAREVoiceJP/` とし、`BAREVoiceJP.exe` と 3 DLL (`sherpa-onnx-c-api.dll` / `onnxruntime.dll` / `onnxruntime_providers_shared.dll`) を必ず含める。`.sha256` は `<64桁の16進>  <ファイル名>` 形式（`release.yml` の出力そのまま）。

## 構成

```
src/
  main.rs       エントリポイント (CLI → 設定 → エンジン → マイク → UI)
  config.rs     CLI 解析・設定・データ保存先 (%APPDATA%) と旧データ移行
  dll.rs        sherpa-onnx / onnxruntime のロード順制御とシンボル解決
  engine.rs     sherpa-onnx の FFI ラッパーとモデル解決
  models.rs     モデル定義とダウンロード (tar.bz2 展開 / 直接ファイル配置)
  update.rs     セルフアップデート (GitHub Releases / ZIP / 差し替え)
  audio.rs      録音 (cpal)、リングバッファ
  text.rs       テキスト整形と辞書 (words / replacements / hotwords)
  app.rs        録音 → 推論 → 貼り付けのパイプライン
  state.rs      状態管理 (長押しと2回押しの判定)
  stats.rs      累積実績 (stats.json のアトミック保存)
  history.rs    認識履歴 (history.json のアトミック保存、直近10件)
  win.rs        Win32 (ホットキー、SendInput、クリップボード、自動起動)
  overlay.rs    フローティングバー (tiny-skia)
  tray.rs       タスクトレイ
  ui.rs         WebView2 設定画面
  sound.rs      通知音の合成
  eval.rs       --eval / --sweep ハーネス
  logging.rs    ログ (stderr + logs/app.log の 5 MiB ローテーション)
  web/          設定画面・オンボーディング (HTML/CSS/JS)
tests/text_cases.json   テキスト整形の評価ケース
tools/make_icon.py      アイコン生成
docs/release/README.txt                 配布 ZIP 同梱の利用者向け README
docs/release/THIRD-PARTY-LICENSES.txt   第三者ライセンス表記
.github/workflows/ci.yml                Windows CI (fmt / clippy / test / MSRV / cargo audit)
.github/workflows/release.yml           Windows リリース自動化
```

## インストーラ

`installer.iss` は Inno Setup 6 用の定義だが、**現在のリリースフローには入っていない**（`release.yml` は ZIP しか作らない）。使う場合は Inno Setup 6 を入れてリポジトリ直下で:

```powershell
iscc installer.iss
```

`Output\BAREVoiceJP_Setup.exe` が出る。`target\release` の exe と DLL、辞書テンプレート、ライセンス3点を同梱し、辞書はアップグレード時に上書きしない（`onlyifdoesntexist`）。
