# Windows taskbar font

`Inter-Variable-GDI.ttf` is the TrueType build of Inter used by the Windows
taskbar badge renderer. Rust embeds it with `include_bytes!` and registers it
for the current process with `AddFontMemResourceEx`; it is never installed into
Windows and does not write to the system font directory.

This native GDI font is intentionally kept separate from the WebView WOFF2
subsets in `src/assets/fonts/Inter-Variable-*.woff2`:

- `src-tauri/fonts/Inter-Variable-GDI.ttf` — native Windows taskbar rendering
- `src/assets/fonts/Inter-Variable-*.woff2` — bundled frontend typography

The font comes from the Google Fonts Inter v20 distribution and is licensed
under the SIL Open Font License 1.1. The repository copy of that license is at
`public/licenses/Inter-OFL.txt`.
