# 应用内置字体

应用使用 Inter。Inter v20 的可变字体 WOFF2 子集已提交在本目录中，由 Vite
通过 `src/fonts.css` 中的本地引用打包；构建和运行 ShardX 从不下载任何
字体。

这些文件取自以下带版本的 Google Fonts 端点：

`https://fonts.gstatic.com/s/inter/v20/`

Inter 采用 SIL Open Font License 1.1 授权。许可证存放在
`public/licenses/Inter-OFL.txt`。

`src/fonts.css` 是唯一的排版配置入口。各 feature 的样式必须引用
`--font-app`，而不是直接指定字体名。应用的两个语义字重保持为 400 和
600。

Windows 任务栏角标使用单独命名、单独打包的
`src-tauri/fonts/CascadiaMono-Regular.ttf`。它是由 Rust 内嵌的静态原生
TrueType 字体；这些 Inter WOFF2 文件仍仅用于前端。
