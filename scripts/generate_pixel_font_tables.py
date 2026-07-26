"""Generate the Rust taskbar bitmap-font tables from bundled X.Org BDF files.

The generated module contains only printable ASCII (U+0020..U+007E), which is
the complete alphabet accepted by ShardX profile names plus a safe `?` glyph.
No font parsing or network access happens in the shipped application.
"""

from __future__ import annotations

import argparse
import hashlib
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FONT_DIR = ROOT / "src-tauri" / "fonts" / "pixel-mono"
OUTPUT = ROOT / "src-tauri" / "src" / "pixel_font_data.rs"
ASCII_FIRST = 0x20
ASCII_LAST = 0x7E
MAX_GLYPH_ROWS = 15


@dataclass(frozen=True)
class FontSpec:
    icon_size: int
    file_name: str
    constant: str
    width: int
    height: int
    fixed_y_offset: int


FONT_SPECS = (
    FontSpec(16, "4x6.bdf", "FONT_4X6", 4, 6, 1),
    FontSpec(20, "5x8.bdf", "FONT_5X8", 5, 8, 0),
    FontSpec(24, "6x10.bdf", "FONT_6X10", 6, 10, 1),
    FontSpec(30, "7x13.bdf", "FONT_7X13", 7, 13, 0),
    FontSpec(32, "9x15.bdf", "FONT_9X15", 9, 15, 1),
)


def parse_bdf(path: Path, spec: FontSpec) -> dict[int, tuple[int, ...]]:
    glyphs: dict[int, tuple[int, ...]] = {}
    current: dict[str, object] | None = None
    in_bitmap = False

    for line_number, raw_line in enumerate(
        path.read_text(encoding="ascii").splitlines(), start=1
    ):
        line = raw_line.strip()
        if line.startswith("STARTCHAR "):
            current = {"rows": []}
            in_bitmap = False
            continue
        if current is None:
            continue
        if line.startswith("ENCODING "):
            current["encoding"] = int(line.split()[1])
        elif line.startswith("DWIDTH "):
            parts = line.split()
            current["dwidth"] = (int(parts[1]), int(parts[2]))
        elif line.startswith("BBX "):
            parts = line.split()
            current["bbx"] = tuple(int(value) for value in parts[1:5])
        elif line == "BITMAP":
            in_bitmap = True
        elif line == "ENDCHAR":
            encoding = int(current.get("encoding", -1))
            if ASCII_FIRST <= encoding <= ASCII_LAST:
                dwidth = current.get("dwidth")
                bbx = current.get("bbx")
                raw_rows = current["rows"]
                if dwidth != (spec.width, 0):
                    raise ValueError(
                        f"{path.name}:{line_number}: U+{encoding:04X} has "
                        f"DWIDTH {dwidth}, expected ({spec.width}, 0)"
                    )
                if not isinstance(bbx, tuple) or bbx[:2] != (spec.width, spec.height):
                    raise ValueError(
                        f"{path.name}:{line_number}: U+{encoding:04X} has "
                        f"BBX {bbx}, expected {spec.width}x{spec.height}"
                    )
                if len(raw_rows) != spec.height:
                    raise ValueError(
                        f"{path.name}:{line_number}: U+{encoding:04X} has "
                        f"{len(raw_rows)} rows, expected {spec.height}"
                    )

                storage_bits = ((spec.width + 7) // 8) * 8
                shift = storage_bits - spec.width
                normalized = tuple(int(row, 16) >> shift for row in raw_rows)
                limit = 1 << spec.width
                if any(row >= limit for row in normalized):
                    raise ValueError(
                        f"{path.name}:{line_number}: U+{encoding:04X} exceeds cell width"
                    )
                glyphs[encoding] = normalized
            current = None
            in_bitmap = False
        elif in_bitmap:
            current["rows"].append(line)

    expected = set(range(ASCII_FIRST, ASCII_LAST + 1))
    missing = sorted(expected.difference(glyphs))
    if missing:
        listed = ", ".join(f"U+{value:04X}" for value in missing)
        raise ValueError(f"{path.name} is missing printable ASCII glyphs: {listed}")
    return glyphs


def rust_character_comment(codepoint: int) -> str:
    character = chr(codepoint)
    if character == " ":
        character = "space"
    elif character == "\\":
        character = "backslash"
    elif character == "\t":
        character = "tab"
    return f"U+{codepoint:04X} {character}"


def render_font(spec: FontSpec, glyphs: dict[int, tuple[int, ...]]) -> list[str]:
    lines = [
        f"static {spec.constant}: [[u16; MAX_GLYPH_ROWS]; GLYPH_COUNT] = ["
    ]
    for codepoint in range(ASCII_FIRST, ASCII_LAST + 1):
        rows = list(glyphs[codepoint])
        rows.extend([0] * (MAX_GLYPH_ROWS - len(rows)))
        rendered = ", ".join(f"0b{row:0{spec.width}b}" for row in rows)
        lines.append(f"    [{rendered}], // {rust_character_comment(codepoint)}")
    lines.append("];\n")
    return lines


def generate() -> str:
    parsed: list[tuple[FontSpec, dict[int, tuple[int, ...]], str]] = []
    for spec in FONT_SPECS:
        path = FONT_DIR / spec.file_name
        contents = path.read_bytes()
        parsed.append((spec, parse_bdf(path, spec), hashlib.sha256(contents).hexdigest()))

    lines = [
        "// @generated by scripts/generate_pixel_font_tables.py; do not edit by hand.",
        "// Sources are the Public Domain X.Org misc-fixed BDF files in",
        "// src-tauri/fonts/pixel-mono/.",
    ]
    for spec, _, digest in parsed:
        lines.append(f"// SHA-256 {spec.file_name}: {digest}")
    lines.extend(
        [
            "",
            f"pub(crate) const MAX_GLYPH_ROWS: usize = {MAX_GLYPH_ROWS};",
            f"const ASCII_FIRST: u32 = 0x{ASCII_FIRST:02X};",
            f"const ASCII_LAST: u32 = 0x{ASCII_LAST:02X};",
            f"const GLYPH_COUNT: usize = {ASCII_LAST - ASCII_FIRST + 1};",
            "pub(crate) const PIXEL_ICON_SIZES: [i32; 5] = [16, 20, 24, 30, 32];",
            "",
            "#[derive(Clone, Copy)]",
            "pub(crate) struct PixelFont {",
            "    pub(crate) cell_width: i32,",
            "    pub(crate) cell_height: i32,",
            "    pub(crate) fixed_y_offset: i32,",
            "    glyphs: &'static [[u16; MAX_GLYPH_ROWS]; GLYPH_COUNT],",
            "}",
            "",
            "impl PixelFont {",
            "    pub(crate) fn glyph(self, character: char) -> Option<&'static [u16; MAX_GLYPH_ROWS]> {",
            "        let codepoint = character as u32;",
            "        if !(ASCII_FIRST..=ASCII_LAST).contains(&codepoint) {",
            "            return None;",
            "        }",
            "        self.glyphs.get((codepoint - ASCII_FIRST) as usize)",
            "    }",
            "}",
            "",
            "pub(crate) fn for_icon_size(icon_size: i32) -> Option<PixelFont> {",
            "    if !PIXEL_ICON_SIZES.contains(&icon_size) {",
            "        return None;",
            "    }",
            "    match icon_size {",
        ]
    )
    for spec, _, _ in parsed:
        lines.append(
            f"        {spec.icon_size} => Some(PixelFont {{ cell_width: {spec.width}, "
            f"cell_height: {spec.height}, fixed_y_offset: {spec.fixed_y_offset}, "
            f"glyphs: &{spec.constant} }}),"
        )
    lines.extend(["        _ => None,", "    }", "}", ""])
    for spec, glyphs, _ in parsed:
        lines.extend(render_font(spec, glyphs))
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if the committed Rust table does not match the BDF sources",
    )
    args = parser.parse_args()
    generated = generate()
    if args.check:
        current = OUTPUT.read_text(encoding="utf-8") if OUTPUT.exists() else ""
        if current != generated:
            raise SystemExit(
                f"{OUTPUT.relative_to(ROOT)} is stale; run {Path(__file__).name}"
            )
        print(f"verified {OUTPUT.relative_to(ROOT)}")
        return
    OUTPUT.write_text(generated, encoding="utf-8", newline="\n")
    print(f"wrote {OUTPUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
