#!/usr/bin/env python3
"""超低遅延 Push-to-Talk 音声入力 (SenseVoice / ReazonSpeech / sherpa-onnx)。

キーを押している間だけ録音し、離した瞬間に推論してアクティブウィンドウへ
Ctrl+V (macOS は Cmd+V) で貼り付ける、完全ローカルのプロトタイプ。

■ インストール
    python3 -m pip install sherpa-onnx sounddevice pynput pyperclip numpy

    # Linux (PortAudio とクリップボード用ツール)
    sudo apt install libportaudio2 xclip        # Wayland なら wl-clipboard
    # macOS
    brew install portaudio
    # Windows は pip だけで可 (PortAudio は sounddevice が同梱)

■ モデル
    mkdir -p models && cd models
    # SenseVoice-Small
    curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2
    tar xf sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2
    # ReazonSpeech (Zipformer, 日本語)
    curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01.tar.bz2
    tar xf sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01.tar.bz2

    CONFIG["model_dir"] は SenseVoice、CONFIG["reazonspeech_model_dir"] は
    ReazonSpeech の展開先。別の場所なら --model-dir /path/to/model-dir を指定する。

■ 使い方
    python3 voice_input.py                          # SenseVoice
    python3 voice_input.py --model reazonspeech     # ReazonSpeech
    python3 voice_input.py --list-devices
    python3 voice_input.py --hotkey f9 --device 2

    既定の録音キーは右 Alt (macOS は右 Option)。
    押している間だけ録音。離すと推論して貼り付け。
    録音中に Esc を押すとキャンセル。0.2 秒未満の押下も破棄。
    終了は Ctrl+C。

■ 注意
    - グローバルホットキーには GUI セッションが必要。
      macOS は「アクセシビリティ」と「入力監視」の許可が必要。
      Linux は X11 前提 (Wayland では pynput の制限を受ける)。
      WSL では Windows 側のキーを取得できないため、Windows 版 Python での
      実行を推奨。WSL でも X11/WSLg のアプリに対してなら動作し得る。
    - クリップボード復元は既定で無効 (二重貼り付け・競合回避)。
      `CONFIG["restore_clipboard"] = True` にすると復元するが、プレーンテキスト
      のみの退避なので、元が画像やリッチテキストの場合は失われる。
    - 推論と貼り付けは 1 本のワーカースレッドで直列化している
      (asyncio 等は不使用)。Rust 移植時は各クラスをそのまま構造体へ対応させる。
"""

from __future__ import annotations

import argparse
import logging
import os
import platform as platform_module
import queue
import re
import shutil
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np

ROOT = Path(__file__).resolve().parent
logger = logging.getLogger("voice_input")

# ---------------------------------------------------------------------------
# ここを 1 行ずつ変えれば主要な挙動を調整できる。
# ---------------------------------------------------------------------------
CONFIG: dict[str, Any] = {
    # --- モデル ---
    "model": "sensevoice",      # sensevoice / reazonspeech
    "model_dir": str(
        ROOT / "models" / "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17"
    ),
    "reazonspeech_model_dir": str(
        ROOT / "models" / "sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01"
    ),
    "num_threads": 2,           # CPU 推論スレッド数
    "provider": "cpu",          # cpu / cuda / coreml など
    "language": "auto",         # SenseVoice のみ: auto / ja / en / zh / ko / yue
    "use_itn": True,            # SenseVoice のみ: 逆テキスト正規化

    # --- 録音 / ホットキー (None = OS ごとの既定値) ---
    "hotkey": "alt_r",          # 右Alt (= Key.alt_r)。None なら OS 既定
    "paste_modifier": None,     # 例: "ctrl", "cmd", "alt"
    "paste_key": "v",
    "sample_rate": 16000,       # マイク入力 (固定)
    "block_size": 256,          # PortAudio コールバックのフレーム数 (16ms)
    "input_latency": "low",     # PortAudio の希望レイテンシ ("low"/"high"/None)
    "pre_roll_ms": 200,         # 押下直前の音声を先頭に結合
    "tail_grace_ms": 80,        # 離した直後の取りこぼし防止
    "min_duration_ms": 200,     # これ未満の押下は破棄
    "repeat_guard_ms": 250,     # キーリピート/直後の再押下を無視する時間
    "ring_seconds": 60.0,       # 常時保持する音声バッファ長
    "device": None,             # 入力デバイス番号 or 名前の一部

    # --- 貼り付け / クリップボード ---
    "paste_delay_ms": 50,       # クリップボード設定後、貼り付けまでの待機
    "restore_clipboard": False, # 既定はOFF (Windowsの二重貼り付け/競合回避)
    "clipboard_restore_delay_ms": 300,

    # --- テキスト整形 ---
    "remove_tags": True,        # <|...|> などのタグを除去
    "remove_ja_spaces": True,   # 日本語文字間の不要な空白を削除
    "replacements": {
        "えーぴーあい": "API",
        "えーぴーアイ": "API",
        "エーピーアイ": "API",
        "えーあい": "AI",
        "エーアイ": "AI",
        "じーぴーてぃー": "GPT",
        "ジーピーティー": "GPT",
        "えるえるえむ": "LLM",
    },
}

SENSEVOICE_DOWNLOAD_HINT = """\
  mkdir -p models && cd models
  curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2
  tar xf sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2
"""

REAZONSPEECH_DOWNLOAD_HINT = """\
  mkdir -p models && cd models
  curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01.tar.bz2
  tar xf sherpa-onnx-zipformer-ja-reazonspeech-2024-08-01.tar.bz2
"""


# ---------------------------------------------------------------------------
# 例外
# ---------------------------------------------------------------------------
class VoiceInputError(RuntimeError):
    """起動時・実行時にユーザーへそのまま見せてよいエラー。"""


class ModelNotFoundError(VoiceInputError):
    pass


class AudioCaptureError(VoiceInputError):
    pass


class ClipboardError(VoiceInputError):
    pass


# ---------------------------------------------------------------------------
# モデル解決 / 推論エンジン
# ---------------------------------------------------------------------------
def resolve_sensevoice_files(model_dir: str | Path) -> tuple[Path, Path]:
    """SenseVoice のモデル (.onnx) と tokens.txt を探す。"""
    path = Path(model_dir).expanduser()
    if path.is_file():
        model_candidates = [path]
        base = path.parent
    else:
        base = path
        model_candidates = [
            base / name
            for name in ("model.int8.onnx", "model.onnx", "model.fp16.onnx")
        ]

    model = next((p for p in model_candidates if p.is_file()), None)
    if model is None:
        raise ModelNotFoundError(
            f"SenseVoice モデルが見つかりません: {base}\n"
            f"探したファイル: {', '.join(p.name for p in model_candidates)}\n"
            f"次のいずれかで用意してください:\n{SENSEVOICE_DOWNLOAD_HINT}"
            f"または --model-dir /path/to/model-dir を指定してください。"
        )

    tokens = base / "tokens.txt"
    if not tokens.is_file():
        raise ModelNotFoundError(
            f"tokens.txt が見つかりません: {tokens}\n"
            f"モデルの tar.bz2 を展開し直すか、--model-dir を確認してください。"
        )
    return model, tokens


def resolve_reazonspeech_files(
    model_dir: str | Path,
) -> tuple[Path, Path, Path, Path]:
    """ReazonSpeech Zipformer の encoder/decoder/joiner/tokens を探す。"""
    base = Path(model_dir).expanduser()
    if base.is_file():
        base = base.parent
    expected = {
        "encoder": base / "encoder-epoch-99-avg-1.int8.onnx",
        "decoder": base / "decoder-epoch-99-avg-1.onnx",
        "joiner": base / "joiner-epoch-99-avg-1.int8.onnx",
        "tokens": base / "tokens.txt",
    }
    missing = [
        f"{name}: {path.name}"
        for name, path in expected.items()
        if not path.is_file()
    ]
    if missing:
        raise ModelNotFoundError(
            f"ReazonSpeech モデルが見つかりません: {base}\n"
            f"不足ファイル: {', '.join(missing)}\n"
            f"次のコマンドで用意してください:\n{REAZONSPEECH_DOWNLOAD_HINT}"
            f"または --model-dir /path/to/model-dir を指定してください。"
        )
    return (
        expected["encoder"],
        expected["decoder"],
        expected["joiner"],
        expected["tokens"],
    )


def load_sherpa_onnx() -> Any:
    try:
        import sherpa_onnx
    except Exception as exc:
        raise VoiceInputError(
            "sherpa-onnx を読み込めません。\n"
            "  python3 -m pip install sherpa-onnx\n"
            f"  詳細: {exc}"
        ) from exc
    return sherpa_onnx


class OfflineAsrEngine:
    """sherpa-onnx OfflineRecognizer の共通推論パイプライン。"""

    def __init__(self, recognizer: Any, sample_rate: int) -> None:
        self._recognizer = recognizer
        self.sample_rate = sample_rate

    def transcribe(self, samples: np.ndarray) -> str:
        """16kHz / mono / float32 の波形を文字起こしする。"""
        stream = self._recognizer.create_stream()
        stream.accept_waveform(self.sample_rate, samples)
        self._recognizer.decode_stream(stream)
        return stream.result.text


class SenseVoiceEngine(OfflineAsrEngine):
    """SenseVoice-Small 用のエンジン。"""

    def __init__(
        self,
        model_dir: str | Path,
        sample_rate: int,
        num_threads: int,
        provider: str,
        language: str,
        use_itn: bool,
    ) -> None:
        model, tokens = resolve_sensevoice_files(model_dir)
        sherpa_onnx = load_sherpa_onnx()

        logger.info("SenseVoice モデル読み込み: %s", model)
        try:
            recognizer = sherpa_onnx.OfflineRecognizer.from_sense_voice(
                model=str(model),
                tokens=str(tokens),
                num_threads=num_threads,
                sample_rate=sample_rate,
                provider=provider,
                language=language,
                use_itn=use_itn,
                debug=False,
            )
        except Exception as exc:
            raise VoiceInputError(
                f"SenseVoice モデルの初期化に失敗しました: {model}\n"
                f"  provider={provider} / num_threads={num_threads}\n"
                f"  詳細: {exc}"
            ) from exc
        super().__init__(recognizer, sample_rate)
        logger.info(
            "モデル準備完了 (SenseVoice / ITN=%s, language=%s, threads=%d, provider=%s)",
            "on" if use_itn else "off",
            language,
            num_threads,
            provider,
        )


class ReazonSpeechEngine(OfflineAsrEngine):
    """ReazonSpeech Zipformer (日本語) 用のエンジン。"""

    def __init__(
        self,
        model_dir: str | Path,
        sample_rate: int,
        num_threads: int,
        provider: str,
    ) -> None:
        encoder, decoder, joiner, tokens = resolve_reazonspeech_files(model_dir)
        sherpa_onnx = load_sherpa_onnx()

        logger.info("ReazonSpeech モデル読み込み: %s", encoder.parent)
        try:
            recognizer = sherpa_onnx.OfflineRecognizer.from_transducer(
                encoder=str(encoder),
                decoder=str(decoder),
                joiner=str(joiner),
                tokens=str(tokens),
                num_threads=num_threads,
                sample_rate=sample_rate,
                decoding_method="greedy_search",
                provider=provider,
                debug=False,
            )
        except Exception as exc:
            raise VoiceInputError(
                f"ReazonSpeech モデルの初期化に失敗しました: {encoder.parent}\n"
                f"  provider={provider} / num_threads={num_threads}\n"
                f"  詳細: {exc}"
            ) from exc
        super().__init__(recognizer, sample_rate)
        logger.info(
            "モデル準備完了 (ReazonSpeech / threads=%d, provider=%s)",
            num_threads,
            provider,
        )


def build_engine(
    config: dict[str, Any],
    model_dir: str | Path | None = None,
) -> OfflineAsrEngine:
    """CONFIG["model"] に応じて推論エンジンを生成する。"""
    model_type = str(config["model"]).lower()
    sample_rate = int(config["sample_rate"])
    num_threads = int(config["num_threads"])
    provider = str(config["provider"])

    if model_type == "sensevoice":
        selected_dir = model_dir if model_dir is not None else config["model_dir"]
        return SenseVoiceEngine(
            model_dir=selected_dir,
            sample_rate=sample_rate,
            num_threads=num_threads,
            provider=provider,
            language=str(config["language"]),
            use_itn=bool(config["use_itn"]),
        )
    if model_type == "reazonspeech":
        selected_dir = (
            model_dir if model_dir is not None else config["reazonspeech_model_dir"]
        )
        return ReazonSpeechEngine(
            model_dir=selected_dir,
            sample_rate=sample_rate,
            num_threads=num_threads,
            provider=provider,
        )
    raise VoiceInputError(
        f"不明なモデル種別です: {config['model']} "
        "(sensevoice / reazonspeech)"
    )


# ---------------------------------------------------------------------------
# テキスト整形
# ---------------------------------------------------------------------------
_TAG_RE = re.compile(r"<\|[^|]*\|>")
_SPECIAL_TOKEN_RE = re.compile(r"</?(?:s|unk|pad|blank)>", re.IGNORECASE)
# 日本語 (かな・漢字) 同士の間の空白だけを削除する。英語や数字の空白は残す。
_JA_CHARS = r"\u3040-\u30ff\u3400-\u4dbf\u4e00-\u9fff\u3005\u3006"
_JA_SPACE_RE = re.compile(rf"(?<=[{_JA_CHARS}])\s+(?=[{_JA_CHARS}])")


class TextFormatter:
    """タグ除去・空白正規化・置換辞書。"""

    def __init__(
        self,
        replacements: dict[str, str],
        remove_tags: bool = True,
        remove_ja_spaces: bool = True,
    ) -> None:
        self._remove_tags = remove_tags
        self._remove_ja_spaces = remove_ja_spaces
        # 長いキーを先に置換する ("えーぴーあい" と "えーあい" の競合対策)
        self._replacements = sorted(
            ((k, v) for k, v in replacements.items() if k),
            key=lambda kv: len(kv[0]),
            reverse=True,
        )

    def format(self, text: str) -> str:
        if not text:
            return ""
        if self._remove_tags:
            text = _TAG_RE.sub("", text)
            text = _SPECIAL_TOKEN_RE.sub("", text)
        text = text.replace("\u3000", " ")
        text = re.sub(r"\s+", " ", text).strip()
        if self._remove_ja_spaces:
            text = _JA_SPACE_RE.sub("", text)
        for src, dst in self._replacements:
            text = text.replace(src, dst)
        return text.strip()


# ---------------------------------------------------------------------------
# 音声キャプチャ (常時リングバッファへ書き込み、プリアロールを可能にする)
# ---------------------------------------------------------------------------
class AudioCapture:
    """PortAudio 入力ストリーム + 固定長リングバッファ。

    position は「これまでに書き込んだ総サンプル数」。押下時の position から
    pre_roll 分だけ遡り、離した時の position + tail_grace 分までを切り出す。
    """

    def __init__(
        self,
        sample_rate: int,
        ring_seconds: float,
        block_size: int,
        device: int | str | None,
        latency: str | None = "low",
    ) -> None:
        capacity = max(1, int(sample_rate * ring_seconds))
        self.sample_rate = sample_rate
        self.block_size = block_size
        self._device = device
        self._latency = latency
        self._ring = np.zeros(capacity, dtype=np.float32)
        self._total = 0
        self._xruns = 0
        self._lock = threading.Lock()
        self._stream: Any = None

    # -- 状態 ---------------------------------------------------------------
    @property
    def position(self) -> int:
        with self._lock:
            return self._total

    @property
    def xruns(self) -> int:
        with self._lock:
            return self._xruns

    # -- 書き込み -----------------------------------------------------------
    def append(self, samples: np.ndarray) -> None:
        """テストや外部入力からも使えるリングバッファ書き込み。"""
        mono = np.asarray(samples, dtype=np.float32).reshape(-1)
        incoming = mono.size
        if incoming == 0:
            return
        with self._lock:
            capacity = self._ring.size
            if incoming > capacity:
                # 溢れた分は捨て、最後の capacity サンプルだけを保持する。
                mono = mono[-capacity:]
            n = mono.size
            # 保持する先頭サンプルの絶対位置は total + incoming - n。
            start = (self._total + incoming - n) % capacity
            first = min(n, capacity - start)
            self._ring[start:start + first] = mono[:first]
            if n > first:
                self._ring[:n - first] = mono[first:]
            self._total += incoming

    def read_range(self, start_sample: int, end_sample: int) -> np.ndarray:
        """絶対サンプル番号 [start, end) をコピーして返す。"""
        start_sample = int(start_sample)
        end_sample = int(end_sample)
        with self._lock:
            total = self._total
            capacity = self._ring.size
            oldest = max(0, total - capacity)
            start = max(start_sample, oldest)
            end = min(end_sample, total)
            if end <= start:
                return np.empty(0, dtype=np.float32)
            truncated = start != start_sample or end != end_sample
            count = end - start
            out = np.empty(count, dtype=np.float32)
            index = start % capacity
            first = min(count, capacity - index)
            out[:first] = self._ring[index:index + first]
            if count > first:
                out[first:] = self._ring[:count - first]
        if truncated:
            logger.warning(
                "音声バッファ範囲が不足 (要求 %d..%d / 利用可能 %d..%d)",
                start_sample,
                end_sample,
                oldest,
                total,
            )
        return out

    # -- PortAudio ----------------------------------------------------------
    def _callback(self, indata: Any, frames: int, time_info: Any, status: Any) -> None:
        try:
            if status:
                with self._lock:
                    self._xruns += 1
                logger.debug("PortAudio status: %s", status)
            # indata はコールバック中のみ有効なので、ここで必ずコピーする。
            mono = np.asarray(indata[:, 0], dtype=np.float32).copy()
            self.append(mono)
        except Exception:  # コールバックから例外を外へ出さない
            logger.debug("音声コールバックでエラー", exc_info=True)

    def start(self) -> None:
        try:
            import sounddevice as sd
        except Exception as exc:
            raise AudioCaptureError(
                "sounddevice / PortAudio を読み込めません。\n"
                "  python3 -m pip install sounddevice\n"
                "  Linux: sudo apt install libportaudio2\n"
                f"  詳細: {exc}"
            ) from exc

        device = self._device
        if isinstance(device, str) and device.strip().isdigit():
            device = int(device.strip())
        if isinstance(device, str):
            try:
                device = self._find_device(sd, device)
            except AudioCaptureError:
                raise
            except Exception as exc:
                raise AudioCaptureError(
                    f"入力デバイス一覧を取得できません: {exc}"
                ) from exc

        try:
            info = sd.query_devices(device, "input")
        except Exception as exc:
            raise AudioCaptureError(
                "入力デバイスを取得できません。\n"
                "  --list-devices でデバイス一覧を確認し、"
                "--device N を指定してください。\n"
                f"  詳細: {exc}"
            ) from exc

        logger.info(
            "マイク: [%s] %s (device default %.0f Hz)",
            device if device is not None else "default",
            info.get("name", "?"),
            float(info.get("default_samplerate", 0.0)),
        )

        try:
            sd.check_input_settings(
                device=device,
                samplerate=self.sample_rate,
                channels=1,
                dtype="float32",
            )
        except Exception as exc:
            # 事前チェックはバックエンドによって誤判定することがあるため、
            # ここでは止めずに実際のストリーム開始を試す。
            logger.warning(
                "%d Hz / mono / float32 の事前チェックに失敗しました "
                "(ストリーム開始を試みます): %s",
                self.sample_rate,
                exc,
            )

        stream = None
        try:
            stream = sd.InputStream(
                samplerate=self.sample_rate,
                blocksize=self.block_size,
                device=device,
                channels=1,
                dtype="float32",
                latency=self._latency,
                callback=self._callback,
            )
            stream.start()
            self._stream = stream
        except Exception as exc:
            if stream is not None:
                try:
                    stream.close()
                except Exception:
                    logger.debug("失敗したストリームのクローズに失敗", exc_info=True)
            raise AudioCaptureError(
                "マイク入力ストリームを開始できません。\n"
                f"  16kHz / mono / float32 に対応したデバイスか確認してください。\n"
                f"  --list-devices で別デバイスを確認し、--device N を指定できます。\n"
                f"  詳細: {exc}"
            ) from exc
        logger.info("録音ストリーム開始 (%d Hz, mono, float32)", self.sample_rate)

    @staticmethod
    def _find_device(sd: Any, needle: str) -> int:
        matches: list[tuple[int, str]] = []
        for index, dev in enumerate(sd.query_devices()):
            if int(dev.get("max_input_channels", 0)) <= 0:
                continue
            name = str(dev.get("name", ""))
            if needle.lower() in name.lower():
                matches.append((index, name))
        if not matches:
            names = [
                f"{i}: {d['name']}"
                for i, d in enumerate(sd.query_devices())
                if int(d.get("max_input_channels", 0)) > 0
            ]
            raise AudioCaptureError(
                f"名前に '{needle}' を含む入力デバイスがありません。\n"
                + ("\n".join(names) if names else "入力デバイスが見つかりません。")
            )
        if len(matches) > 1:
            logger.warning(
                "デバイス名が複数一致しました。最初のものを使います: %s",
                ", ".join(f"{i}:{n}" for i, n in matches),
            )
        return matches[0][0]

    def stop(self) -> None:
        stream, self._stream = self._stream, None
        if stream is None:
            return
        try:
            stream.stop()
        except Exception:
            logger.debug("録音ストリーム停止時の例外", exc_info=True)
        try:
            stream.close()
        except Exception:
            logger.debug("録音ストリームクローズ時の例外", exc_info=True)


def list_input_devices() -> int:
    try:
        import sounddevice as sd
    except Exception as exc:
        logger.error(
            "sounddevice / PortAudio を読み込めません: %s\n"
            "  python3 -m pip install sounddevice\n"
            "  Linux: sudo apt install libportaudio2",
            exc,
        )
        return 1
    try:
        devices = sd.query_devices()
        default_input, _ = sd.default.device
    except Exception as exc:
        logger.error("オーディオデバイスを取得できません: %s", exc)
        return 1

    print("入力デバイス:")
    found = False
    for index, dev in enumerate(devices):
        if int(dev.get("max_input_channels", 0)) <= 0:
            continue
        found = True
        mark = "*" if index == default_input else " "
        print(
            f"{mark} {index:2d}: {dev['name']} "
            f"(input ch={dev['max_input_channels']}, "
            f"default={float(dev.get('default_samplerate', 0.0)):.0f} Hz)"
        )
    if not found:
        print("  (入力デバイスが見つかりません)")
    print("  * = 既定の入力デバイス")
    return 0 if found else 1


# ---------------------------------------------------------------------------
# OS 判定 / キー操作 / クリップボード
# ---------------------------------------------------------------------------
@dataclass(frozen=True)
class PlatformDefaults:
    kind: str          # windows / macos / linux / wsl
    label: str
    hotkey: str
    paste_modifier: str


def _is_wsl() -> bool:
    if os.environ.get("WSL_DISTRO_NAME") or os.environ.get("WSL_INTEROP"):
        return True
    try:
        version = Path("/proc/version").read_text(encoding="utf-8", errors="ignore").lower()
    except OSError:
        return False
    return "microsoft" in version or "wsl" in version


def detect_platform() -> PlatformDefaults:
    system = platform_module.system()
    if system == "Windows":
        return PlatformDefaults("windows", "Windows", "alt_r", "ctrl")
    if system == "Darwin":
        return PlatformDefaults("macos", "macOS", "alt_r", "cmd")
    if system == "Linux":
        if _is_wsl():
            return PlatformDefaults("wsl", "WSL", "alt_r", "ctrl")
        return PlatformDefaults("linux", "Linux", "alt_r", "ctrl")
    raise VoiceInputError(f"未対応の OS です: {system}")


def load_pynput_keyboard() -> Any:
    try:
        from pynput import keyboard
    except Exception as exc:
        raise VoiceInputError(
            "pynput を初期化できません。グローバルホットキーには GUI セッションが必要です。\n"
            "  Linux: X11 が必要 (Wayland は制限あり)\n"
            "  macOS: 「アクセシビリティ」と「入力監視」を許可\n"
            "  WSL: Windows 版 Python での実行を推奨\n"
            f"  詳細: {exc}"
        ) from exc
    return keyboard


_KEY_ALIASES = {
    "control": "ctrl",
    "control_l": "ctrl_l",
    "control_r": "ctrl_r",
    "option": "alt",
    "option_l": "alt_l",
    "option_r": "alt_r",
    "command": "cmd",
    "command_l": "cmd_l",
    "command_r": "cmd_r",
    "escape": "esc",
    "return": "enter",
}


def parse_key(spec: str) -> Any:
    keyboard = load_pynput_keyboard()
    text = spec.strip()
    if not text:
        raise VoiceInputError("ホットキーが空です。例: ctrl_r, alt_r, f9")
    name = text.lower().removeprefix("key.")
    name = _KEY_ALIASES.get(name, name)
    if hasattr(keyboard.Key, name):
        return getattr(keyboard.Key, name)
    if len(text) == 1:
        return keyboard.KeyCode.from_char(text.lower())
    raise VoiceInputError(
        f"不明なキー名です: {spec}\n"
        "  例: ctrl_r, alt_r, shift_r, f9, space, v"
    )


_RIGHT_ALT_NAMES = frozenset({"alt_r", "alt_gr"})


def _key_enum_name(key: Any) -> str | None:
    name = getattr(key, "name", None)
    return name.lower() if isinstance(name, str) else None


# Windows の低レベルキーボードフック用定数
_WM_KEYDOWN = 0x0100
_WM_KEYUP = 0x0101
_WM_SYSKEYDOWN = 0x0104
_WM_SYSKEYUP = 0x0105
_VK_MENU = 0x12
_VK_RMENU = 0xA5
_LLKHF_EXTENDED = 0x01


def _is_right_alt_event(msg: int, data: Any) -> bool:
    """Win32 の KBDLLHOOKSTRUCT が右 Alt のイベントか判定する。"""
    if msg not in (_WM_KEYDOWN, _WM_KEYUP, _WM_SYSKEYDOWN, _WM_SYSKEYUP):
        return False
    vk = int(getattr(data, "vkCode", -1))
    flags = int(getattr(data, "flags", 0))
    return vk == _VK_RMENU or (vk == _VK_MENU and bool(flags & _LLKHF_EXTENDED))


def same_key(a: Any, b: Any) -> bool:
    if a == b:
        return True
    # Windows の右 Alt は AltGr として届くことがあるため、同一視する。
    name_a = _key_enum_name(a)
    name_b = _key_enum_name(b)
    if name_a in _RIGHT_ALT_NAMES and name_b in _RIGHT_ALT_NAMES:
        return True
    char_a = getattr(a, "char", None)
    char_b = getattr(b, "char", None)
    return bool(char_a and char_b and char_a.lower() == char_b.lower())


class PlatformIntegration:
    """OS ごとのクリップボード、貼り付けキー送信、キー監視を担当。"""

    def __init__(self, defaults: PlatformDefaults, paste_key: str = "v") -> None:
        self.defaults = defaults
        self.paste_key = paste_key
        self._pyperclip: Any = None

    # -- クリップボード -----------------------------------------------------
    def _load_pyperclip(self) -> Any:
        if self._pyperclip is None:
            try:
                import pyperclip
            except ImportError as exc:
                raise ClipboardError(
                    "pyperclip がインストールされていません。\n"
                    "  python3 -m pip install pyperclip"
                ) from exc
            self._pyperclip = pyperclip
        return self._pyperclip

    def clipboard_get(self) -> str:
        if self.defaults.kind == "wsl":
            return self._wsl_clipboard_get()
        pyperclip = self._load_pyperclip()
        try:
            return pyperclip.paste()
        except Exception as exc:
            raise ClipboardError(
                "クリップボードを読み取れません。\n"
                "  Linux: sudo apt install xclip (Wayland は wl-clipboard)\n"
                f"  詳細: {exc}"
            ) from exc

    def clipboard_set(self, text: str) -> None:
        if self.defaults.kind == "wsl":
            self._wsl_clipboard_set(text)
            return
        pyperclip = self._load_pyperclip()
        try:
            pyperclip.copy(text)
        except Exception as exc:
            raise ClipboardError(
                "クリップボードへ書き込めません。\n"
                "  Linux: sudo apt install xclip (Wayland は wl-clipboard)\n"
                f"  詳細: {exc}"
            ) from exc

    # -- 貼り付け -----------------------------------------------------------
    def paste(self) -> None:
        if self.defaults.kind == "wsl":
            self._wsl_paste()
            return
        if self.defaults.kind == "windows":
            # pynput の Controller を介さず、Win32 SendInput で
            # Ctrl+V を1回だけ送る。
            self._windows_send_ctrl_v()
            return

        keyboard = load_pynput_keyboard()
        modifier_name = self.defaults.paste_modifier.lower()
        modifier_name = _KEY_ALIASES.get(modifier_name, modifier_name)
        modifier = getattr(keyboard.Key, modifier_name, None)
        if modifier is None:
            raise VoiceInputError(
                f"不明な貼り付け修飾キー: {self.defaults.paste_modifier}"
            )
        try:
            controller = keyboard.Controller()
            # キーイベントの間に明示的な待機を入れ、重複送出を避ける。
            controller.press(modifier)
            time.sleep(0.03)
            controller.press(self.paste_key)
            time.sleep(0.03)
            controller.release(self.paste_key)
            time.sleep(0.03)
            controller.release(modifier)
        except Exception as exc:
            raise VoiceInputError(
                "貼り付けキーを送信できませんでした。\n"
                f"  詳細: {exc}"
            ) from exc

    @staticmethod
    def _windows_send_ctrl_v() -> None:
        """Win32 SendInput で Ctrl+V を1回だけ送る。

        pynput の Controller を経由しないため、キーイベントの重複や
        キーリピートの影響を受けにくい。
        """
        try:
            import ctypes
        except Exception as exc:
            raise VoiceInputError(
                f"Windows の SendInput を初期化できません: {exc}"
            ) from exc

        # Win32 の固定幅型。wintypes の型サイズはプラットフォームで
        # 変わるため、ここでは明示的に 16/32bit 型を使う。
        word = ctypes.c_uint16
        dword = ctypes.c_uint32
        long = ctypes.c_int32
        ulong_ptr = (
            ctypes.c_ulonglong
            if ctypes.sizeof(ctypes.c_void_p) == 8
            else ctypes.c_ulong
        )

        class KEYBDINPUT(ctypes.Structure):
            _fields_ = [
                ("wVk", word),
                ("wScan", word),
                ("dwFlags", dword),
                ("time", dword),
                ("dwExtraInfo", ulong_ptr),
            ]

        class MOUSEINPUT(ctypes.Structure):
            _fields_ = [
                ("dx", long),
                ("dy", long),
                ("mouseData", dword),
                ("dwFlags", dword),
                ("time", dword),
                ("dwExtraInfo", ulong_ptr),
            ]

        class HARDWAREINPUT(ctypes.Structure):
            _fields_ = [
                ("uMsg", dword),
                ("wParamL", word),
                ("wParamH", word),
            ]

        class INPUT_UNION(ctypes.Union):
            _fields_ = [
                ("mi", MOUSEINPUT),
                ("ki", KEYBDINPUT),
                ("hi", HARDWAREINPUT),
            ]

        class INPUT(ctypes.Structure):
            _fields_ = [
                ("type", dword),
                ("union", INPUT_UNION),
            ]

        input_keyboard = 1
        keyeventf_keyup = 0x0002
        vk_control = 0x11
        vk_v = 0x56

        def make_event(vk: int, keyup: bool) -> INPUT:
            return INPUT(
                type=input_keyboard,
                union=INPUT_UNION(
                    ki=KEYBDINPUT(
                        wVk=vk,
                        wScan=0,
                        dwFlags=keyeventf_keyup if keyup else 0,
                        time=0,
                        dwExtraInfo=0,
                    )
                ),
            )

        # Ctrl down -> V down -> V up -> Ctrl up を1回の SendInput で送る。
        events = (INPUT * 4)(
            make_event(vk_control, False),
            make_event(vk_v, False),
            make_event(vk_v, True),
            make_event(vk_control, True),
        )
        try:
            user32 = ctypes.WinDLL("user32", use_last_error=True)
            user32.SendInput.argtypes = (
                ctypes.c_uint,
                ctypes.POINTER(INPUT),
                ctypes.c_int,
            )
            user32.SendInput.restype = ctypes.c_uint
            sent = user32.SendInput(4, events, ctypes.sizeof(INPUT))
        except Exception as exc:
            raise VoiceInputError(
                f"SendInput による Ctrl+V 送信に失敗しました: {exc}"
            ) from exc

        if sent != 4:
            error = ctypes.get_last_error()
            raise VoiceInputError(
                "SendInput による Ctrl+V 送信が途中で失敗しました "
                f"(sent={sent}/4, error={error})"
            )

    # -- キー監視 -----------------------------------------------------------
    def create_listener(
        self,
        on_press: Any,
        on_release: Any,
        hotkey: Any = None,
    ) -> Any:
        keyboard = load_pynput_keyboard()
        options: dict[str, Any] = {}
        holder: dict[str, Any] = {
            "listener": None,
            "right_alt_down": False,
        }

        # Windows で右 Alt を PTT に使う場合、Alt 単独押下でメニューバーへ
        # フォーカスが移るのを防ぐ。低レベルフックで右 Alt を OS へ
        # 伝播させず、フック内からアプリの押下/離下処理を直接呼ぶ。
        if (
            self.defaults.kind == "windows"
            and _key_enum_name(hotkey) in _RIGHT_ALT_NAMES
        ):
            def win32_event_filter(msg: int, data: Any) -> bool:
                if not _is_right_alt_event(msg, data):
                    return True

                if msg in (_WM_KEYDOWN, _WM_SYSKEYDOWN):
                    if not holder["right_alt_down"]:
                        holder["right_alt_down"] = True
                        on_press(hotkey)
                elif msg in (_WM_KEYUP, _WM_SYSKEYUP):
                    if holder["right_alt_down"]:
                        holder["right_alt_down"] = False
                        on_release(hotkey)

                listener = holder["listener"]
                if listener is not None:
                    # SuppressException を送出して OS への伝播を止める。
                    listener.suppress_event()
                return False

            options["win32_event_filter"] = win32_event_filter

        try:
            listener = keyboard.Listener(
                on_press=on_press,
                on_release=on_release,
                **options,
            )
            if "win32_event_filter" in options:
                holder["listener"] = listener
            return listener
        except Exception as exc:
            raise VoiceInputError(
                "キーボード監視を開始できません。GUI セッションと権限を確認してください。\n"
                f"  詳細: {exc}"
            ) from exc

    # -- WSL の Windows クリップボード連携 -----------------------------------
    @staticmethod
    def _powershell(command: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess[bytes]:
        exe = shutil.which("powershell.exe")
        if not exe:
            raise ClipboardError(
                "WSL の Windows クリップボード連携に失敗しました。\n"
                "  powershell.exe が見つかりません (WSL interop を有効にしてください)。"
            )
        try:
            return subprocess.run(
                [exe, "-NoProfile", "-NonInteractive", "-Command", command],
                input=input_bytes,
                capture_output=True,
                timeout=10,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise ClipboardError(f"PowerShell の実行に失敗しました: {exc}") from exc

    def _wsl_clipboard_get(self) -> str:
        result = self._powershell(
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard -Raw"
        )
        if result.returncode != 0:
            raise ClipboardError(
                "WSL のクリップボード読み取りに失敗しました: "
                + result.stderr.decode("utf-8", errors="replace").strip()
            )
        return result.stdout.decode("utf-8", errors="replace").rstrip("\r\n")

    def _wsl_clipboard_set(self, text: str) -> None:
        result = self._powershell(
            "[Console]::InputEncoding=[Text.Encoding]::UTF8; "
            "Set-Clipboard -Value ([Console]::In.ReadToEnd())",
            input_bytes=text.encode("utf-8"),
        )
        if result.returncode != 0:
            raise ClipboardError(
                "WSL のクリップボード書き込みに失敗しました: "
                + result.stderr.decode("utf-8", errors="replace").strip()
            )

    def _wsl_paste(self) -> None:
        # フォーカス中の Windows ウィンドウへ Ctrl+V を送る。
        result = self._powershell(
            "$w = New-Object -ComObject WScript.Shell; $w.SendKeys('^v')"
        )
        if result.returncode != 0:
            raise VoiceInputError(
                "WSL からの貼り付けキー送信に失敗しました: "
                + result.stderr.decode("utf-8", errors="replace").strip()
            )


# ---------------------------------------------------------------------------
# Push-to-Talk オーケストレーション
# ---------------------------------------------------------------------------
@dataclass
class Utterance:
    start_sample: int
    end_sample: int
    released_at: float


class VoiceInputApp:
    def __init__(
        self,
        config: dict[str, Any],
        engine: OfflineAsrEngine,
        capture: AudioCapture,
        platform: PlatformIntegration,
    ) -> None:
        self._config = config
        self._engine = engine
        self._capture = capture
        self._platform = platform
        self._formatter = TextFormatter(
            config["replacements"],
            remove_tags=config["remove_tags"],
            remove_ja_spaces=config["remove_ja_spaces"],
        )

        self._sample_rate = int(config["sample_rate"])
        self._pre_roll_samples = max(
            0, int(self._sample_rate * config["pre_roll_ms"] / 1000)
        )
        self._tail_samples = max(
            0, int(self._sample_rate * config["tail_grace_ms"] / 1000)
        )
        self._min_duration_s = max(0.0, config["min_duration_ms"] / 1000.0)
        self._repeat_guard_s = max(0.0, config["repeat_guard_ms"] / 1000.0)
        self._paste_delay_s = max(0.0, config["paste_delay_ms"] / 1000.0)
        self._restore_clipboard = bool(config["restore_clipboard"])
        self._restore_delay_s = max(
            0.0, config["clipboard_restore_delay_ms"] / 1000.0
        )
        self._hotkey_spec = str(config["hotkey"])

        self._lock = threading.Lock()
        self._recording = False
        self._cancelled = False
        self._pressed_at = 0.0
        self._start_sample = 0
        self._last_hotkey_event_at = float("-inf")

        self._queue: queue.Queue[Utterance | None] = queue.Queue()
        self._worker = threading.Thread(
            target=self._worker_loop, name="voice-input-worker", daemon=True
        )
        self._hotkey: Any = None
        self._cancel_key: Any = None

    # -- ライフサイクル -----------------------------------------------------
    def run(self) -> None:
        self._hotkey = parse_key(self._hotkey_spec)
        self._cancel_key = parse_key("esc")
        self._worker.start()
        listener = None
        try:
            listener = self._platform.create_listener(
                self._on_press, self._on_release, self._hotkey
            )
            try:
                listener.start()
            except Exception as exc:
                raise VoiceInputError(
                    "キーボード監視スレッドを開始できません。\n"
                    "  Linux: X11 セッションか確認 / macOS: 権限を確認\n"
                    f"  詳細: {exc}"
                ) from exc
            logger.info(
                "準備完了: ホットキー=%s（押している間だけ録音） / キャンセル=Esc / 終了=Ctrl+C",
                self._hotkey_spec,
            )
            while listener.running:
                time.sleep(0.1)
        except KeyboardInterrupt:
            logger.info("終了します...")
        finally:
            if listener is not None:
                try:
                    listener.stop()
                except Exception:
                    logger.debug("キーボードリスナー停止時の例外", exc_info=True)
            self._queue.put(None)
            self._worker.join(timeout=2.0)

    # -- キーイベント -------------------------------------------------------
    def _on_press(self, key: Any) -> None:
        try:
            if same_key(key, self._hotkey):
                now = time.monotonic()
                with self._lock:
                    # 1) 録音中の追加押下 (Windows のキーリピート) は完全に無視。
                    if self._recording:
                        return
                    # 2) 離下直後のリピート的再押下も無視する。無視した押下で
                    #    タイマーを更新するため、リピートが続く間は開始しない。
                    if now - self._last_hotkey_event_at < self._repeat_guard_s:
                        self._last_hotkey_event_at = now
                        logger.debug("キーリピート/直後の再押下を無視")
                        return
                    self._recording = True
                    self._cancelled = False
                    self._pressed_at = now
                    self._start_sample = max(
                        0, self._capture.position - self._pre_roll_samples
                    )
                    self._last_hotkey_event_at = now
                logger.info("● 録音開始")
                return
            if same_key(key, self._cancel_key):
                with self._lock:
                    if not self._recording or self._cancelled:
                        return
                    self._cancelled = True
                logger.info("× キャンセル (Esc)")
        except Exception:
            logger.exception("キー押下処理でエラー")

    def _on_release(self, key: Any) -> None:
        try:
            if not same_key(key, self._hotkey):
                return
            now = time.monotonic()
            with self._lock:
                self._last_hotkey_event_at = now
                # 録音していない時の離下イベントは無視する。
                if not self._recording:
                    return
                # 受け付けた離下で即座に False にし、以降の重複離下を弾く。
                self._recording = False
                cancelled = self._cancelled
                duration = now - self._pressed_at
                start_sample = self._start_sample
                end_sample = self._capture.position + self._tail_samples

            if cancelled:
                logger.info("× 破棄: Esc によるキャンセル (%.2f 秒)", duration)
                return
            if duration < self._min_duration_s:
                logger.info(
                    "× 破棄: 録音が短すぎます (%.0f ms < %d ms)",
                    duration * 1000.0,
                    self._config["min_duration_ms"],
                )
                return

            logger.info("■ 録音終了: %.2f 秒", duration)
            self._queue.put(Utterance(start_sample, end_sample, now))
        except Exception:
            logger.exception("キー解放処理でエラー")

    # -- 推論ワーカー -------------------------------------------------------
    def _worker_loop(self) -> None:
        while True:
            item = self._queue.get()
            if item is None:
                self._queue.task_done()
                return
            try:
                self._process(item)
            except Exception:
                logger.exception("推論または貼り付けに失敗しました")
            finally:
                self._queue.task_done()

    def _process(self, item: Utterance) -> None:
        # デバイスバッファの遅れを考慮し、tail 分が届くまで少し待つ。
        deadline = time.monotonic() + 2.0
        while self._capture.position < item.end_sample and time.monotonic() < deadline:
            time.sleep(0.005)

        audio = self._capture.read_range(item.start_sample, item.end_sample)
        if audio.size == 0:
            logger.warning("音声データを取得できませんでした")
            return
        if self._capture.xruns:
            logger.debug("PortAudio xruns: %d", self._capture.xruns)

        peak = float(np.max(np.abs(audio)))
        if peak < 1e-4:
            logger.warning("マイク入力がほぼ無音です (デバイス設定を確認してください)")

        started = time.perf_counter()
        raw_text = self._engine.transcribe(audio)
        infer_ms = (time.perf_counter() - started) * 1000.0

        logger.info(
            "推論時間: %.1f ms (音声 %.2f 秒, RMS %.4f)",
            infer_ms,
            audio.size / self._sample_rate,
            float(np.sqrt(np.mean(np.square(audio)))),
        )
        logger.debug("生の認識結果: %r", raw_text)

        text = self._formatter.format(raw_text)
        if not text:
            logger.info("認識結果: (空) — ペーストしません")
            return
        logger.info("認識結果: %s", text)

        self._paste(text, item.released_at)

    # -- クリップボード / 貼り付け ------------------------------------------
    def _paste(self, text: str, released_at: float) -> None:
        original: str | None = None
        if self._restore_clipboard:
            try:
                original = self._platform.clipboard_get()
            except ClipboardError as exc:
                logger.warning("クリップボードの退避をスキップ: %s", exc)

        self._platform.clipboard_set(text)
        paste_ok = False
        try:
            time.sleep(self._paste_delay_s)
            self._platform.paste()
            paste_ok = True
            logger.info(
                "ペースト完了 (キーを離してから %.0f ms)",
                (time.monotonic() - released_at) * 1000.0,
            )
        finally:
            if self._restore_clipboard:
                if not paste_ok:
                    logger.warning(
                        "貼り付けに失敗したため、認識結果をクリップボードに残します"
                    )
                elif original is None:
                    logger.debug("元クリップボードが無いため復元しません")
                else:
                    time.sleep(self._restore_delay_s)
                    try:
                        self._platform.clipboard_set(original)
                        logger.debug("クリップボードを復元しました")
                    except ClipboardError as exc:
                        logger.warning("クリップボードの復元に失敗: %s", exc)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------
def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="SenseVoice / ReazonSpeech を使う Push-to-Talk 音声入力",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--model",
        choices=["sensevoice", "reazonspeech"],
        help="推論モデル",
    )
    parser.add_argument(
        "--model-dir",
        help="選択したモデルのディレクトリ",
    )
    parser.add_argument(
        "--hotkey",
        help="押している間だけ録音するキー (例: ctrl_r, alt_r, f9)",
    )
    parser.add_argument(
        "--paste-modifier",
        help="貼り付けの修飾キー (例: ctrl, cmd, alt)",
    )
    parser.add_argument(
        "--device",
        help="入力デバイス番号または名前の一部 (--list-devices で確認)",
    )
    parser.add_argument(
        "--language",
        choices=["auto", "ja", "en", "zh", "ko", "yue"],
        help="SenseVoice の認識言語 (ReazonSpeech では無視)",
    )
    parser.add_argument("--num-threads", type=int, help="CPU 推論スレッド数")
    parser.add_argument("--provider", help="ONNX Runtime provider (cpu/cuda/coreml など)")
    parser.add_argument(
        "--pre-roll-ms",
        type=int,
        help="押下直前の音声を先頭に含める長さ (ms)",
    )
    parser.add_argument(
        "--min-duration-ms",
        type=int,
        help="これ未満の押下は破棄 (ms)",
    )
    parser.add_argument(
        "--paste-delay-ms",
        type=int,
        help="クリップボード設定から貼り付けまでの待機 (ms)",
    )
    parser.add_argument(
        "--restore-clipboard",
        action=argparse.BooleanOptionalAction,
        default=None,
        help="貼り付け後に元のクリップボードを復元する",
    )
    parser.add_argument(
        "--itn",
        action=argparse.BooleanOptionalAction,
        default=None,
        help="SenseVoice の逆テキスト正規化 (数字・通貨など)",
    )
    parser.add_argument(
        "--list-devices",
        action="store_true",
        help="入力デバイス一覧を表示して終了",
    )
    parser.add_argument("--debug", action="store_true", help="デバッグログを表示")
    return parser


def apply_cli_overrides(config: dict[str, Any], args: argparse.Namespace) -> None:
    if args.model:
        config["model"] = args.model
    if args.hotkey:
        config["hotkey"] = args.hotkey
    if args.paste_modifier:
        config["paste_modifier"] = args.paste_modifier
    if args.device is not None:
        config["device"] = args.device
    if args.language:
        config["language"] = args.language
    if args.num_threads is not None:
        config["num_threads"] = args.num_threads
    if args.provider:
        config["provider"] = args.provider
    if args.pre_roll_ms is not None:
        config["pre_roll_ms"] = args.pre_roll_ms
    if args.min_duration_ms is not None:
        config["min_duration_ms"] = args.min_duration_ms
    if args.paste_delay_ms is not None:
        config["paste_delay_ms"] = args.paste_delay_ms
    if args.restore_clipboard is not None:
        config["restore_clipboard"] = args.restore_clipboard
    if args.itn is not None:
        config["use_itn"] = args.itn


def setup_logging(debug: bool) -> None:
    logging.basicConfig(
        level=logging.DEBUG if debug else logging.INFO,
        format="%(asctime)s [%(levelname)s] %(message)s",
        datefmt="%H:%M:%S",
    )


def main(argv: list[str] | None = None) -> int:
    args = build_arg_parser().parse_args(argv)
    setup_logging(args.debug)

    if args.list_devices:
        return list_input_devices()

    config = dict(CONFIG)
    apply_cli_overrides(config, args)

    try:
        defaults = detect_platform()
        if config["hotkey"] is None:
            config["hotkey"] = defaults.hotkey
        if config["paste_modifier"] is None:
            config["paste_modifier"] = defaults.paste_modifier
        logger.info(
            "プラットフォーム: %s / ホットキー=%s / 貼り付け=%s+%s",
            defaults.label,
            config["hotkey"],
            config["paste_modifier"],
            config["paste_key"],
        )

        engine = build_engine(config, model_dir=args.model_dir)
        platform = PlatformIntegration(defaults, paste_key=str(config["paste_key"]))
        capture = AudioCapture(
            sample_rate=int(config["sample_rate"]),
            ring_seconds=float(config["ring_seconds"]),
            block_size=int(config["block_size"]),
            device=config["device"],
            latency=config["input_latency"],
        )
        app = VoiceInputApp(config, engine, capture, platform)

        capture.start()
        try:
            app.run()
        finally:
            capture.stop()
        return 0
    except VoiceInputError as exc:
        logger.error("%s", exc)
        return 1
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    sys.exit(main())
