# Bundled application font

`JetBrainsMono-Variable.woff2` is stored directly in this repository and is
bundled by Vite through the local reference in `src/fonts.css`. Building the
application does not fetch fonts from the network.

`src/fonts.css` is the single typography configuration entry point. When the
application font changes, replace the local font asset, update its `@font-face`
metadata there, and update the corresponding license under `public/licenses`.

The current JetBrains Mono file is pinned to upstream commit
`19371302b95d218af43299bce79ddbddd0bc364d`.
