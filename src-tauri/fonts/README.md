# Windows 任务栏字体

`CascadiaMono-Regular.ttf` 是静态 TrueType 构建版本，用于 32px 以上的
Windows 任务栏角标帧，并作为旧式非 ASCII 名称的兼容回退。Rust 用
`include_bytes!` 内嵌该字体，并通过 `AddFontMemResourceEx` 为当前进程
注册；它从不会被安装进 Windows，也从不写入系统字体目录。

仓库中的该文件取自 Microsoft 官方 Cascadia Code v2407.24 发行版的
`ttf/static/CascadiaMono-Regular.ttf`。其 SHA-256 为：

`06520d032ec274fa5040b22c6f4a1d829081b24ba40b2da56dae89bf10c7b481`

这一原生 GDI 字体有意与 WebView 的 Inter WOFF2 子集分开维护：

- `src-tauri/fonts/CascadiaMono-Regular.ttf` - 原生 Windows 任务栏标签
- `src-tauri/fonts/pixel-mono/*.bdf` - Public Domain 的 X.Org misc-fixed
  点阵字号，用于原生 16/20/24/30/32px 任务栏标签
- `src/assets/fonts/Inter-Variable-*.woff2` - 打包进前端的排版字体

Cascadia Code 采用 SIL Open Font License 1.1 授权。仓库中的许可证副本
位于 `public/licenses/Cascadia-Code-OFL.txt`。
