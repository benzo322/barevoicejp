# 設定ファイルとコマンドライン

## `config.toml`

`%APPDATA%\BAREVoiceJP\config.toml` に置く。設定画面での変更は即座に保存・反映される。CLI 引数は `config.toml` の値をさらに上書きする（保存はしない）。

| キー | 既定値 | 内容 |
|---|---|---|
| `first_run` | `true` | オンボーディングを表示するか |
| `model` | `sensevoice` | 使用モデル ID |
| `model_base_url` | `""` | モデル配布元。空なら公式（sherpa-onnx GitHub / Hugging Face）。`https://...` / UNC / 絶対パスで任意の配布元。平文 HTTP はループバックのみ |
| `hotkey` | `alt_r` | 押している間だけ録音するキー |
| `autostart` | `false` | Windows ログオン時に自動起動 |
| `device` | （なし） | 入力デバイスの番号、または名前の一部 |
| `language` | `ja` | SenseVoice の認識言語（`ja` / `en` / `zh` / `ko` / `yue`） |
| `num_threads` | `2` | CPU 推論スレッド数（1〜16） |
| `sound_start` / `sound_finish` | `true` | 録音開始・終了の通知音 |
| `sound_volume` | `0.55` | 通知音の音量（0.0〜1.0） |
| `sound_theme` | `soft` | `soft` / `pop` / `chime` / `subtle` |
| `floating_bar` | `true` | フローティングバーを表示 |

`model_base_url` だけは設定画面に項目がなく、`config.toml` を直接編集する（変更後は再起動）。

## コマンドライン

```
BAREVoiceJP [オプション]
```

| オプション | 既定値 | 内容 |
|---|---|---|
| `--model NAME` | `sensevoice` | `sensevoice` / `reazonspeech` / `haas-ja-v1.0` / `whisper-base` / `whisper-small` / `whisper-turbo` |
| `--model-dir DIR` | `%APPDATA%\BAREVoiceJP\models/<name>` | モデルのディレクトリ |
| `--hotkey KEY` | `alt_r` | 録音キー |
| `--device SPEC` | 既定デバイス | 入力デバイス番号、または名前の一部 |
| `--language LANG` | `ja` | SenseVoice の言語（`auto` も可） |
| `--num-threads N` | `2` | CPU スレッド数 |
| `--provider NAME` | `cpu` | ONNX Runtime execution provider |
| `--hotwords-score F` | `2.0` | ホットワードスコア（現行エンジンは未対応） |
| `--highpass` / `--no-highpass` | OFF | 80 Hz ハイパスフィルタ |
| `--denoise` / `--no-denoise` | OFF | RNNoise ノイズリダクション（未実装・指定しても無効） |
| `--pre-roll-ms MS` | `200` | 押下直前の音声を含める長さ |
| `--min-duration-ms MS` | `200` | これ未満の押下は破棄 |
| `--paste-delay-ms MS` | `50` | クリップボード設定から貼り付けまでの待機 |
| `--paste-modifier MOD` | `ctrl` | 貼り付けの修飾キー。`ctrl` 以外は警告が出て無視される |
| `--restore-clipboard` / `--no-restore-clipboard` | OFF | 貼り付け後に元のクリップボードを復元 |
| `--itn` / `--no-itn` | ON | SenseVoice の逆テキスト正規化 |
| `--fuzzy` | OFF | 辞書のあいまい一致を有効化 |
| `--list-devices` | — | 入力デバイス一覧を表示して終了 |
| `--eval FILE` | — | テキスト整形の評価ケースを実行して終了 |
| `--sweep` | — | `--eval` 時にファジーしきい値 70/75/80/85 を比較 |
| `--debug` | — | デバッグログを出す（`%APPDATA%\BAREVoiceJP\logs\app.log` にも記録） |
| `--console` | — | デバッグビルドでコンソールを残す（リリースは常に非表示） |
| `--help` | — | ヘルプを表示して終了 |

## `stats.json`

設定画面のステータスカードに出す累積実績。`%APPDATA%\BAREVoiceJP\stats.json` に、数値だけを保存する。

| キー | 内容 |
|---|---|
| `total_chars` | 導入からの総入力文字数（Unicode スカラー値単位） |
| `total_sessions` | 貼り付けに成功した発話の総回数 |
| `today_chars` | 当日の入力文字数。ローカル日付が変わると 0 に戻る |
| `today_date` | `today_chars` が属するローカル日付（`YYYY-MM-DD`） |

- 本文も音声も保存しない（本文は `history.json` 側）
- 加算は `Ctrl+V` の送信まで成功した時点。貼り付け先が変わって送らなかった場合は数えない
- 書き込みは `stats.json.tmp` へ書き切って `sync_all` してから置き換える。途中でクラッシュしても既存ファイルは壊れない
- 推定削減時間は 200 文字/分で換算する（`total_chars / 200` 分）。60 分以上は「約 X.X 時間」で出す
- 消せば 0 から再集計。壊れた JSON は警告をログに出して 0 から始める

## `history.json`

確定した認識テキストを、ローカル日時とモデル名つきで直近10件だけ `%APPDATA%\BAREVoiceJP\history.json` に保存する。

- 設定画面の「履歴」タブに新しい順で並ぶ
- 各行のコピーボタンでクリップボードへ送れる
- 「全件クリア」で `history.json` ごと空にする
- 書き込みは `history.json.tmp` へ書いて `sync_all` してから置き換える
- 壊れていたら警告をログに出し、空の履歴で起動する
