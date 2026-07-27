"""Generate shared Windows base frames and all ShardX launcher icons.

The outer artwork has exactly one source of truth:
src-tauri/icons/shardx-browser-taskbar-base.png.

The generated browser-base ICO supplies the exact native outer frame consumed
by Rust before it adds a NAME badge.  The launcher is distinguished only by a
compact white play symbol aligned to the browser lens' visual center.  Install
Pillow and run this file from any working directory.
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
LAUNCHER_MASTER_PATH = ICON_DIR / "launcher-icon-master.png"
WINDOWS_ICON_SIZES = (16, 20, 24, 30, 32, 40, 48, 64, 128, 256)
# Tauri 2.11 decodes only the first entry of a configured Windows ICO when it
# constructs the default window icon. Put the native 96-DPI taskbar size first;
# the runtime replaces it with the closest DPI-specific frame immediately after
# the window is created.
TAURI_LAUNCHER_ICON_ORDER = (24, 16, 20, 30, 32, 40, 48, 64, 128, 256)
SUPERSAMPLING = 4
PLAY_WIDTH = 58
PLAY_HEIGHT = 68
PLAY_CORNER_RADIUS = 3


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


def detect_lens_center(base: Image.Image) -> tuple[float, float]:
    """Return the center of the canonical white lens ring in image coordinates."""
    bright_neutral = Image.new("L", base.size, 0)
    source = base.load()
    target = bright_neutral.load()
    for y in range(base.height):
        for x in range(base.width):
            red, green, blue, alpha = source[x, y]
            if (
                alpha >= 200
                and min(red, green, blue) >= 220
                and max(red, green, blue) - min(red, green, blue) <= 30
            ):
                target[x, y] = 255

    bounds = bright_neutral.getbbox()
    if bounds is None:
        raise ValueError("canonical browser base has no detectable white lens ring")

    left, top, right, bottom = bounds
    if not (96 <= right - left <= 160 and 96 <= bottom - top <= 160):
        raise ValueError(f"unexpected white lens ring bounds: {bounds}")

    return ((left + right) / 2, (top + bottom) / 2)


def mask_centroid(mask: Image.Image) -> tuple[float, float]:
    bounds = mask.getbbox()
    if bounds is None:
        raise ValueError("cannot measure an empty mask")

    pixels = mask.load()
    left, top, right, bottom = bounds
    total = 0
    weighted_x = 0
    weighted_y = 0
    for y in range(top, bottom):
        for x in range(left, right):
            value = pixels[x, y]
            total += value
            weighted_x += x * value
            weighted_y += y * value

    return (weighted_x / total, weighted_y / total)


def translated_mask(mask: Image.Image, offset_x: float, offset_y: float) -> Image.Image:
    return mask.transform(
        mask.size,
        Image.Transform.AFFINE,
        (1, 0, -offset_x, 0, 1, -offset_y),
        resample=Image.Resampling.BICUBIC,
        fillcolor=0,
    )


def centered_play_mask(
    size: int,
    lens_center: tuple[float, float],
) -> Image.Image:
    """Draw a rounded play triangle whose alpha centroid matches the lens."""
    canvas_size = size * SUPERSAMPLING
    center = canvas_size / 2
    width = PLAY_WIDTH * size * SUPERSAMPLING / 256
    height = PLAY_HEIGHT * size * SUPERSAMPLING / 256

    # For a right-pointing triangle with two vertices on the left, positioning
    # the left edge one third of its width before the target places its area
    # centroid at the target before rasterization.
    left = center - width / 3
    right = center + width * 2 / 3
    top = center - height / 2
    bottom = center + height / 2

    raw = Image.new("L", (canvas_size, canvas_size), 0)
    ImageDraw.Draw(raw).polygon(
        (
            (round(left), round(top)),
            (round(right), round(center)),
            (round(left), round(bottom)),
        ),
        fill=255,
    )

    corner_radius = PLAY_CORNER_RADIUS * size * SUPERSAMPLING / 256
    if corner_radius > 0:
        raw = raw.filter(ImageFilter.GaussianBlur(corner_radius))
        raw = raw.point(lambda value: 255 if value >= 128 else 0)

    target_x = lens_center[0] * size / 256 - 0.5
    target_y = lens_center[1] * size / 256 - 0.5
    offset_x = 0.0
    offset_y = 0.0
    final = raw.resize((size, size), Image.Resampling.LANCZOS)

    # Downsampling can introduce a fractional centroid shift. Iterate in the
    # supersampled space until the emitted frame, not merely its vector source,
    # is centered to within one hundredth of a pixel.
    for _ in range(6):
        shifted = translated_mask(raw, offset_x, offset_y)
        final = shifted.resize((size, size), Image.Resampling.LANCZOS)
        centroid_x, centroid_y = mask_centroid(final)
        error_x = target_x - centroid_x
        error_y = target_y - centroid_y
        if abs(error_x) <= 0.01 and abs(error_y) <= 0.01:
            break
        offset_x += error_x * SUPERSAMPLING
        offset_y += error_y * SUPERSAMPLING

    centroid_x, centroid_y = mask_centroid(final)
    if abs(centroid_x - target_x) > 0.03 or abs(centroid_y - target_y) > 0.03:
        raise ValueError(
            f"play triangle centroid is not aligned at {size}px: "
            f"({centroid_x:.3f}, {centroid_y:.3f}) vs "
            f"({target_x:.3f}, {target_y:.3f})"
        )
    return final


def masked_color(
    mask: Image.Image,
    color: tuple[int, int, int, int],
) -> Image.Image:
    layer = Image.new("RGBA", mask.size, color)
    if color[3] == 255:
        layer.putalpha(mask)
    else:
        layer.putalpha(mask.point(lambda value: value * color[3] // 255))
    return layer


def launcher_overlay(
    size: int,
    lens_center: tuple[float, float],
) -> Image.Image:
    overlay = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    play = centered_play_mask(size, lens_center)

    # A centered halo preserves the triangle's visual centroid while separating
    # the white glyph from both bright cyan and dark blue parts of the lens.
    halo_radius = max(0.2, 1.4 * size / 256)
    halo = play.filter(ImageFilter.GaussianBlur(halo_radius))
    overlay = Image.alpha_composite(overlay, masked_color(halo, (0, 24, 58, 105)))
    overlay = Image.alpha_composite(overlay, masked_color(play, (250, 250, 248, 255)))
    return overlay


def render_launcher(
    base_frame: Image.Image,
    lens_center: tuple[float, float],
) -> Image.Image:
    size = base_frame.width
    if base_frame.size != (size, size):
        raise ValueError("launcher base frame must be square")
    return Image.alpha_composite(
        base_frame.copy(),
        launcher_overlay(size, lens_center),
    )


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
    lens_center = detect_lens_center(base)

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
        size: render_launcher(base_frames[size], lens_center)
        for size in WINDOWS_ICON_SIZES
    }
    launcher_frames[256].save(
        LAUNCHER_MASTER_PATH,
        format="PNG",
        optimize=True,
    )
    write_windows_ico(
        ICON_DIR / "icon.ico",
        launcher_frames,
        TAURI_LAUNCHER_ICON_ORDER,
    )


if __name__ == "__main__":
    main()
