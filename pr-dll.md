## 症状

Windows 11 25H2 (build 26200) で、`BAREVoiceJP.exe` が起動直後にアクセス違反 (0xc0000005) で落ちる。

```
Faulting module: sherpa-onnx-c-api.dll
Exception code:  0xc0000005
Fault offset:    0x000000000006b3a1
```

ログは `SenseVoice モデル読み込み:` の行で途切れ、`モデル準備完了` が出ない。

## 原因

`C:\Windows\System32\onnxruntime.dll`（OS 同梱の 1.17.1）が、exe と同じディレクトリの `onnxruntime.dll`（1.28.2）より先に解決されていた。

sherpa-onnx 1.13.8 は ORT API v28 を要求するが、1.17 は v17 までしか提供しない。そのため `OrtGetApiBase()->GetApi(28)` が NULL を返し、直後の推論初期化でアクセス違反になる。

**アプリ固有の問題ではない** ことを以下の 3 点で確認した。

- 上流の `sherpa-onnx-offline.exe` を同じフォルダで実行しても同じ症状（`only API versions [1, 17] are supported. Current ORT Version is: 1.17.1`）
- `onnxruntime.dll` を退避させても同じ → System32 のを掴んでいる
- 何もインポートしない検証プロセスでも `LoadLibraryW("onnxruntime.dll")` が `C:\WINDOWS\SYSTEM32\onnxruntime.dll` を返す

## 修正

インポートライブラリのリンクをやめ、**実行時に ORT → sherpa の順で明示的にロード** する。

- `src/dll.rs` を追加 — ロード順の制御、シンボルの実行時解決、起動時の診断ログ
- `build.rs` — `cargo:rustc-link-lib=sherpa-onnx-c-api` を削除。これを残すと起動時点で束縛され、順序を制御できない
- `src/engine.rs` — `extern "C"` の宣言を削除し、`dll::api()` 経由の呼び出しへ置換
- `src/main.rs` — エンジン初期化の前に `dll::preload()` を呼ぶ

ロード順を守るだけでなく、**掴んだ ORT が v28 を提供しているかを起動時に検証** する。満たさない場合は、原因不明のアクセス違反ではなく読めるエラーで止まる。

```
[WARNING] 同梱の onnxruntime.dll が API v28 を提供していません (version=1.17.1)。
古い onnxruntime.dll が優先して読み込まれている可能性があります。
```

## 検証

修正前は 5 回連続でクラッシュ、修正後は 5 回連続で起動成功。

```
[INFO] onnxruntime 1.28.2 / api28=true
[INFO] SenseVoice モデル読み込み: ...\model.int8.onnx
[INFO] モデル準備完了 (SenseVoice / ITN=on, language=ja, threads=2, provider=cpu)
[INFO] 準備完了: ホットキー=alt_r / キャンセル=Esc / トレイから設定を開けます
```

プロセスがロードしているモジュールも `app\onnxruntime.dll`（同梱版）に変わったことを確認済み。

`cargo test --release`: 85 tests / 80 passed / 1 failed / 4 ignored

失敗 1 件は `win::tests::writes_clipboard_with_owner_window`。この環境では Chrome がクリップボードを掴んでおり、`Set-Clipboard` を含む他のどのプロセスも開けない状態（`OpenClipboard` が `error=5`）。テスト本体ではなく環境要因で、`src/win.rs` は今回変更していない。

## 回帰テスト

`src/dll.rs` に 2 本追加。

- `bundled_onnxruntime_supports_api_v28` — 同梱 ORT が v28 を提供しているか
- `loads_dlls_from_exe_directory` — バージョンが 1.17 系でないこと（System32 を掴んでいないか）

## ドキュメント

- `docs/development.md` — リンクしない理由と `src/dll.rs` の役割を追記
- `docs/release/README.txt` — 利用者向けのトラブルシュートを追加
