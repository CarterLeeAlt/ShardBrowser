# Bundled application font

The application uses Inter. Inter v20 variable WOFF2 subsets are committed in
this directory and bundled by Vite through local references in `src/fonts.css`;
building and running ShardX never downloads a font.

The files are sourced from the versioned Google Fonts endpoints under:

`https://fonts.gstatic.com/s/inter/v20/`

Inter is licensed under the SIL Open Font License 1.1. The license is stored at
`public/licenses/Inter-OFL.txt`.

`src/fonts.css` is the single typography configuration entry point. Feature
styles must reference `--font-app` rather than naming a font directly. The
application's two semantic font weights remain 400 and 600.

The Windows taskbar badge uses the separately named and packaged
`src-tauri/fonts/Inter-Variable-GDI.ttf`. It is a native TrueType font embedded
by Rust; these WOFF2 files remain frontend-only.
