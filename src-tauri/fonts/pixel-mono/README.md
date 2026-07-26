# X.Org misc-fixed bitmap fonts

These complete upstream BDF files are the native monospaced bitmap strikes used
for ShardX browser taskbar labels at 32 pixels and below. The required
printable-ASCII glyphs are compiled into compact Rust tables by
`scripts/generate_pixel_font_tables.py`; the application never parses the BDF
files and never downloads fonts at runtime or build time.

Source: `freedesktop-unofficial-mirror/xorg__font__misc-misc` (the GitHub mirror
of X.Org's `font-misc-misc` repository), `master` branch:

- `4x6.bdf`, Git blob `ac68ebda533f46af4c747ce2726e303c7e3576ca`
- `5x8.bdf`, Git blob `50637b49e6a393b6f021eba422c06f283bfe35ac`
- `6x10.bdf`, Git blob `c03715f69ff951411b3d8480397bd08916644f4e`
- `7x13.bdf`, Git blob `07db3c6293eacce01bd28d4c8ea418ec2c4e507f`
- `9x15.bdf`, Git blob `68d97093d21da4ac2a864becda54c6e3da554015`

Size mapping:

- 16px icon -> 4x6
- 20px icon -> 5x8
- 24px icon -> 6x10
- 30px icon -> 7x13
- 32px icon -> 9x15
- 40px and above keep the bundled Cascadia Mono TrueType renderer

The upstream `COPYING` file states: "Public domain font. Share and enjoy."
