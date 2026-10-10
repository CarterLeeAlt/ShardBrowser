# X.Org misc-fixed 点阵字体

这些完整的上游 BDF 文件是 ShardX 浏览器任务栏标签在 32 像素及以下使用的
原生等宽点阵字号。所需的可打印 ASCII 字形由
`scripts/generate_pixel_font_tables.py` 编译为紧凑的 Rust 表；应用从不
解析这些 BDF 文件，也不会在运行时或构建时下载字体。

来源：`freedesktop-unofficial-mirror/xorg__font__misc-misc`（X.Org
`font-misc-misc` 仓库的 GitHub 镜像），`master` 分支：

- `4x6.bdf`，Git blob `ac68ebda533f46af4c747ce2726e303c7e3576ca`
- `5x8.bdf`，Git blob `50637b49e6a393b6f021eba422c06f283bfe35ac`
- `6x10.bdf`，Git blob `c03715f69ff951411b3d8480397bd08916644f4e`
- `7x13.bdf`，Git blob `07db3c6293eacce01bd28d4c8ea418ec2c4e507f`
- `9x15.bdf`，Git blob `68d97093d21da4ac2a864becda54c6e3da554015`

尺寸映射：

- 16px 图标 -> 4x6
- 20px 图标 -> 5x8
- 24px 图标 -> 6x10
- 30px 图标 -> 7x13
- 32px 图标 -> 9x15
- 40px 及以上继续使用内置的 Cascadia Mono TrueType 渲染器

上游 `COPYING` 文件声明："Public domain font. Share and enjoy."
