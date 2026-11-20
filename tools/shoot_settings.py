#!/usr/bin/env python3
"""設定画面をモック状態で描画して PNG にする（開発用）。

WebView2 の代わりに Chromium で `src/web/settings.html` を開き、
`window.__barevoice.apply()` にダミー状態を流し込んで撮る。
レイアウトの確認と README 用スクリーンショットの生成に使う。

使い方:
    python tools/shoot_settings.py              # 全タブ
    python tools/shoot_settings.py input sound  # 指定タブだけ
出力: target/shots/<tab>.png
"""

import asyncio
import json
import sys
from pathlib import Path

from playwright.async_api import async_playwright

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "target" / "shots"
CHROME = Path.home() / ".cache/ms-playwright/chromium-1234/chrome-linux64/chrome"
VIEWPORT = {"width": 1080, "height": 900}

MOCK = {
    "settings": {
        "first_run": False,
        "model": "sensevoice",
        "hotkey": "alt_r",
        "autostart": True,
        "device": None,
        "language": "ja",
        "num_threads": 4,
        "sound_start": True,
        "sound_finish": True,
        "sound_volume": 0.55,
        "sound_theme": "soft",
        "floating_bar": True,
    },
    "engine": {"ready": True, "building": False, "error": None, "model": "sensevoice",
               "hotwords": False},
    "models": [
        {"id": "sensevoice", "name": "SenseVoice", "subtitle": "日英中韓広東語。既定・推奨",
         "size": "155 MB", "speed": 5, "accuracy": 4, "recommended": True,
         "installed": True, "directoryExists": True, "installedBytes": 162529280,
         "installedLabel": "155 MB",
         "download": {"phase": "idle", "downloaded": 0, "total": 0, "error": None}},
        {"id": "reazonspeech", "name": "ReazonSpeech", "subtitle": "日本語特化。長い発話に強い",
         "size": "680 MB", "speed": 4, "accuracy": 5, "recommended": False,
         "installed": False, "directoryExists": False, "installedBytes": 0,
         "installedLabel": "0 B",
         "download": {"phase": "downloading", "downloaded": 268435456, "total": 713031680,
                      "error": None}},
        {"id": "haas-ja-v1.0", "name": "Haas-ja v1.0", "subtitle": "日本語特化。量子化で省メモリ",
         "size": "480 MB", "speed": 4, "accuracy": 5, "recommended": False,
         "installed": True, "directoryExists": True, "installedBytes": 503316480,
         "installedLabel": "480 MB",
         "download": {"phase": "idle", "downloaded": 0, "total": 0, "error": None}},
        {"id": "whisper-base", "name": "Whisper Base", "subtitle": "軽量。短いメモ向け",
         "size": "198 MB", "speed": 5, "accuracy": 3, "recommended": False,
         "installed": False, "directoryExists": False, "installedBytes": 0,
         "installedLabel": "0 B",
         "download": {"phase": "idle", "downloaded": 0, "total": 0, "error": None}},
        {"id": "whisper-small", "name": "Whisper Small", "subtitle": "速度と精度のバランス",
         "size": "610 MB", "speed": 3, "accuracy": 4, "recommended": False,
         "installed": False, "directoryExists": False, "installedBytes": 0,
         "installedLabel": "0 B",
         "download": {"phase": "idle", "downloaded": 0, "total": 0, "error": None}},
        {"id": "whisper-turbo", "name": "Whisper Turbo", "subtitle": "高精度。マシンパワーが必要",
         "size": "538 MB", "speed": 2, "accuracy": 5, "recommended": False,
         "installed": False, "directoryExists": False, "installedBytes": 0,
         "installedLabel": "0 B",
         "download": {"phase": "idle", "downloaded": 0, "total": 0, "error": None}},
    ],
    "history": [
        {"text": "この設計だと再送時に二重登録される気がする。冪等キーを入れた方がよくない?",
         "time": "今日 14:32", "model": "SenseVoice"},
        {"text": "ごめん、もう一回さっきの話を前提から説明してくれる",
         "time": "今日 13:05", "model": "SenseVoice"},
        {"text": "リトライのバックオフ、指数だと詰まるからジッタ入れて",
         "time": "今日 11:47", "model": "SenseVoice"},
        {"text": "その案でいこう。ただしロールバック手順は先に決めておきたい",
         "time": "昨日 19:22", "model": "Haas-ja v1.0"},
        {"text": "テストが落ちてるのは多分モックの日付固定が原因だと思う",
         "time": "昨日 16:08", "model": "SenseVoice"},
    ],
    "stats": {"totalChars": 184326, "totalSessions": 1284, "todayChars": 6421,
              "estimatedSavedMinutes": 924},
    "defaultDevice": "Microphone (USB Audio)",
    "devices": [
        {"value": "0", "label": "Microphone (USB Audio)", "isDefault": True},
        {"value": "1", "label": "Headset Microphone (Bluetooth)", "isDefault": False},
        {"value": "2", "label": "ステレオ ミキサー", "isDefault": False},
    ],
    "words": [
        {"target": "TypeScript", "reading": "たいぷすくりぷと"},
        {"target": "Supabase", "reading": "すーぱーべーす"},
        {"target": "冪等", "reading": "べきとう"},
        {"target": "GitHub Actions", "reading": "ぎっとはぶあくしょんず"},
    ],
    "level": 0.18,
    "visual": "recording",
    "captureError": None,
    "autostart": True,
    "paths": {
        "words": r"C:\Users\ben\AppData\Roaming\BAREVoiceJP\words.txt",
        "config": r"C:\Users\ben\AppData\Roaming\BAREVoiceJP\config.toml",
        "models": r"C:\Users\ben\AppData\Roaming\BAREVoiceJP\models",
        "logs": r"C:\Users\ben\AppData\Roaming\BAREVoiceJP\logs",
        "modelSource": "公式配布元（sherpa-onnx GitHub / Hugging Face）",
    },
    "version": "0.3.0",
    "update": {"phase": "available", "current": "0.3.0", "latest": "v0.4.0",
               "hasUpdate": True, "downloaded": 0, "total": 0, "error": None},
    "notice": None,
}


async def main(tabs: list[str]) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    html = (ROOT / "src/web/settings.html").read_text(encoding="utf-8")
    js = (ROOT / "src/web/settings.js").read_text(encoding="utf-8")
    # CSP は script-src 'none' なので、撮影用にだけ外す。
    html = html.replace("script-src 'none'", "script-src 'unsafe-inline'")
    html = html.replace("</body>", f"<script>{js}</script></body>")

    async with async_playwright() as p:
        browser = await p.chromium.launch(executable_path=str(CHROME),
                                         args=["--no-sandbox"])
        page = await browser.new_page(viewport=VIEWPORT, device_scale_factor=1)
        await page.goto("about:blank")
        await page.set_content(html, wait_until="load")
        await page.wait_for_timeout(300)
        await page.evaluate(
            "state => { window.__barevoice.apply(state); }",
            json.loads(json.dumps(MOCK)),
        )
        await page.wait_for_timeout(400)
        for tab in tabs:
            await page.evaluate("t => window.__barevoice.openTab(t)", tab)
            await page.wait_for_timeout(500)
            await page.screenshot(path=str(OUT / f"{tab}.png"))
            print(OUT / f"{tab}.png")
        await browser.close()


if __name__ == "__main__":
    requested = sys.argv[1:] or ["input", "models", "dictionary", "history", "sound", "general", "about"]
    asyncio.run(main(requested))
