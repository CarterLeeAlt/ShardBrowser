"""Generate the shared ShardX browser and launcher Windows icons.

The outer artwork has exactly one source of truth:
src-tauri/icons/shardx-browser-taskbar-base.png.

The generated browser-base ICO supplies the exact native outer frame consumed
by Rust before it adds a NAME badge. The launcher uses that same artwork
without an additional overlay. Install Pillow and run this file from any
working directory.
"""

from __future__ import annotations

import io
import struct
from pathlib import Path

from PIL import Image


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

    # Keep the human-viewable launcher master byte-for-byte identical to the
    # canonical browser artwork. The ICO uses the same per-size pixel frames;
    # only its entry order differs because Tauri decodes the first frame.
    LAUNCHER_MASTER_PATH.write_bytes(BASE_PATH.read_bytes())
    launcher_frames = {
        size: base_frames[size].copy()
        for size in WINDOWS_ICON_SIZES
    }
    write_windows_ico(
        ICON_DIR / "icon.ico",
        launcher_frames,
        TAURI_LAUNCHER_ICON_ORDER,
    )


if __name__ == "__main__":
    main()
