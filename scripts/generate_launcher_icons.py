"""Generate shared Windows base frames and all ShardX launcher icons.

The outer artwork has exactly one source of truth:
src-tauri/icons/shardx-browser-taskbar-base.png.

The generated browser-base ICO supplies the exact native outer frame consumed
by Rust before it adds a NAME badge.  The launcher is distinguished only by
the compact E-style diamond rendered by this script.  Install Pillow and run
this file from any working directory.
"""

from __future__ import annotations

import io
import struct
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter


ROOT = Path(__file__).resolve().parents[1]
ICON_DIR = ROOT / "src-tauri" / "icons"
BASE_PATH = ICON_DIR / "shardx-browser-taskbar-base.png"
BASE_ICO_PATH = ICON_DIR / "shardx-browser-taskbar-base.ico"
WINDOWS_ICON_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
# Tauri 2.11 decodes only the first entry of a configured Windows ICO when it
# constructs the default window icon. Put the native 96-DPI taskbar size first;
# the runtime replaces it with the closest DPI-specific frame immediately after
# the window is created.
TAURI_LAUNCHER_ICON_ORDER = (24, 16, 20, 32, 40, 48, 64, 128, 256)
SUPERSAMPLING = 4


def assert_canonical_base(base: Image.Image) -> None:
    if base.size != (256, 256):
        raise ValueError("canonical browser base must be exactly 256x256")

    alpha = base.getchannel("A")
    if alpha.getbbox() != (0, 0, 256, 256):
        raise ValueError("canonical browser base contains transparent outer padding")

    edges = (
        [alpha.getpixel((x, 0)) for x in range(256)],
        [alpha.getpixel((x, 255)) for x in range(256)],
        [alpha.getpixel((0, y)) for y in range(256)],
        [alpha.getpixel((255, y)) for y in range(256)],
    )
    if any(max(edge) < 250 for edge in edges):
        raise ValueError("canonical browser artwork does not visibly reach every edge")


def scaled(value_at_256: float, size: int) -> int:
    return round(value_at_256 * size * SUPERSAMPLING / 256)


def diamond_points(size: int, radius: int, center_y: int = 122) -> list[tuple[int, int]]:
    center_x = 128
    return [
        (scaled(center_x, size), scaled(center_y - radius, size)),
        (scaled(center_x + radius, size), scaled(center_y, size)),
        (scaled(center_x, size), scaled(center_y + radius, size)),
        (scaled(center_x - radius, size), scaled(center_y, size)),
    ]


def diamond_mask(size: int, radius: int, center_y: int = 122, blur: float = 0) -> Image.Image:
    canvas_size = size * SUPERSAMPLING
    mask = Image.new("L", (canvas_size, canvas_size), 0)
    ImageDraw.Draw(mask).polygon(diamond_points(size, radius, center_y), fill=255)
    if blur:
        mask = mask.filter(
            ImageFilter.GaussianBlur(blur * size * SUPERSAMPLING / 256)
        )
    return mask


def masked_color(size: int, color: tuple[int, int, int, int], mask: Image.Image) -> Image.Image:
    layer = Image.new(
        "RGBA",
        (size * SUPERSAMPLING, size * SUPERSAMPLING),
        color,
    )
    layer.putalpha(mask)
    return layer


def diagonal_face_gradient(size: int, mask: Image.Image) -> Image.Image:
    canvas_size = size * SUPERSAMPLING
    gradient = Image.new("RGBA", (canvas_size, canvas_size))
    draw = ImageDraw.Draw(gradient)
    start = (48, 229, 216, 255)
    end = (0, 82, 161, 255)
    for y in range(canvas_size):
        amount = y / max(canvas_size - 1, 1)
        color = tuple(
            round(start[channel] * (1 - amount) + end[channel] * amount)
            for channel in range(4)
        )
        draw.line((0, y, canvas_size, y), fill=color)
    gradient = gradient.rotate(45, resample=Image.Resampling.BICUBIC, expand=False)
    gradient.putalpha(mask)
    return gradient


def launcher_overlay(size: int) -> Image.Image:
    canvas_size = size * SUPERSAMPLING
    overlay = Image.new("RGBA", (canvas_size, canvas_size), (0, 0, 0, 0))

    # Cover the browser lens with the launcher's dark optical plate.  This is
    # the only central change; the canonical outer swirl remains untouched.
    plate = Image.new("RGBA", overlay.size, (0, 0, 0, 0))
    plate_draw = ImageDraw.Draw(plate)
    plate_draw.ellipse(
        (
            scaled(60, size),
            scaled(54, size),
            scaled(196, size),
            scaled(190, size),
        ),
        fill=(4, 30, 49, 245),
    )
    plate = plate.filter(
        ImageFilter.GaussianBlur(0.35 * size * SUPERSAMPLING / 256)
    )
    overlay = Image.alpha_composite(overlay, plate)

    shadow = diamond_mask(size, 66, center_y=126, blur=4)
    shadow = shadow.point(lambda value: value * 150 // 255)
    overlay = Image.alpha_composite(
        overlay,
        masked_color(size, (0, 0, 0, 150), shadow),
    )

    outer = diamond_mask(size, 62)
    overlay = Image.alpha_composite(
        overlay,
        masked_color(size, (111, 39, 235, 255), outer),
    )

    inner = diamond_mask(size, 43)
    overlay = Image.alpha_composite(
        overlay,
        masked_color(size, (25, 5, 77, 255), inner),
    )

    face = diamond_mask(size, 34)
    overlay = Image.alpha_composite(overlay, diagonal_face_gradient(size, face))

    highlight = ImageDraw.Draw(overlay, "RGBA")
    highlight.line(
        (
            (scaled(128, size), scaled(60, size)),
            (scaled(66, size), scaled(122, size)),
        ),
        fill=(209, 161, 255, 255),
        width=max(1, scaled(2, size)),
    )

    return overlay.resize((size, size), Image.Resampling.LANCZOS)


def render_launcher(base_frame: Image.Image) -> Image.Image:
    size = base_frame.width
    if base_frame.size != (size, size):
        raise ValueError("launcher base frame must be square")
    return Image.alpha_composite(base_frame.copy(), launcher_overlay(size))


def png_bytes(image: Image.Image) -> bytes:
    output = io.BytesIO()
    image.save(output, format="PNG", optimize=True)
    return output.getvalue()


def write_windows_ico(
    path: Path,
    frames: dict[int, Image.Image],
    order: tuple[int, ...],
) -> None:
    encoded_frames = [(size, png_bytes(frames[size])) for size in order]
    header_size = 6 + 16 * len(encoded_frames)
    offset = header_size
    directory = []
    payload = []
    for size, data in encoded_frames:
        encoded_size = 0 if size == 256 else size
        directory.append(
            struct.pack(
                "<BBBBHHII",
                encoded_size,
                encoded_size,
                0,
                0,
                1,
                32,
                len(data),
                offset,
            )
        )
        payload.append(data)
        offset += len(data)

    path.write_bytes(
        struct.pack("<HHH", 0, 1, len(encoded_frames))
        + b"".join(directory)
        + b"".join(payload)
    )


def main() -> None:
    base = Image.open(BASE_PATH).convert("RGBA")
    assert_canonical_base(base)

    # These are the canonical per-size outer frames shared byte-for-byte by
    # the launcher and browser icon pipelines.
    base_frames = {
        size: base.copy()
        if size == 256
        else base.resize((size, size), Image.Resampling.LANCZOS)
        for size in WINDOWS_ICON_SIZES
    }
    write_windows_ico(BASE_ICO_PATH, base_frames, WINDOWS_ICON_SIZES)

    launcher_frames = {
        size: render_launcher(base_frames[size]) for size in WINDOWS_ICON_SIZES
    }
    source = render_launcher(base.resize((1024, 1024), Image.Resampling.LANCZOS))
    source.save(ICON_DIR / "icon-source.png", optimize=True)
    launcher_frames[32].save(ICON_DIR / "32x32.png", optimize=True)
    launcher_frames[128].save(ICON_DIR / "128x128.png", optimize=True)
    launcher_frames[256].save(ICON_DIR / "128x128@2x.png", optimize=True)
    write_windows_ico(
        ICON_DIR / "icon.ico",
        launcher_frames,
        TAURI_LAUNCHER_ICON_ORDER,
    )

    source.save(
        ICON_DIR / "icon.icns",
        format="ICNS",
        sizes=[
            (16, 16),
            (32, 32),
            (64, 64),
            (128, 128),
            (256, 256),
            (512, 512),
            (1024, 1024),
        ],
    )


if __name__ == "__main__":
    main()
