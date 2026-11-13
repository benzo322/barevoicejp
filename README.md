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
- **6種類のモデル** — 初回起動のオンボーディングまたは設定画面から、sherpa-onnx 公式リリース (GitHub) / Hugging Face より直接ダウンロード。社内サーバー / 共有フォルダの配布元へも切り替え可能 (アプリ本体には同梱しない)
- **日本語向けテキスト整形** — タグ除去、空白正規化、辞書置換、英日境界のスペース補正、`SenseVoice` の ITN
- **辞書のホットリロード** — `words.txt` / `replacements.json` は保存すると次回の発話から反映。`hotwords.txt` は監視のみ (現行エンジンは未対応)
- **常駐UI** — トレイ常駐、設定画面 (操作 / モデル / マイク / 辞書 / 履歴 / 通知音 / ガイド / 技術スタック)、初回オンボーディング
- **フローティングバー** — 録音レベルと状態を表示 (`tiny-skia` で描画)
- **通知音** — 4テーマ (soft / pop / chime / subtle)、音量調整可
- **クリップボード保護** — 貼り付け後に元のクリップボード内容を復元するオプション
- **累積実績カウンター** — 総入力文字数・貼り付け回数・当日分と、推定タイピング削減時間を設定画面に表示。本文は保存しない
- **認識履歴** — 確定したテキストを日時・モデル名とともに直近10件だけ `history.json` へ保存。設定画面からワンクリックでコピー・全件クリアできる
- **自動起動** — `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` に登録

## 動作環境

| 項目 | 要件 |
|---|---|
| OS | Windows 10 / 11 (x64) |
| ランタイム | WebView2 Runtime (Windows 11 は標準搭載、Windows 10 は Evergreen 版が必要) |
| ランタイム | Microsoft Visual C++ 2015-2022 再頒布可能パッケージ (x64)。多くの環境では導入済み |
| マイク | 入力デバイスが1つ以上 |
| ディスク | モデル用に 160 MB 〜 700 MB（Haas-ja 量子化版は約 480 MB） |

## インストール

[Releases](../../releases) の `BAREVoiceJP-vX.Y.Z-win64.zip` を任意のフォルダへ展開し、`BAREVoiceJP.exe` を実行する。管理者権限は不要。ZIP を展開したフォルダが、そのまま設定・辞書・モデル・ログの保存先になる。

1. ZIP を展開する
2. `BAREVoiceJP.exe` を起動し、オンボーディングで使用するモデルを選択してダウンロードする (進捗は設定画面に表示)
3. 右 Alt を押しながら話し、離すとテキストが貼り付く

SmartScreen の警告が出た場合は「詳細情報」→「実行」を選ぶ。詳しい手順と SHA256 の確認方法は、ZIP 同梱の `README.txt` を参照。

アンインストールは、設定画面で自動起動をオフにしたうえでアプリを終了し、展開したフォルダを削除する。

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

リリースビルドは Windows の GUI サブシステムでリンクされるため、起動時にコンソールウィンドウは開かない。ログは exe と同じディレクトリの `logs\app.log` に出力される (下記「ログ」参照)。

モデルは初回起動時に設定画面からダウンロードする。手動で配置する場合は `models/<モデル名>/` に展開する。

### インストーラ

[Inno Setup 6](https://jrsoftware.org/isinfo.php) をインストールし、リポジトリ直下で:

```powershell
iscc installer.iss
```

`Output\BAREVoiceJP_Setup.exe` が生成される。インストーラは `target\release\BAREVoiceJP.exe` と `*.dll`、辞書テンプレート (`words.txt` / `replacements.json`) を同梱する。辞書はアップグレード時に上書きしない (`onlyifdoesntexist`)。

### リリース

`.github/workflows/release.yml` が `v*` タグの push (または手動実行) で Windows x64 向けの配布 ZIP をビルドし、GitHub Release に添付する。処理内容は次のとおり。

- sherpa-onnx v1.13.8 の Windows x64 Release を取得し、SHA256 を検証して展開
- `cargo build --release --locked`
- `BAREVoiceJP/` に exe・DLL・辞書テンプレート・`LICENSE`・`README.txt`・`THIRD-PARTY-LICENSES.txt` を格納
- `BAREVoiceJP-<tag>-win64.zip` と `.sha256` を生成してリリースに添付

タグと `Cargo.toml` の `version` は一致している必要がある。モデル本体は同梱しない。

## 使い方

- **録音**: 右 Alt を押している間。離すと推論して `Ctrl+V` を送信
- **設定**: トレイアイコンを右クリック →「設定」
- **終了**: トレイメニューから終了

このビルドの貼り付けは `Ctrl+V` 固定。`--paste-modifier` に `ctrl` 以外を指定すると警告が出る。

### データの置き場所

モデル・辞書・設定・履歴・ログはすべて **exe と同じディレクトリ** (`models/` の親) に置かれる。自動起動時もカレントディレクトリに依存しない。

```
BAREVoiceJP.exe
config.toml          # 設定 (UI が自動生成・保存)
stats.json           # 累積実績 (文字数と発話回数のみ)
history.json         # 認識履歴 (直近10件、本文を含む)
words.txt            # 辞書 (正式表記 : 代表読み)
replacements.json    # 完全一致置換 (かな → 英字)
hotwords.txt         # ホットワード (現行エンジンは未対応)
models/
  sherpa-onnx-.../
logs/
  app.log            # 実行ログ (5 MiB でローテーション)
  app.log.1 … app.log.3
```

インストーラー版では `%LOCALAPPDATA%\Programs\BAREVoiceJP\logs\app.log` になる。

### ログ

リリースビルドは GUI サブシステムのためコンソールを持たない。`INFO` / `WARNING` / `ERROR` とパニック内容は `logs\app.log` に常時出力される。`--debug` 指定時は `DEBUG` も記録する。

- 1 ファイル 5 MiB、`app.log.1`〜`app.log.3` の 3 世代を保持
- コンソールから起動した場合は stderr にも同じ内容を出力
- 書き込み失敗やログ出力自体の失敗でアプリは停止しない

**認識結果や音声データそのものはログへ出力しない。** `--debug` でも音声の内容は残らず、認識結果は文字数のみを記録する。辞書・設定・モデルは exe と同じディレクトリに置かれ、音声はディスクへ保存しない。

## セキュリティとプライバシー

- 推論はローカルで完結し、音声・認識結果を外部サービスへ送信しない
- 音声はメモリ上のリングバッファだけに保持し、ファイルへ書き出さない
- `stats.json` に保存するのは文字数と貼り付け回数だけで、累積実績に認識結果の本文は保存しない
- 認識結果の本文は `history.json` に直近10件だけ保存される。設定画面の「履歴」タブから全件クリアできる
- 社内配布元を使う場合は `checksums.sha256` と照合してから展開・配置する。公式配布元直取得は HTTPS と Content-Length で検証し、Haas-ja は組み込みの SHA-256 でも検証する
- リモートの配布元は HTTPS を必須とし、HTTPS → HTTP のリダイレクトも拒否する（平文 HTTP はループバック検証時のみ）
- アーカイブの受信・展開サイズに上限を設け、圧縮爆弾によるディスク枯渇を防ぐ
- 設定画面の WebView は埋め込みページ以外への遷移、新しいウィンドウ、権限要求を拒否する
- 推論中にフォーカスが別のウィンドウへ移った場合は `Ctrl+V` を送らず、認識結果をクリップボードに残す
- 自動起動は `HKCU` の `Run` のみを使用し、管理者権限を要求しない
- 貼り付け後に元のクリップボードへ戻すには `--restore-clipboard`（既定 OFF）を有効にする
- `model_base_url` に認証情報を含める場合は、`config.toml` に平文で残る点に注意する（ログと設定画面では伏せる）
- 旧バージョンの `logs/app.log` には認識結果が残っている可能性があるため、展開前に既存ログを削除する

## モデル

| ID | 名称 | サイズ | 速度 | 精度 | 備考 |
|---|---|---|---|---|---|
| `sensevoice` | SenseVoice | 約 155 MB | 5 | 4 | 日英中韓広東語。既定・推奨 |
| `reazonspeech` | ReazonSpeech | 約 680 MB | 4 | 5 | 日本語特化。長い発話に強い |
| `haas-ja-v1.0` | Haas-ja-v1.0 | 約 480 MB | 4 | 5 | 日本語特化。int8量子化で軽量・省メモリ |
| `whisper-base` | Whisper Base | 約 198 MB | 5 | 3 | 軽量・短いメモ向け |
| `whisper-small` | Whisper Small | 約 610 MB | 3 | 4 | 速度と精度のバランス |
| `whisper-turbo` | Whisper Turbo | 約 538 MB | 2 | 5 | 高精度。マシンパワーが必要 |

モデル本体はアプリに同梱せず、初回起動後に設定画面の「モデル」タブからダウンロードする。既定では sherpa-onnx 公式リリース (GitHub) から直接取得し、`haas-ja-v1.0` のみ Hugging Face (`oboroge0/Haas-ja-v1.0-onnx-quantized`) から取得する。`config.toml` の `model_base_url` を設定すると、社内サーバー（HTTPS または UNC 共有）からの配布に切り替えられる。Whisper は日本語 (`ja`) 固定、SenseVoice は `--language` で切り替えられる。

`haas-ja-v1.0` は int8/4bit 量子化版で、`encoder.quantized.onnx` と外部データ `encoder.data` を同じディレクトリに置く必要がある（設定画面からのダウンロードでは自動的に揃う）。旧 fp32 版の `encoder.onnx` が既にある場合もそのまま起動できるが、軽量化するにはモデルを削除して再ダウンロードする。

### 配布元の切り替え（社内サーバー / 共有フォルダ）

既定の公式配布元直取得をやめ、社内サーバーや共有フォルダから配布する場合の設定。`model_base_url` を空に戻せば公式配布元へ戻る。

#### 設定ファイルとキー

設定ファイルは **exe と同じディレクトリの `config.toml`**。インストーラー版では次の場所になる。

```
%LOCALAPPDATA%\Programs\BAREVoiceJP\config.toml
```

キーは **`model_base_url`** 1つ。空なら公式配布元 (sherpa-onnx GitHub / Hugging Face) から直接ダウンロードし、値を入れると社内配布元を使う。`config.toml` を開き、次の行を追加または変更する（アプリ再起動後に反映）。

```toml
# HTTPS の社内 Web サーバー（末尾スラッシュは有無どちらでも可）
model_base_url = "https://barevoice.example.co.jp/models"
```

```toml
# UNC ファイル共有（TOML のリテラル文字列 '' を使うと \ をそのまま書ける）
model_base_url = '\\fileserver\share\BAREVoiceJP\models'
```

```toml
# ローカル/ネットワークドライブの絶対パス
model_base_url = 'D:\BAREVoiceJP\models'
```

| 指定 | 例 | 動作 |
|---|---|---|
| HTTPS | `https://host/barevoice/models` | `ureq` で取得。接続10秒・応答ヘッダ30秒・本文2時間でタイムアウト |
| HTTP (ループバックのみ) | `http://127.0.0.1:8765` | ローカル検証用。`127.0.0.1` / `localhost` / `[::1]` 以外の平文 HTTP は拒否される |
| UNC | `\\fileserver\share\BAREVoiceJP\models` | 共有フォルダからストリームコピー |
| 絶対パス | `D:\BAREVoiceJP\models` | ローカルフォルダからストリームコピー |
| `file://` | `file:///C:/BAREVoiceJP/models` | 上記の絶対パスとして扱う |

`model_base_url` が空の場合は、設定画面の「モデル」タブに公式配布元 (sherpa-onnx GitHub / Hugging Face) の説明が表示され、そこから直接ダウンロードする。社内配布元を指定した場合は、その URL / パスが表示される。

#### サーバー側の配置

ベース URL/パスの直下に、**アーカイブ本体または ONNX ファイル一式と `checksums.sha256`** を置く。

```
<model_base_url>/
  checksums.sha256
  sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2
  sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01.tar.bz2
  sherpa-onnx-whisper-base.tar.bz2
  sherpa-onnx-whisper-small.tar.bz2
  sherpa-onnx-whisper-turbo.tar.bz2
  encoder.quantized.onnx
  encoder.data
  decoder.onnx
  joiner.onnx
  tokens.txt
```

`encoder.quantized.onnx` 〜 `tokens.txt` は `haas-ja-v1.0` の量子化版用。`encoder.quantized.onnx` と `encoder.data` は必ず同じディレクトリに置く。

`checksums.sha256` は `sha256sum` 互換形式（`<64桁の16進>␠␠<ファイル名>`）。`#` 行と `*` 付きファイル名、`models/foo.tar.bz2` のようなパス表記も受け付ける。

```text
# BAREVoiceJP models
7d1efa2138a65b0b488df37f8b89e3d91a60676e416f515b952358d83dfd347e  sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2
911b2083efd7c0dca2ac3b358b75222660dc09fb716d64fbfc417ba6c99ff3de  sherpa-onnx-whisper-base.tar.bz2
aba291d61d3d594bc6fb8c7f90acbd980e7bc7bf6b7caaf3965ddc25e6f42fac  encoder.quantized.onnx
30befbf6aeb75383e6551f5e58de53efdb76972ddc7d0f76070c517899873586  encoder.data
2947cb5512699cbba753fe45fe221428aadaa9856597debbb7aabe19cc253622  decoder.onnx
8a68320e750890ff9e944a794b41ae1c42c6f439ac4c273503458a6b42adc12d  joiner.onnx
ec78b13c5d9f156cf0d5fdbd6b5aec5785680f319c92630c11d5d11b14b83a84  tokens.txt
```

生成例（PowerShell）:

```powershell
Get-ChildItem .\*.tar.bz2 | Get-FileHash -Algorithm SHA256 |
  ForEach-Object { "$($_.Hash.ToLower())  $([IO.Path]::GetFileName($_.Path))" } |
  Set-Content -Encoding ascii checksums.sha256
Get-ChildItem .\encoder.quantized.onnx, .\encoder.data, .\decoder.onnx, .\joiner.onnx, .\tokens.txt |
  Get-FileHash -Algorithm SHA256 |
  ForEach-Object { "$($_.Hash.ToLower())  $([IO.Path]::GetFileName($_.Path))" } |
  Add-Content -Encoding ascii checksums.sha256
```

生成例（Linux / WSL）:

```bash
sha256sum *.tar.bz2 encoder.quantized.onnx encoder.data decoder.onnx joiner.onnx tokens.txt > checksums.sha256
```

**注意**: ハッシュは実際に配布するアーカイブのバイト列に対して計算する。再圧縮・再パッケージした場合は必ず再生成する。ハッシュは HTTPS または共有 ACL で保護された経路で取得する前提（マニフェスト自体の改ざん対策は経路の信頼に依存する）。

#### 検証とエラー表示

`model_base_url` を設定した場合の流れ:

1. マニフェストを取得し、対象ファイルの期待ハッシュを取得
2. `.part` へダウンロード/コピー（進捗を UI へ通知）
3. **ローカルの `.part` ファイルを SHA-256 で検証**
4. 一致した場合だけファイルを確定し、アーカイブは `models/` へ展開、直接配置モデルは `models/Haas-ja-v1.0-onnx/` へ移動
5. 不一致なら `.part` を削除し、モデルカードに  
   `SHA-256 が一致しません。ダウンロードが不完全です。再ダウンロードしてください。` を表示

進捗は `ダウンロード中 → 検証中 → 展開中 → DL済み` の順で表示される。公式リリースの tar.bz2 直取得ではハッシュがないため `検証中` を省き、`ダウンロード中 → 展開中 → DL済み` となる。Haas-ja は公式配布元から取得する場合も組み込みの SHA-256 で検証する。接続失敗・タイムアウト時は `モデル配布元に接続できません。ネットワーク接続を確認してください: <URL>` を表示する。共有パスの場合は `社内サーバーの共有パスに接続できません` / `ファイルがありません` / `共有権限を確認してください` を表示する。

#### ローカルモックでの確認

PowerShell の例（SenseVoice の実アーカイブをローカル HTTP で配信する）:

```powershell
# 1. 配布用フォルダを作る
New-Item -ItemType Directory -Force .\mockmodels | Out-Null
Copy-Item .\models\sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2 .\mockmodels\
Get-ChildItem .\mockmodels\*.tar.bz2 | Get-FileHash -Algorithm SHA256 |
  ForEach-Object { "$($_.Hash.ToLower())  $([IO.Path]::GetFileName($_.Path))" } |
  Set-Content -Encoding ascii .\mockmodels\checksums.sha256

# 2. モックサーバーを起動 (別ウィンドウ)
py -m http.server 8765 --directory .\mockmodels
```

`config.toml` に以下を追加してアプリを再起動する。

```toml
model_base_url = "http://127.0.0.1:8765"
```

設定画面 →「モデル」で対象モデルを未インストール状態にし、「ダウンロード」を押す。  
`ダウンロード中` → `SHA-256 を検証しています` → `アーカイブを展開しています` → `DL済み` と進めば正常。

異常系:

- モックサーバーを停止して別モデルをダウンロード → `モデル配布元に接続できません…`
- `checksums.sha256` のハッシュを1文字変える → `SHA-256 が一致しません…再ダウンロードしてください`
- `checksums.sha256` から対象行を削除 → `… の SHA-256 がありません`
- UNC の確認: 共有フォルダに同じ2ファイルを置き、`model_base_url = '\\<host>\<share>\mockmodels'` に変更

ユニットテストは以下で確認できる。

```powershell
cargo test models::tests
cargo test
```

`models::tests` は HTTP モックサーバー、UNC/絶対パス、マニフェスト解析、SHA-256 一致/不一致、ダウンロード→検証→展開→完了までをカバーする。

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

1行1語。ただし sherpa-onnx がホットワードを受け付けるのは `modified_beam_search` 指定時のみで、本アプリの SenseVoice は `greedy_search` 固定、他エンジンもホットワードを渡していない。そのため現在は内容が認識に反映されることはなく、`--hotwords-score` も受理するだけで未使用。将来ホットワード対応エンジンを追加する場合の入力ファイルとして残している。

## 設定ファイル (`config.toml`)

| キー | 既定値 | 内容 |
|---|---|---|
| `first_run` | `true` | オンボーディングを表示するか |
| `model` | `sensevoice` | 使用モデル ID |
| `model_base_url` | `""` | モデル配布元。空なら公式配布元 (sherpa-onnx GitHub / Hugging Face) から直接ダウンロード。`https://...` / UNC / 絶対パスを指定すると社内配布元。平文 HTTP はループバックのみ |
| `hotkey` | `alt_r` | 押している間だけ録音するキー |
| `autostart` | `false` | Windows ログオン時に自動起動 |
| `device` | (なし) | 入力デバイスの番号または名前の一部 |
| `language` | `ja` | SenseVoice の認識言語 (`ja` / `en` / `zh` / `ko` / `yue`) |
| `num_threads` | `2` | CPU 推論スレッド数 (1〜16) |
| `sound_start` / `sound_finish` | `true` | 録音開始・終了の通知音 |
| `sound_volume` | `0.55` | 通知音の音量 (0.0〜1.0) |
| `sound_theme` | `soft` | `soft` / `pop` / `chime` / `subtle` |
| `floating_bar` | `true` | フローティングバーを表示 |

設定画面での変更は即座に保存・反映される。CLI 引数は `config.toml` の値をさらに上書きする (保存はしない)。`model_base_url` は設定画面の項目ではなく、`config.toml` を直接編集する（変更後はアプリを再起動する）。

## 累積実績 (`stats.json`)

設定画面のステータスカードに、音声入力の累積実績を表示する。`config.toml` と同じディレクトリの `stats.json` に、次の数値だけを保存する。

| キー | 内容 |
|---|---|
| `total_chars` | アプリ導入からの総入力文字数 (Unicode スカラー値単位) |
| `total_sessions` | 貼り付けに成功した発話の総回数 |
| `today_chars` | 当日の入力文字数。ローカル日付の変更時に 0 へ戻る |
| `today_date` | `today_chars` が属するローカル日付 (`YYYY-MM-DD`) |

- 累積実績には認識結果の本文・音声を保存しない（本文は別途 `history.json` に直近10件だけ保存）
- 加算は `Ctrl+V` の送信まで成功した時点。貼り付け先が変わって送信しなかった場合は数えない
- 書き込みは同一ディレクトリの `stats.json.tmp` へ書き切って `sync_all` した後に置き換えるため、途中でクラッシュしても既存ファイルは壊れない
- 推定削減時間は 200 文字/分で換算する (`total_chars / 200` 分)。設定画面では 60 分以上を「約 X.X 時間」で表示する
- ファイルを削除すると 0 から再集計される。壊れた JSON は警告をログへ出し、0 から再開する

## 認識履歴 (`history.json`)

確定した認識テキストを、ローカル日時・使用モデル名とともに直近10件だけ保存する。保存先は `config.toml` と同じディレクトリの `history.json`。

- 設定画面の「履歴」タブに新しい順で表示される
- 各行のコピーボタンで、そのテキストをクリップボードへ送る
- 「全件クリア」で `history.json` ごと空にする
- 書き込みは `history.json.tmp` へ書き切って `sync_all` した後に置き換えるため、途中でクラッシュしても既存の履歴は壊れない
- 履歴ファイルが壊れている場合は警告をログへ出し、空の履歴で起動する

## コマンドライン

```
BAREVoiceJP [オプション]
```

| オプション | 既定値 | 内容 |
|---|---|---|
| `--model NAME` | `sensevoice` | `sensevoice` / `reazonspeech` / `haas-ja-v1.0` / `whisper-base` / `whisper-small` / `whisper-turbo` |
| `--model-dir DIR` | `models/<name>` | モデルのディレクトリ |
| `--hotkey KEY` | `alt_r` | 録音キー |
| `--device SPEC` | 既定デバイス | 入力デバイス番号または名前の一部 |
| `--language LANG` | `ja` | SenseVoice の言語 (`auto` も指定可) |
| `--num-threads N` | `2` | CPU スレッド数 |
| `--provider NAME` | `cpu` | ONNX Runtime execution provider |
| `--hotwords-score F` | `2.0` | ホットワードスコア (現行エンジンは未対応) |
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
| `--debug` | — | デバッグログを出力 (`logs/app.log` にも記録) |
| `--console` | — | デバッグビルドでコンソールを表示したまま起動 (リリースは常に非表示) |

## テストと評価

```powershell
cargo test
```

ユニットテストはテキスト整形・設定・音声・状態管理・累積実績・認識履歴の保存などをカバーする。

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
  models.rs     モデル定義とダウンロード (tar.bz2 展開 / 直接ファイル配置)
  audio.rs      録音 (cpal)、リングバッファ、無音判定
  text.rs       テキスト整形と辞書 (words / replacements / hotwords)
  app.rs        録音 → 推論 → 貼り付けのパイプライン
  state.rs      状態管理
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
tools/make_icon.py      アイコン生成スクリプト
voice_input.py          旧 Python プロトタイプ (実行時には不要)
docs/release/README.txt                 配布 ZIP 同梱の利用者向け README
docs/release/THIRD-PARTY-LICENSES.txt   第三者ライセンス表記
.github/workflows/release.yml           Windows リリース自動化
LICENSE                                  MIT License
```

`voice_input.py` は Rust 版以前のクロスプラットフォーム・プロトタイプで、整形ロジックの移植元。Windows アプリのビルドや実行には使わない。

## 既知の制限

- Windows x64 専用 (Win32 API と sherpa-onnx の DLL に依存)
- 貼り付けは `Ctrl+V` 固定
- モデル本体はリポジトリにも配布 ZIP にも含まれない (初回起動時に取得)

## ライセンス

MIT License。全文は [LICENSE](LICENSE) を参照。

同梱・依存する外部コンポーネントにはそれぞれのライセンスが適用される。配布 ZIP には [THIRD-PARTY-LICENSES.txt](docs/release/THIRD-PARTY-LICENSES.txt) を同梱している。sherpa-onnx (Apache-2.0)、ONNX Runtime (MIT)、WebView2 Runtime (Microsoft)、Rust クレート、および各 ASR モデル。モデル本体は同梱していないため、再配布する場合は各モデルのライセンスを確認すること。
