#!/usr/bin/env python3
"""icon.jpg のチェッカー背景を透過し、icon.png / app.ico / favicon を生成する。

背景は白 (#ffffff) と薄いグレー (#d0d0d0) のチェッカーで、どちらも
アイコンの外側だけを透過する。角丸の縁は近傍の背景色とアイコン色から
アルファを復元し、縮小後の白フチを防ぐ。

使い方:
    python tools/make_icon.py            # 余白を切り詰めて透過 (既定)
    python tools/make_icon.py --no-trim  # 元の余白を残して透過
"""

from __future__ import annotations

import argparse
import base64
import io
import struct
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
ICO_SIZES = (16, 32, 48, 256)
ICON_SIZE = 1024
FAVICON_SIZE = 32


def background_mask(rgb: np.ndarray) -> np.ndarray:
    """外周とつながっている白・グレーの画素を背景として返す。"""
    values = rgb.astype(np.int16)
    chroma = values.max(axis=2) - values.min(axis=2)
    candidate = (chroma <= 20) & (values.min(axis=2) >= 190)
    mask = Image.fromarray((candidate * 255).astype(np.uint8), "L").copy()
    width, height = mask.size
    seeds = [(x, 0) for x in range(width)]
    seeds += [(x, height - 1) for x in range(width)]
    seeds += [(0, y) for y in range(1, height - 1)]
    seeds += [(width - 1, y) for y in range(1, height - 1)]
    for seed in seeds:
        if mask.getpixel(seed) == 255:
            ImageDraw.floodfill(mask, seed, 128, thresh=0)
    return np.asarray(mask) == 128


def dilate(mask: np.ndarray, iterations: int) -> np.ndarray:
    result = mask.copy()
    for _ in range(iterations):
        padded = np.pad(result, 1, constant_values=False)
        result = (
            padded[1:-1, 1:-1]
            | padded[:-2, 1:-1]
            | padded[2:, 1:-1]
            | padded[1:-1, :-2]
            | padded[1:-1, 2:]
            | padded[:-2, :-2]
            | padded[:-2, 2:]
            | padded[2:, :-2]
            | padded[2:, 2:]
        )
    return result


def nearest(mask: np.ndarray, y: int, x: int, radius: int) -> tuple[int, int] | None:
    """指定画素から最も近い True 画素をチェビシェフ距離で探す。"""
    height, width = mask.shape
    for distance in range(1, radius + 1):
        top = max(0, y - distance)
        bottom = min(height - 1, y + distance)
        left = max(0, x - distance)
        right = min(width - 1, x + distance)
        for xx in range(left, right + 1):
            for yy in (top, bottom):
                if 0 <= yy < height and mask[yy, xx]:
                    return yy, xx
        for yy in range(top + 1, bottom):
            for xx in (left, right):
                if 0 <= xx < width and mask[yy, xx]:
                    return yy, xx
    return None


def solve_alpha(pixel: np.ndarray, background: np.ndarray, icon: np.ndarray) -> float:
    delta = icon - background
    denominator = float(np.dot(delta, delta))
    if denominator < 1e-6:
        return 1.0
    value = float(np.dot(pixel - background, delta) / denominator)
    return min(1.0, max(0.0, value))


def matte(source: Image.Image) -> Image.Image:
    rgb = np.asarray(source.convert("RGB"))
    height, width = rgb.shape[:2]
    background = background_mask(rgb)
    band = dilate(background, 6) & ~background
    core = ~dilate(background, 6)

    rgba = np.dstack([rgb, np.full((height, width), 255, np.uint8)])
    dominant = np.median(rgb[core].reshape(-1, 3), axis=0)
    rgba[background, :3] = dominant.astype(np.uint8)
    rgba[background, 3] = 0

    for y, x in zip(*np.nonzero(band), strict=True):
        icon_point = nearest(core, int(y), int(x), 14)
        if icon_point is None:
            continue
        background_point = nearest(background, int(y), int(x), 8)
        if background_point is None:
            continue
        icon = rgb[icon_point].astype(np.float64)
        back = rgb[background_point].astype(np.float64)
        alpha = solve_alpha(rgb[y, x].astype(np.float64), back, icon)
        rgba[y, x, :3] = icon.astype(np.uint8)
        rgba[y, x, 3] = round(alpha * 255)
    return Image.fromarray(rgba, "RGBA")


def trim(image: Image.Image, padding: float = 0.04) -> Image.Image:
    alpha = np.asarray(image)[:, :, 3]
    ys, xs = np.nonzero(alpha > 8)
    if len(xs) == 0:
        return image
    left, right = int(xs.min()), int(xs.max()) + 1
    top, bottom = int(ys.min()), int(ys.max()) + 1
    center_x = (left + right) / 2
    center_y = (top + bottom) / 2
    side = round(max(right - left, bottom - top) * (1 + 2 * padding))
    half = side // 2
    return image.crop(
        (round(center_x) - half, round(center_y) - half,
         round(center_x) - half + side, round(center_y) - half + side)
    )


def resize(image: Image.Image, size: int) -> Image.Image:
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


def png_bytes(image: Image.Image) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format="PNG", optimize=True)
    return buffer.getvalue()


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
        "<IiiHHIIiiII",
        40,
        width,
        height * 2,
        1,
        32,
        0,
        xor.nbytes + len(and_mask),
        0,
        0,
        0,
        0,
    )
    return header + xor.tobytes() + bytes(and_mask)


def write_ico(path: Path, image: Image.Image) -> None:
    entries: list[tuple[int, bytes]] = []
    for size in ICO_SIZES:
        resized = resize(image, size)
        payload = png_bytes(resized) if size >= 256 else bmp_bytes(resized)
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
    path.write_bytes(header + directory + b"".join(payload for _, payload in entries))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--trim",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="透過した外周を切り詰める (--no-trim で無効)",
    )
    parser.add_argument("--source", default=str(ROOT / "icon.jpg"))
    parser.add_argument("--output-dir", default=str(ROOT))
    args = parser.parse_args()

    output = Path(args.output_dir)
    source = Image.open(args.source)
    icon = matte(source)
    if args.trim:
        icon = trim(icon)
    icon = resize(icon, ICON_SIZE)

    output.mkdir(parents=True, exist_ok=True)
    icon.save(output / "icon.png", format="PNG", optimize=True)
    write_ico(output / "app.ico", icon)

    favicon = resize(icon, FAVICON_SIZE)
    favicon_path = output / "favicon.png"
    favicon.save(favicon_path, format="PNG", optimize=True)
    print(f"icon.png: {icon.width}x{icon.height}")
    print(f"app.ico: {', '.join(f'{size}x{size}' for size in ICO_SIZES)}")
    print(f"favicon: {favicon_path} ({len(base64.b64encode(favicon_path.read_bytes()))} bytes base64)")


if __name__ == "__main__":
    main()
