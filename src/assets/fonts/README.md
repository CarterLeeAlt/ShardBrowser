# 应用内置字体

应用英文与数字用 Inter，简体中文用 Noto Sans SC。字体均从仓库中的可变 WOFF2 文件打包，不从系统字体或网络指定字体来源。

这些文件取自以下带版本的 Google Fonts 端点：

`https://fonts.gstatic.com/s/inter/v20/`

Inter 采用 SIL Open Font License 1.1 授权。许可证存放在
`public/licenses/Inter-OFL.txt`。

`NotoSansSC-Variable.woff2` 由 [Google Fonts 源文件](https://github.com/google/fonts/tree/2894aab31764f10f29c421bdfd2340d3b382d384/ofl/notosanssc) 无损转换为 WOFF2，保留原字库和可变字重。授权见 `public/licenses/NotoSansSC-OFL.txt`。

`src/fonts.css` 是唯一的排版配置入口。各 feature 的样式必须引用
`--font-app`，而不是直接指定字体名。应用的两个语义字重保持为 400 和
600。

Windows 任务栏角标使用单独命名、单独打包的
`src-tauri/fonts/CascadiaMono-Regular.ttf`。它是由 Rust 内嵌的静态原生
TrueType 字体；这些 Inter WOFF2 文件仍仅用于前端。
