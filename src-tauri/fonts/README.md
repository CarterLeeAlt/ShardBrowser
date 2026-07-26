# Windows taskbar fonts

`CascadiaMono-Regular.ttf` is the static TrueType build used by Windows
taskbar badge frames above 32px and as a compatibility fallback for legacy
non-ASCII names. Rust embeds it with `include_bytes!` and registers it for the
current process with `AddFontMemResourceEx`; it is never installed into Windows
and never writes to the system font directory.

The repository file is `ttf/static/CascadiaMono-Regular.ttf` from Microsoft's
official Cascadia Code v2407.24 release. Its SHA-256 is:

`06520d032ec274fa5040b22c6f4a1d829081b24ba40b2da56dae89bf10c7b481`

This native GDI font is intentionally separate from the WebView Inter WOFF2
subsets:

- `src-tauri/fonts/CascadiaMono-Regular.ttf` - native Windows taskbar labels
- `src-tauri/fonts/pixel-mono/*.bdf` - Public Domain X.Org misc-fixed bitmap
  strikes for native 16/20/24/30/32px taskbar labels
- `src/assets/fonts/Inter-Variable-*.woff2` - bundled frontend typography

Cascadia Code is licensed under the SIL Open Font License 1.1. The repository
copy of the license is at `public/licenses/Cascadia-Code-OFL.txt`.
