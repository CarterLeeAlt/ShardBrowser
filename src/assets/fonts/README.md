# Bundled application font

The application uses Arimo, an open-source, metrically compatible alternative
to Arial. Arimo v36 variable WOFF2 subsets are committed in this directory and
bundled by Vite through local references in `src/fonts.css`; building and
running ShardX never downloads a font.

The files are sourced from the versioned Google Fonts endpoints under:

`https://fonts.gstatic.com/s/arimo/v36/`

Arimo is licensed under the SIL Open Font License 1.1. The license is stored at
`public/licenses/Arimo-OFL.txt`.

`src/fonts.css` is the single typography configuration entry point. Feature
styles must reference `--font-app` rather than naming a font directly. The
application's two semantic font weights remain 400 and 600.

`JetBrainsMono-Variable.woff2` is retained as an unused legacy asset and is not
bundled by Vite.
