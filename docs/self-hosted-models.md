# 自前の配布元からモデルを配る

既定では sherpa-onnx の GitHub Releases（Haas-ja だけ Hugging Face）から直接落とす。社内サーバーや共有フォルダに置きたい場合は `%APPDATA%\BAREVoiceJP\config.toml` の `model_base_url` を1つ変える。空に戻せば公式に戻る。

```toml
# HTTPS の Web サーバー（末尾スラッシュは有無どちらでも可）
model_base_url = "https://barevoice.example.co.jp/models"
```

```toml
# UNC ファイル共有（TOML のリテラル文字列 '' なら \ をそのまま書ける）
model_base_url = '\\fileserver\share\BAREVoiceJP\models'
```

```toml
# ローカル / ネットワークドライブの絶対パス
model_base_url = 'D:\BAREVoiceJP\models'
```

設定はアプリの再起動で反映される。`model_base_url` は設定画面に項目がない（認証情報を含み得るため、画面にもログにも出さない設計）。

## 指定できるスキーム

| 指定 | 例 | 動作 |
|---|---|---|
| HTTPS | `https://host/barevoice/models` | `ureq` で取得。接続10秒・応答ヘッダ30秒・本文2時間でタイムアウト |
| HTTP（ループバックのみ） | `http://127.0.0.1:8765` | ローカル検証用。`127.0.0.1` / `localhost` / `[::1]` 以外の平文 HTTP は拒否 |
| UNC | `\\fileserver\share\BAREVoiceJP\models` | 共有フォルダからストリームコピー |
| 絶対パス | `D:\BAREVoiceJP\models` | ローカルフォルダからストリームコピー |
| `file://` | `file:///C:/BAREVoiceJP/models` | 絶対パスとして扱う |

HTTPS → HTTP のリダイレクトも拒否する。

## サーバー側の置き方

ベース URL / パスの直下に、アーカイブ本体（または ONNX 一式）と `checksums.sha256` を置く。

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

`encoder.quantized.onnx` から下は `haas-ja-v1.0` の量子化版用。`encoder.quantized.onnx` と `encoder.data` は必ず同じフォルダに置く。

Haas-ja は `encoder.onnx`（fp32）だけが既にある場合も起動できるが、軽くしたいなら消して取り直す。

### `checksums.sha256`

`sha256sum` 互換の形式（`<64桁の16進>␠␠<ファイル名>`）。`#` 行、`*` 付きファイル名、`models/foo.tar.bz2` のようなパス表記も読む。

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

作るとき:

```powershell
Get-ChildItem .\*.tar.bz2 | Get-FileHash -Algorithm SHA256 |
  ForEach-Object { "$($_.Hash.ToLower())  $([IO.Path]::GetFileName($_.Path))" } |
  Set-Content -Encoding ascii checksums.sha256
Get-ChildItem .\encoder.quantized.onnx, .\encoder.data, .\decoder.onnx, .\joiner.onnx, .\tokens.txt |
  Get-FileHash -Algorithm SHA256 |
  ForEach-Object { "$($_.Hash.ToLower())  $([IO.Path]::GetFileName($_.Path))" } |
  Add-Content -Encoding ascii checksums.sha256
```

```bash
sha256sum *.tar.bz2 encoder.quantized.onnx encoder.data decoder.onnx joiner.onnx tokens.txt > checksums.sha256
```

ハッシュは**実際に配るバイト列**に対して計算する。再圧縮したら必ず作り直す。マニフェスト自体の改ざん対策は経路の信頼に依存するので、HTTPS か共有 ACL で守る。

## 取得の流れと失敗時

1. マニフェストを取って、対象ファイルの期待ハッシュを得る
2. `.part` へダウンロード / コピー（進捗は UI に流れる）
3. **ローカルの `.part` を SHA-256 で検証**
4. 一致したら確定。アーカイブは `models/` へ展開、直接配置モデルは `models/Haas-ja-v1.0-onnx/` へ移す
5. 不一致なら `.part` を消して、カードに `SHA-256 が一致しません。ダウンロードが不完全です。再ダウンロードしてください。` を出す

進捗は `ダウンロード中 → 検証中 → 展開中 → DL済み` の順。`Content-Length` を返さない配布元や、受信バイト数が想定と違う場合はサイズを検証できないのでその場で止める。公式の tar.bz2 もバイナリに埋め込んだ SHA-256 とバイト数で検証している。

主なエラー文言:

- `モデル配布元に接続できません。ネットワーク接続を確認してください: <URL>`
- `配布元のフォルダに接続できません` / `ファイルがありません` / `アクセス権を確認してください`
- `… の SHA-256 がありません`（マニフェストに対象行が無い）

## ローカルのモックで試す

PowerShell で、実アーカイブをローカル HTTP に載せる例。

```powershell
New-Item -ItemType Directory -Force .\mockmodels | Out-Null
Copy-Item .\models\sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2 .\mockmodels\
Get-ChildItem .\mockmodels\*.tar.bz2 | Get-FileHash -Algorithm SHA256 |
  ForEach-Object { "$($_.Hash.ToLower())  $([IO.Path]::GetFileName($_.Path))" } |
  Set-Content -Encoding ascii .\mockmodels\checksums.sha256

py -m http.server 8765 --directory .\mockmodels
```

```toml
model_base_url = "http://127.0.0.1:8765"
```

設定画面の「モデル」で対象を未インストールにして「ダウンロード」を押す。`ダウンロード中` → `SHA-256 を検証しています` → `アーカイブを展開しています` → `DL済み` と進めば正常。

異常系の見方:

- モックサーバーを止めて別モデルを落とす → `モデル配布元に接続できません…`
- `checksums.sha256` のハッシュを1文字変える → `SHA-256 が一致しません…再ダウンロードしてください`
- 対象行を消す → `… の SHA-256 がありません`
- UNC を見るなら、共有に同じファイルを置いて `model_base_url = '\\<host>\<share>\mockmodels'` に変える
