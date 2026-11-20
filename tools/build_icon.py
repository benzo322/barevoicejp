#!/usr/bin/env python3
"""BAREVoiceJP のアイコンを SVG から生成する。

デザインの狙い:
  マイク＝音声入力、だけのアイコンは世の中に腐るほどある。そこで
  マイクのグリル（本来は網目）を「声量の波形」に置き換えた。モチーフが
  1 つに統合されるので、要素を足さずに単純すぎなさを出せる。

  フラットさは保ちつつ、面を重ねて奥行きを出す。
    1. 角丸の背景（濃紺）
    2. 一段明るい円（光の当たる面）
    3. 白いマイク＋水色の台座

  サイズで絵を差し替える（Windows の ICO は各サイズに別画像を持てる）:
    - 32px 以上: 波形グリル入りの詳細版
    - 24px 以下: 波形を捨て、カプセルとクレードルを太くした簡略版
  16px に詳細版を縮小すると線が 1px 未満になり、マイクに見えなくなるため。

使い方:
    python tools/build_icon.py
出力:
    icon.svg / icon-small.svg / icon.png (1024) / app.ico (16,32,48,256)
    / favicon.png (32)
"""

from __future__ import annotations

import asyncio
import io
import struct
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
OUT_ICON = ROOT / "icon.png"
OUT_ICO = ROOT / "app.ico"
OUT_SVG = ROOT / "icon.svg"
OUT_SVG_SMALL = ROOT / "icon-small.svg"
OUT_FAVICON = ROOT / "favicon.png"
CHROME = Path.home() / ".cache/ms-playwright/chromium-1234/chrome-linux64/chrome"

SIZE = 1024
ICO_SIZES = (16, 32, 48, 256)
SMALL_MAX = 24

INK = "#0A1322"
LIFT = "#16273F"
SKY = "#38BDF8"
WHITE = "#FFFFFF"

SQUIRCLE = "M512 0C132 0 0 132 0 512s132 512 512 512 512-132 512-512S892 0 512 0Z"

CAP_X, CAP_Y, CAP_W, CAP_H = 426, 196, 172, 380
CAP_CY = CAP_Y + CAP_H / 2

# 簡略版の寸法。カプセルを太く短くし、クレードルと脚を極太にする。
S_CAP_X, S_CAP_Y, S_CAP_W, S_CAP_H = 402, 176, 220, 340
S_CY = S_CAP_Y + S_CAP_H / 2


def wave_bars() -> str:
    """詳細版のグリル。カプセル内に収まる縦バー 5 本。"""
    heights = (120, 188, 236, 188, 120)
    parts = []
    for index, height in enumerate(heights):
        x = CAP_X + 26 + index * 26
        y = CAP_CY - height / 2
        parts.append(
            f'<rect x="{x}" y="{y:.1f}" width="14" height="{height}" rx="7" fill="{SKY}"/>'
        )
    return "".join(parts)


def build_svg() -> str:
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
  <defs>
    <clipPath id="sq"><path d="{SQUIRCLE}"/></clipPath>
    <clipPath id="cap">
      <rect x="{CAP_X}" y="{CAP_Y}" width="{CAP_W}" height="{CAP_H}" rx="{CAP_W // 2}"/>
    </clipPath>
  </defs>

  <path d="{SQUIRCLE}" fill="{INK}"/>
  <g clip-path="url(#sq)">
    <!-- 面 2: 光の当たる円。ベタ塗り 1 枚より奥行きが出る。 -->
    <circle cx="512" cy="474" r="374" fill="{LIFT}"/>
    <circle cx="512" cy="474" r="374" fill="none" stroke="{SKY}"
            stroke-width="3" opacity=".18"/>
  </g>

  <!-- 面 3: マイク。グリルが波形になっている。 -->
  <rect x="{CAP_X}" y="{CAP_Y}" width="{CAP_W}" height="{CAP_H}"
        rx="{CAP_W // 2}" fill="{WHITE}"/>
  <g clip-path="url(#cap)">{wave_bars()}</g>
  <path d="M330 470a182 182 0 0 0 364 0" fill="none" stroke="{WHITE}"
        stroke-width="44" stroke-linecap="round"/>
  <path d="M512 654v66" stroke="{WHITE}" stroke-width="44" stroke-linecap="round"/>
  <path d="M400 724h224" stroke="{SKY}" stroke-width="48" stroke-linecap="round"/>
</svg>"""


def build_svg_small() -> str:
    """24px 以下用。細部を捨て、シルエットだけで読ませる。

    16px では 1024 単位の 1px が 0.016px になる。詳細版のストローク 44 は
    0.7px しかなく、縮小すると線が消えて別物に見える。だから線は 80〜90
    まで太らせ、カプセルも幅 248 まで広げる。

    さらに、内側の円は 16px で背景と同化して「青い丸」に見えるだけなので
    簡略版では省く。クレードルは太らせすぎると三日月になるため、カプセル
    の幅より少し広い程度に抑える。
    """
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
  <defs><clipPath id="sq"><path d="{SQUIRCLE}"/></clipPath></defs>
  <path d="{SQUIRCLE}" fill="{LIFT}"/>

  <!-- カプセル。16px で 4px 弱の幅を確保する。 -->
  <rect x="388" y="150" width="248" height="392" rx="124" fill="{WHITE}"/>
  <!-- グリルは帯 1 本。5 本のバーは 16px で潰れて汚れる。 -->
  <rect x="430" y="300" width="164" height="94" rx="47" fill="{SKY}"/>
  <!-- クレードル。カプセル幅 + 24 に抑える。 -->
  <path d="M300 470a212 212 0 0 0 424 0" fill="none" stroke="{WHITE}"
        stroke-width="86" stroke-linecap="round"/>
  <path d="M512 690v54" stroke="{WHITE}" stroke-width="86" stroke-linecap="round"/>
  <path d="M356 770h312" stroke="{SKY}" stroke-width="96" stroke-linecap="round"/>
</svg>"""


async def rasterize(svg_text: str, out: Path, size: int) -> None:
    from playwright.async_api import async_playwright

    svg = svg_text.replace(
        'width="1024" height="1024"', f'width="{size}" height="{size}"'
    )
    async with async_playwright() as p:
        browser = await p.chromium.launch(executable_path=str(CHROME), args=["--no-sandbox"])
        page = await browser.new_page(
            viewport={"width": size, "height": size}, device_scale_factor=1
        )
        await page.set_content(
            f"<html><body style='margin:0;background:transparent'>{svg}</body></html>"
        )
        await page.wait_for_timeout(100)
        await page.screenshot(path=str(out), omit_background=True)
        await browser.close()


def premultiplied_resize(image: Image.Image, size: int) -> Image.Image:
    """透明部の色がにじまないよう、乗算済みアルファで縮小する。"""
    array = np.asarray(image).astype(np.float32) / 255.0
    alpha = array[:, :, 3:4]
    premultiplied = array[:, :, :3] * alpha
    alpha_image = Image.fromarray(
        np.clip(alpha[:, :, 0] * 255, 0, 255).round().astype(np.uint8), "L"
    )
    color_image = Image.fromarray(
        np.clip(premultiplied * 255, 0, 255).round().astype(np.uint8), "RGB"
    )
    color = np.asarray(
        color_image.resize((size, size), Image.Resampling.LANCZOS)
    ).astype(np.float32) / 255.0
    alpha = np.asarray(
        alpha_image.resize((size, size), Image.Resampling.LANCZOS)
    ).astype(np.float32) / 255.0
    alpha = alpha[:, :, None]
    straightened = np.divide(
        color, alpha, out=np.zeros_like(color), where=alpha > 1e-4
    )
    rgba = np.dstack([straightened, alpha[:, :, 0]])
    return Image.fromarray(
        np.clip(rgba * 255, 0, 255).round().astype(np.uint8), "RGBA"
    )


def bmp_bytes(image: Image.Image) -> bytes:
    """ICO 用の 32bpp DIB (XOR + AND マスク) を作る。"""
    width, height = image.size
    rgba = np.asarray(image)
    xor = np.ascontiguousarray(rgba[::-1, :, [2, 1, 0, 3]])
    stride = ((width + 31) // 32) * 4
    and_mask = bytearray(stride * height)
    for y in range(height):
        for x in range(width):
            if rgba[y, x, 3] < 128:
                and_mask[(height - 1 - y) * stride + x // 8] |= 0x80 >> (x % 8)
    header = struct.pack(
        "<IiiHHIIiiII", 40, width, height * 2, 1, 32, 0,
        xor.nbytes + len(and_mask), 0, 0, 0, 0,
    )
    return header + xor.tobytes() + bytes(and_mask)


def png_bytes(image: Image.Image) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format="PNG", optimize=True)
    return buffer.getvalue()


def write_ico(path: Path, renderings: dict[int, Image.Image]) -> None:
    """サイズごとに別画像を入れた ICO を書く。"""
    entries: list[tuple[int, bytes]] = []
    for size in ICO_SIZES:
        image = renderings[size]
        payload = png_bytes(image) if size >= 256 else bmp_bytes(image)
        entries.append((size, payload))

    offset = 6 + 16 * len(entries)
    header = struct.pack("<HHH", 0, 1, len(entries))
    directory = bytearray()
    for size, payload in entries:
        dimension = 0 if size >= 256 else size
        directory += struct.pack(
            "<BBBBHHII", dimension, dimension, 0, 0, 1, 32, len(payload), offset
        )
        offset += len(payload)
    path.write_bytes(header + directory + b"".join(p for _, p in entries))


async def main() -> None:
    large = build_svg()
    small = build_svg_small()
    OUT_SVG.write_text(large, encoding="utf-8")
    OUT_SVG_SMALL.write_text(small, encoding="utf-8")

    tmp = ROOT / "target" / "icon-render"
    tmp.mkdir(parents=True, exist_ok=True)

    # 32px 以上は詳細版、24px 以下は簡略版を実寸で描く。
    renderings: dict[int, Image.Image] = {}
    for size in ICO_SIZES:
        source = small if size <= SMALL_MAX else large
        raw = tmp / f"render-{size}.png"
        await rasterize(source, raw, size)
        renderings[size] = Image.open(raw).convert("RGBA")

    await rasterize(large, tmp / "render-1024.png", SIZE)
    hero = premultiplied_resize(Image.open(tmp / "render-1024.png").convert("RGBA"), SIZE)
    hero.save(OUT_ICON, format="PNG", optimize=True)

    write_ico(OUT_ICO, renderings)
    favicon = renderings[32]
    favicon.save(OUT_FAVICON, format="PNG", optimize=True)

    print(f"icon.svg:       {OUT_SVG.name}")
    print(f"icon-small.svg: {OUT_SVG_SMALL.name}")
    print(f"icon.png:       {hero.width}x{hero.height}")
    print(f"app.ico:        {', '.join(f'{s}x{s}' for s in ICO_SIZES)}"
          f" (<= {SMALL_MAX}px は簡略版)")
    print(f"favicon:        {OUT_FAVICON.name} ({len(png_bytes(favicon))} bytes)")


if __name__ == "__main__":
    if not CHROME.is_file():
        raise SystemExit(f"Chromium が見つかりません: {CHROME}")
    asyncio.run(main())
