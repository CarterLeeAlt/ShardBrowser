# ShardX Launcher

## 本仓库相对原项目的主要改进

本仓库基于 [ProxyShard/ShardBrowser](https://github.com/ProxyShard/ShardBrowser)
持续维护，保留原项目的 Chromium 指纹能力、自动化 API、MCP 与多语言 SDK，
并重点补强了 Windows 便携使用、数据安全、代理测试、会话稳定性和批量管理体验。
完整的项目说明见下方。

* **严格的 Windows x64 便携化** — 使用无需安装的便携 EXE，运行时、配置、
  浏览器资料、Cookie、导出文件和 MCP 下载均保存在程序旁的
  `shardx-launcher` 目录；启动时会校验安装路径及目录可写性，避免静默写入
  系统用户目录，也不再生成 MSI 安装包。
* **更可靠的 Runtime 生命周期** — 首次缺少 Runtime 时自动安装；已有
  Runtime 会在启动后及每小时自动比较远端版本与 ETag，但绝不在后台下载、
  安装或替换文件；界面会显示上次检查结果和时间，实际更新与修复仍必须由用户
  主动触发。安装过程采用暂存、完整性校验、原子切换和失败回滚，并同步强化了
  Node、Python、Rust SDK 与 MCP 的下载和依赖安全。
  引擎更新与启动、编辑、备份共用占用保护；下载期间释放全局资源锁。指纹模板的
  包装格式和内部配置一起迁移，迁移失败会报告具体文件并阻止启动，执行“修复”后重试。
* **代理管理与测试重构** — 支持 SOCKS5、HTTP、HTTPS 批量导入、去重和输入
  顺序保持；新增内容统一追加到列表末尾。单个坏代理不会阻塞其他结果，批量测试
  会逐条刷新 UI，所有 TCP、UDP 和 Geo-IP 测试统一使用 5 秒超时；手动测试拥有
  优先通道，定时自动测试不会挤占它。Geo-IP 支持六个服务、可配置首选服务，
  首选查询失败或缺少城市、时区时，会在 5 秒预算内并行补查备用服务，采用同一
  出口 IP 的完整地理信息；到时仍不完整则保留有效结果。同 IP 复测会刷新地理
  信息，保留首次出现时间，并避免用不完整结果覆盖已知城市与时区。
  浏览器启动的代理预检共用 5 秒总预算：先检查代理端口，再并行查询地理信息和
  出口连通性；仅 WebRTC 自动模式检查 UDP。确认失败立即取消启动并解除界面
  启动状态，图标与浏览器文件在预检通过后准备。
  启动中再次点击行内启动按钮可取消：网络等待立即中断，释放启动占用；批量启动
  支持取消正在预检和仍在队列中的环境。进程已开始创建时，可在运行后使用停止按钮。
  未绑定代理的"直连"环境启动时会显式加 `--no-proxy-server`，完全绕过
  系统代理（例如 Clash 的系统代理模式），确保真正的直连出口。
* **浏览器会话与账号保护** — 浏览器正常关闭时等待其完整退出，避免粗暴终止
  损坏登录态；存在运行中的浏览器时阻止误退出启动器，并禁止修改其配置或代理。
  CDP 环境通过 `Browser.close` 正常关闭，无头模式同样适用；等待 20 秒仍未退出时
  返回错误并保留进程跟踪，可再次尝试。启动器重启后核对进程身份并恢复 CDP；
  无法核实的进程保持保护，恢复失败次数落盘，界面提供诊断与“重试恢复”。
  每个浏览器还会锁定已验证的代理网络身份；同一绑定发生国家或时区跳变时不再
  直接阻止启动，而是把环境重新锚定到新出口并给出显著警告。只有代理出口完全
  不可达时才取消启动；geo 查询失败但出口存活时会保留锁定身份继续启动并提示。
* **更低的指纹重复风险** — 新建浏览器优先使用尚未使用或使用次数最少的指纹
  模板；Canvas 与 WebGL 默认启用稳定的每配置噪声，ClientRects、Audio、
  Sensors 和 Fonts 默认保持真实值。新建与克隆时会检测有效指纹碰撞并重新生成
  唯一种子，同时普通编辑和备份恢复不会擅自改变已有浏览器指纹。
  新建与克隆还会校正平台版本与 Apple 芯片代际，并按已安装引擎的元数据同步
  Chrome、GREASE、TLS 参数及适用的 Apple WebGPU 特性。平台版本、TLS 和 GPU
  一致性修正仅作用于新身份；已有环境仍只在引擎更新流程中同步 Chrome 与 GREASE。
* **环境配置、Cookie 与浏览器数据备份** — 关键 JSON 使用原子写入并保留 `.bak`
  恢复副本；Cookie 导入采用原子替换。环境备份包含指纹配置、浏览器持久化数据、
  绑定代理和 Chromium 加密密钥。导出仅排除已知的网页、代码、GPU/着色器缓存及
  根目录的运行锁、崩溃报告和统计文件，筛选不会清理源环境；Cookie、密码、网站存储
  （含 IndexedDB、Service Worker、CacheStorage 和网站文件存储）、扩展数据、
  设置、书签、历史和已落盘的标签页会话均保留。未知目录默认保留，缓存筛选限定
  在用户数据根目录与 Local State 登记的浏览器资料目录（含 Default），不会按
  任意深度的同名目录过滤。恢复后按需重建缓存，首次访问可能需要重新下载资源。
  导入前会校验清单、大小、路径和内容，失败时回滚；恢复保留已备份的登录资料与
  指纹身份，网站服务端已过期或撤销的会话仍需重新登录。**注意：备份内含
  浏览器 Cookie 解密密钥——任何拿到该文件的人都能以这些账号身份登录，请离线
  妥善保存，切勿上传或分发给不可信对象。**
  v4 备份还包含所选扩展库文件及固定扩展 ID，恢复时一并还原；同 ID 扩展内容
  不同则报告冲突，保留现有版本。v2、v3、v4 备份均可读；导入按原归档清单恢复，
  旧备份中已有的缓存文件也会还原。
  Cookie JSON 往返保留分区键、跨站祖先标记、来源协议/端口/类型和优先级，导入数量
  以数据库实际记录数为准。备份按环境占用，界面显示忙碌状态并阻止重复提交。
* **设置 → 备份与恢复** — 关闭所有浏览器后，可将全部环境、持久浏览器数据、
  代理（含未绑定代理）、资源库、文件夹及排序导出为一个 `.shardx-workspace` 文件。
  文件仅限能解锁原 Windows 用户 DPAPI 密钥的环境使用。恢复到空工作区，保留环境
  和代理 ID，不覆盖现有数据。下载的运行时、可重建缓存、回收站和应用设置不在备份内。
* **环境回收站** — 普通环境的单个、批量、文件夹与 API 删除均进入应用内回收站，
  整份保留浏览器数据 7 天。恢复保留原环境 ID、指纹和登录资料；删除与恢复通过
  操作日志处理，中断后回滚未提交操作。启动时及每小时彻底清理到期条目，也可
  手动彻底删除或清空。临时环境仍在退出时直接清理。
* **本地扩展库** — 支持本地 CRX、ZIP 和已解压目录，统一导入便携目录；在环境
  编辑器中勾选所需扩展，下次启动生效。每个环境的扩展数据独立保存；取消选择后
  扩展不再加载，重新选择可继续使用原数据。固定扩展 ID 可随便携目录移动。
  环境和回收站中的扩展引用均会阻止库条目删除，解除引用后才可删除或替换版本。
  当前引擎要求 Manifest V3；损坏条目单独显示错误，健康条目仍可使用。引用统计
  按一次环境扫描汇总，列表按相关存储事件刷新。
* **更符合批量工作的配置管理** — 浏览器与代理都支持手动排序；批量添加保持
  输入顺序并整体追加到底部，单个新增同样追加到底部。克隆、导入和恢复遵循相同
  的显示顺序规则，并补充安全命名、运行中操作锁定和更清晰的错误反馈。
  选中多个环境后可点击“分配代理”，按列表顺序预览并应用配对；默认跳过已占用
  代理，未匹配环境保留原绑定，也可明确允许循环共享。应用时复核绑定与运行状态，
  通过现有操作日志整批写入，失败时回滚。
  编辑器按实际变更字段合并，保留原有语言链、DNT 和指纹参数；同字段并发修改提示
  冲突，保存时再次核对版本。API PATCH 在代理探测结束后基于最新配置应用部分更新。
* **Windows 与界面体验改进** — 默认浅色主题，统一 Inter 字体和图标体系；
  为每个浏览器生成稳定的独立任务栏图标与名称徽标，并持续优化列表列宽、状态
  显示、行内重命名、批量工具栏、通知和窗口交互。移除了代理购买入口和推广按钮；
  Runtime 自动检查仅比较元数据，下载安装始终由用户明确触发。
  启动器界面使用简体中文；各浏览器环境的语言可独立设置。
  窗口内容区最小宽度为 1286 逻辑像素，可完整显示浏览器和代理表格的各列；操作列
  宽度固定，其余各列均分窗口加宽后增加的空间。
  API 页面和侧栏显示实际监听状态；端口占用时显示错误，保存的端口与当前监听端口
  分开展示。API 存储错误区分输入错误、资源不存在、操作冲突与内部 I/O 故障。

<p align="center">
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-MIT-blue?style=flat-square"></a>
</p>

<p align="center">
  <a href="https://pypi.org/project/shardx/"><img alt="PyPI version" src="https://img.shields.io/pypi/v/shardx?style=flat-square&logo=pypi&logoColor=white&label=pypi&color=blue"></a>
  <a href="https://www.npmjs.com/package/@proxyshard/shardx"><img alt="npm version" src="https://img.shields.io/npm/v/@proxyshard/shardx?style=flat-square&logo=npm&logoColor=white&label=npm&color=red"></a>
  <a href="https://crates.io/crates/shardx"><img alt="crates.io version" src="https://img.shields.io/crates/v/shardx?style=flat-square&logo=rust&logoColor=white&label=crates.io&color=orange"></a>
  <a href="https://docs.rs/shardx"><img alt="docs.rs" src="https://img.shields.io/docsrs/shardx?style=flat-square&logo=docsdotrs&logoColor=white&label=docs.rs"></a>
</p>

<p align="center">
  <a href="https://github.com/ProxyShard/ShardBrowser/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/ProxyShard/ShardBrowser?style=flat-square&logo=github&label=Stars&color=lightgrey"></a>
  <a href="https://github.com/ProxyShard/ShardBrowser/commits"><img alt="Last commit" src="https://img.shields.io/github/last-commit/ProxyShard/ShardBrowser?style=flat-square&color=success"></a>
  <a href="https://pypi.org/project/shardx/"><img alt="PyPI downloads" src="https://img.shields.io/pypi/dm/shardx?style=flat-square&logo=pypi&logoColor=white&label=pypi&color=brightgreen"></a>
  <a href="https://www.npmjs.com/package/@proxyshard/shardx"><img alt="npm downloads" src="https://img.shields.io/npm/dt/@proxyshard/shardx?style=flat-square&logo=npm&logoColor=white&label=npm&color=brightgreen"></a>
  <a href="https://crates.io/crates/shardx"><img alt="crates.io downloads" src="https://img.shields.io/crates/d/shardx?style=flat-square&logo=rust&logoColor=white&label=crates.io&color=brightgreen"></a>
</p>

本项目源自 **[ProxyShard](https://proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)** 团队 — 该代理服务提供完整的
**SOCKS5 UDP 中继**（RFC 1928 §7），并在出口侧实施主动的
**p0f TCP 指纹伪装**（代理所声称的操作系统与网站实际看到的 SYN/ACK 形状
一致）。ShardX 是我们为充分发挥这类代理能力而自建的反检测浏览器技术栈：
启动器负责管理环境、绑定代理，并分发在引擎层面实施指纹伪装的
**Chromium 157** 补丁版浏览器。

* **官网：**     [https://proxyshard.com](https://proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **文档：**     [https://docs.proxyshard.com](https://docs.proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **使用说明：** [https://docs.proxyshard.com/eng/usage-instructions/shardx-browser](https://docs.proxyshard.com/eng/usage-instructions/shardx-browser?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **UDP 介绍：** [https://docs.proxyshard.com/eng/our-products/about-udp](https://docs.proxyshard.com/eng/our-products/about-udp?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **p0f 介绍：** [https://docs.proxyshard.com/eng/our-products/p0f-spoofing](https://docs.proxyshard.com/eng/our-products/p0f-spoofing?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)

按任务需要选择任意方式驱动 ShardX — 四种方式读取同一份磁盘状态，
环境在所有入口之间互通，无需任何同步步骤：

* **桌面 UI** — 日常工作区（环境、代理、Cookie、指纹编辑器）。
* **本地 HTTP API** — `127.0.0.1:40325` 上的 Bearer-JWT 认证接口；用任意
  语言创建 / 启动 / 停止环境并获取 CDP 端点。
* **MCP 服务器** — 接入 Claude Desktop / Cursor，用自然语言编排环境
  （HTTP API + 经 CDP 的浏览器控制）。
* **独立 SDK** — Python、Node 与 Rust 库，自带引擎分发，完全无需 GUI；
  适合采集脚本 / CI / 服务器场景。

各方式的配置方法见下方 [使用方式](#使用方式)。

<p align="center">
  <img src="docs/screenshots/00-launcher-workspace.jpg" alt="ShardX Launcher" width="820">
</p>

---

## 这是什么

**一款免费、开源的反检测浏览器，面向网页采集与多账号场景。**

可以并行运行数百个相互隔离的浏览器身份，每个都是一台完整成型的"设备"：
拥有自己的 GPU、屏幕、字体、音频栈、时区、语言、WebGL/WebGPU 能力、
TLS ClientHello、UA-CH、WebRTC 策略、地理位置和 Cookie — 各信号之间
相互自洽，且**全部在 Chromium 的 C++ 引擎内部伪装**（Blink / V8 /
网络栈），而不是检测器一眼就能识破的 JS 注入。

开箱即含 220 个设备指纹模板（Mac M1–M5、带 RTX/GTX/Intel/AMD GPU 的
Windows 台式机/笔记本、Linux 工作站），为每个环境绑定 SOCKS5 / HTTP 代理
后，其余交给启动器 — 根据代理出口国家自动解析时区、语言和地理位置，
隔离的 `user-data-dir`、持久化 Cookie、Widevine 预热、按启动器策略禁用
QUIC、默认阻断 WebRTC。

任何用途均可免费使用 — 可搭配 [ProxyShard](https://proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
代理获得稳定 TCP 与可选 UDP 中继，也支持自带代理。

开箱即得的效果：

| 测试项                                                          | 结果                                                                     |
|-----------------------------------------------------------------|--------------------------------------------------------------------------|
| [browserleaks.com/quic](https://browserleaks.com/quic)          | QUIC `True`，JA4 与真实 Chrome 一致，经 SOCKS5 UDP 中继 MTU 1232          |
| [fingerprint.com](https://fingerprint.com/demo)                 | Bot / VPN / DevTools / 浏览器篡改 全部 `Not detected`                     |
| [browserscan.net](https://www.browserscan.net)                  | 真实性 **100%**                                                            |
| [pixelscan.net](https://pixelscan.net)                          | 指纹**一致**，未检出代理 / 自动化                                          |
| [fp.haru.gay](https://fp.haru.gay)                              | `isBot: false`，所有子信号 `false`                                         |
| [antcpt.com/score_detector](https://antcpt.com/score_detector/) | reCAPTCHA v3 得分 **0.9**                                                  |
| [networktest.twilio.com](https://networktest.twilio.com)        | TURN UDP / TCP / TLS + 语音 — 全部 **Pass**（无真实 IP 泄漏）              |

---

## 已修补的指纹面

所有覆盖都在浏览器引擎内部实现 — 不存在可供检测器识别的 JavaScript 垫片
层，因此伪装值在 iframe、Web Worker、DevTools 和无头检查之间保持一致。

* **设备身份** — User Agent、平台、厂商、CPU 核心数、内存、触摸点数、
  完整 Sec-CH-UA 栈（brand、版本、架构、位宽、移动端标记、型号）及稳定的
  GREASE。
* **图形** — WebGL renderer / vendor / 扩展 / 限制，WebGPU 适配器与
  limits，Canvas、DOMRect 和 ClientRects 的每环境确定性噪声，色域与 HDR
  声明。
* **音频** — 采样率、声道数，可选的原始音频样本每环境噪声。
* **屏幕与窗口** — 完整分辨率 + 可用区域 + DPR + 色深，设最大尺寸上限，
  操作系统不会把窗口调整到超出所声称的尺寸。
* **语言区域** — 时区、ICU locale、主语言与 Accept-Language 头均根据
  绑定代理的国家自动推导。
* **地理位置** — 坐标可手动设置或由代理出口 IP 推导；绝不使用宿主机
  GPS / Wi-Fi 定位。
* **网络能力** — 连接类型、下行速率、RTT、省流标记、存储配额、JS 堆
  上限、电池状态、媒体设备数量。
* **TLS ClientHello** — 内置 Chromium 的密码套件与签名算法选择、扩展
  洗牌，JA4 / Akamai / Peetprint 指纹与真实 Chrome 一致。
* **UDP 中继仍然可用** — SOCKS5 UDP 支持仍被测试与保留，供 WebRTC
  策略和 SDK 使用；桌面启动器强制关闭 QUIC。
* **WebRTC 策略** — `block` / `tcp_only` / `auto`。`auto` 模式下流量走
  代理的 UDP 中继；其余情况下 WebRTC 候选上报代理出口 IP，绝不上报宿主
  IP。私网内的 STUN / TURN 目标会被丢弃。
* **语音合成** — 完整的按操作系统 `speechSynthesis.getVoices()` 枚举
  （macOS 200+ 语音、Windows 为 SAPI + Google、Linux 仅 Google）。
* **字体** — 系统字体枚举固定为每环境的字体集，字体探测返回所声称设备的
  字体而非宿主机字体。
* **Linux 上的 WebGPU** — 默认禁用，与真实 Linux Chrome 的实际暴露情况
  一致（多数发行版默认关闭 WebGPU）。
* **Google 校验头** — 真实 Google Chrome 访问 Google 服务时附加的请求头
  （尤其是 `x-client-data` — 它的缺失是最响亮的 reCAPTCHA 机器人信号）
  被正确复现。
* **WebAuthn** — 平台认证器可用性与所声称的设备一致。
* **加固** — 每环境预热 Widevine、清除无头标记、关闭 devtools-protocol
  侧信道、彻底禁用同步、无钥匙串弹窗、无 Google 账号遥测、不泄漏 Privacy
  Sandbox 注册数据。

---

## 启动器功能

* **环境工作区** — 每环境独立 `user-data-dir`、持久化 Chrome 会话
  （恢复上次会话且不弹崩溃还原气泡）、批量导入、文件夹 / 标签组织、置顶、
  克隆。
* **指纹库** — 经 CDN 分发 220 个多平台指纹模板。更换 GPU 时，环境编辑器会联动随机化
  CPU / 内存 / 平台版本。
* **代理管理器** — SOCKS5 / HTTP / HTTPS，批量粘贴导入，逐代理实测
  （TCP + UDP_ASSOCIATE 探测 + geo 查询），按 id 绑定环境或启动时内联
  指定。根据代理出口国家自动解析时区 / 语言 / 地理位置。未绑定代理的
  直连环境以 `--no-proxy-server` 启动，显式绕过系统代理设置。
* **自动运行时** — 首次启动从 CDN 拉取 ShardX 补丁版 Chromium、Widevine
  CDM 与指纹库，Widevine 放置于 Windows 浏览器运行时旁，持久化 etag 后
  后续启动零网络请求；自包含发行版则内置全部运行时，首启无需任何下载。
* **本地自动化 API** — 运行于 `127.0.0.1` 的 axum HTTP 服务器，
  JWT-Bearer 认证。完整参考见
  [docs.proxyshard.com/eng/shardx-launcher-api](https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle?fallback=true&utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)，
  原始 schema 见 [openapi.yaml](openapi.yaml)。可以编程方式创建 / 启动 /
  停止环境并获取 CDP WebSocket URL。
* **内置 MCP 服务器** — 接入 Claude Desktop / IDE，用自然语言编排环境。
* **Cookie 导入导出** — 导出环境的 Chromium Cookie、导出 Cookie 加当前
  指纹双文件，或从导入原子替换 Cookie，采用 Windows AES-256-GCM + DPAPI
  解密。
* **Windows x64 便携启动器** — 启动器、浏览器运行时、环境、Cookie、MCP
  下载与导出全部保留在可执行文件旁。环境在 Chromium 内仍可模拟 Windows、
  macOS 或 Linux 指纹。

---

## 截图

### 网络引擎能力 — SOCKS5 上的 QUIC + WebRTC

引擎支持经 SOCKS5 UDP 中继使用 QUIC，但桌面启动器现已强制关闭 QUIC。
显式启用 WebRTC 时，Twilio 测试套件的全部 WebRTC 探测（UDP / TCP / TLS）
均通过，且不泄漏宿主 IP。

| browserleaks.com/quic — QUIC `True`，JA4 与内置 Chromium 一致  | networktest.twilio.com — 全部探测 `Pass`                |
|-------------------------------------------------------------|------------------------------------------------------------|
| ![QUIC](docs/screenshots/01-browserleaks-quic.jpg)          | ![Twilio](docs/screenshots/04-twilio-webrtc.jpg)           |

### 机器人 / 自动化检测

| fingerprint.com — Bot / VPN / DevTools / 篡改 `Not detected` | fp.haru.gay — `isBot: false`，全部信号 `false`          |
|-------------------------------------------------------------------|---------------------------------------------------------|
| ![FP](docs/screenshots/03-fingerprint-com.jpg)                    | ![Haru](docs/screenshots/07-haru-bot-detect.jpg)        |

### 指纹一致性

| ProxyShard 自家浏览器检查器 — 9 大类无任何问题     | pixelscan.net — 指纹**一致**                              |
|-------------------------------------------------------------------|---------------------------------------------------------|
| ![ProxyShard](docs/screenshots/02-proxyshard-checker.jpg)         | ![Pixelscan](docs/screenshots/06-pixelscan.jpg)         |

### 真实性评分

| browserscan.net — 真实性 100%，locale 生效                | antcpt.com — reCAPTCHA v3 得分 **0.9**                      |
|-----------------------------------------------------------|------------------------------------------------------------|
| ![Browserscan](docs/screenshots/05-browserscan.jpg)       | ![reCAPTCHA](docs/screenshots/08-recaptcha-score.jpg)      |

---

## 与其他反检测浏览器的对比

三者都是打过补丁的 Chromium 分支 — 差异在于*各自修补了哪些面*、*修补得多
干净*，以及引擎之外包裹了什么。

| 功能                                                        | ShardX（本项目）             | CloakBrowser                 | Multilogin / AdsPower / Dolphin                |
|--------------------------------------------------------------|------------------------------|------------------------------|------------------------------------------------|
| WebGPU 伪装（`navigator.gpu` 适配器 + 全部 limits）          | ✅ 完整                      | ❌ 未处理 — 宿主 GPU 泄漏      | ✅ 完整                                         |
| Client Hints（含 GREASE 的完整 Sec-CH-UA-* 栈）              | ✅ 完整                      | ❌ 部分 / 不一致              | ✅ Multilogin / AdsPower 完整，❌ Dolphin        |
| 按环境固定的字体枚举                                          | ✅ 系统级                    | ❌ 仅 JS 层，宿主字体仍可经 CSS / canvas 字体渲染泄漏 | ⚠️ 部分                       |
| V8 / CDP 侧信道加固（preview-getters、inspector）           | ✅ 全部关闭                  | ❌ 敞开 — CDP 自动化可被检测   | ⚠️ 部分                                        |
| TLS ClientHello 指纹（JA4）                                  | ✅ 与内置 Chromium 一致       | ⚠️ 静态 / 升级后漂移          | ✅ 与所分支 Chrome 版本一致                      |
| SOCKS5 上的 QUIC / HTTP-3                                    | ✅ 经 UDP 中继端到端稳定      | ⚠️ 已实现但不稳定 — 回退 TCP / 会话中途掉线 | ❌ 设置代理时禁用               |
| SOCKS5 上的 WebRTC（STUN 不泄漏真实 IP）                     | ✅ 代理 UDP 中继或合成候选    | ⚠️ 同样的 UDP 中继路径，同样不稳定 | ⚠️ 仅能禁用                    |
| 生成环境的一致性                                              | ✅ 设备自洽（GPU ↔ CPU ↔ 内存 ↔ UA ↔ 字体） | ❌ 频繁自相矛盾（Windows UA + Mac GPU、移动 UA + 桌面屏幕等） | ⚠️ 参差不齐     |
| 内置指纹库                                                    | 220 个设备指纹模板            | ❌ 随机生成器 — 指纹不自洽（Windows UA + Mac GPU、移动 UA + 桌面屏幕等） | 目录制（订阅收费） |
| 定价                                                         | **免费** — 仅代理费用         | **免费** — 仅引擎            | 付费 / 免费增值                                 |
| 管理 UI                                                      | ✅ 桌面应用（本启动器）        | ⚠️ 仅 CLI — 无 GUI，环境靠手工 / 脚本管理 | ✅ 桌面应用                    |
| 启动器源码                                                    | **开放**（MIT，本仓库）       | **开放**（CLI）               | 闭源                                           |

### 为什么这些差异在实践中重要

公开的"我的浏览器像不像真人？"检查站 — fingerprint.com、pixelscan.net、
browserscan.net、fp.haru.gay、antcpt 的 reCAPTCHA 得分检测器 — 通常并不会
逐一探测下面这些面，所以一个在这些面上失效的反检测浏览器依然能在那些页面
全绿通过，让人误以为万事大吉。

真实的产线反欺诈系统确实会查这些面，差距正是账号"多用几次才被标记"而非
"立即被发现"的原因：

* CloakBrowser 上 `navigator.gpu.requestAdapter()` 返回**宿主** GPU，
  声称 RTX 4060 + Windows 的环境会露出底下的 Mac M 系列适配器。ShardX
  （以及付费反检测产品）返回所声称的 GPU 及完整 WebGPU limits。
* CDP 包装层、V8 inspector preview-getters 和 `Object.toString` 侧信道
  在 CloakBrowser 上完全敞开，付费产品也只部分关闭。ShardX 的补丁关闭了
  每一个已公开的侧信道 — 自动化保持不可见。
* 经 canvas 字体渲染或 `document.fonts.check()` 抓取的字体列表在
  CloakBrowser 上无论环境声称什么都返回**宿主**字体。ShardX 在系统层固定
  字体枚举，结果与所声称设备一致。
* CloakBrowser 的环境生成器经常产出不自洽的指纹（Win32 平台配 macOS
  User Agent、移动 UA 配 1920×1080 屏幕、RTX GPU 配
  `hardwareConcurrency=2`）。ShardX 的指纹库来自真实设备采样，所有信号
  相互印证。
* 桌面启动器对每个环境强制关闭 QUIC / HTTP-3，使认证等改状态请求走
  TCP/TLS 路径。SOCKS5 UDP 支持仍向代理测试、WebRTC 策略和 SDK 直启
  保留。

---

## 快速开始

### 方式 A — 下载预构建发行版

发行版通过 GitHub Actions 的 `Release` 工作流手动发布，运行分支为 `main`。
工作流使用源码声明的版本，发布后保持版本号不变。需要调整版本时，显式运行
`node scripts/bump-release-version.mjs --from <当前版本> --to <目标版本>` 同步所有声明，
再执行 `node scripts/check-version-consistency.mjs`；手动填写的 tag 必须与源码版本完全一致。

从 [GitHub Releases](../../releases) 获取 Windows x64 构建。每个 Release
提供两种产物，按需选择：

* **`ShardX-Launcher-portable-win-x64.exe`（便携版）** — 单文件便携
  EXE，首次启动时从 CDN 下载浏览器运行时（约 150 MB）、Widevine（约
  16 MB）与指纹库（约 470 KB）。适合日常使用，升级体积最小。
* **`ShardX-Launcher-selfcontained-win-x64.zip`（自包含版）** — 内置
  启动器 + 浏览器运行时 + Widevine + 指纹库并预置版本快照，解压即用，
  首次启动无任何 CDN 下载（之后仍走应用内更新检查）。适合离线分发或
  网络受限环境。

自包含包由独立的 `runtime-archive` Release 供料（"Sync runtime archive"
工作流播种）。

发行版未做 Authenticode 签名。若 SmartScreen 提示 *"Windows 已保护你的
电脑"*，点击**更多信息** → **仍要运行**。重复启动不会再次提示。

### 方式 B — 从源码构建

```powershell
npm ci
npm run tauri dev
# 仅生成 Windows x64 便携 EXE
npm run tauri:build:windows-x64
```

可执行文件输出到
`src-tauri/target/x86_64-pc-windows-msvc/release/`。迁移启动器时，请把
EXE 与其旁侧的 `shardx-launcher` 数据目录一起移动。

### 首次启动

**便携版**首次启动会从 CDN 下载 Windows x64 补丁版浏览器（约 150 MB）、
Widevine（约 16 MB）与指纹库（约 470 KB）；**自包含版**跳过全部下载直接
进入工作区。所有持久化数据都保存在
`<启动器所在目录>\shardx-launcher\` 下，保持严格的便携布局。之后即可
绑定代理并启动第一个环境。

---

## 使用方式

四种可互换的驱动方式 — 按任务选择即可。它们读取同一份磁盘状态，在 UI
里创建的环境无需任何同步即可被 API、MCP 服务器和 SDK 访问。

### 1. 桌面 UI

日常工作流都在这里。打开应用，在“代理”页新建代理，或从剪贴板批量导入
`socks5://user:pass@host:port` 列表；点击“测试”执行 TCP、UDP 和位置探测。
在“浏览器”页点击环境的代理单元格进行绑定，或选中多个环境后使用“分配代理”，
再点击启动按钮。启动器会处理：

* 首次启动下载引擎、Widevine 和 220 个指纹模板（之后缓存 ETag）；
* 每环境独立 `user-data-dir`，Cookie / 缓存 / 扩展相互隔离；
* 每次启动前根据代理出口国家解析时区 / 语言 / 地理位置；
* 每次启动强制关闭 QUIC，同时保留实时 UDP 探测用于代理诊断与 WebRTC
  策略；新建环境默认 WebRTC 阻断；
* 下次启动重新绑定同一 `user-data-dir`，恢复上次会话且不弹崩溃还原
  气泡。

批量导入 / 导出、文件夹、标签、置顶、克隆、Cookie 导入导出
（Chromium SQLite v10 / DPAPI），以及联动随机化自洽硬件
（CPU ↔ 内存 ↔ 平台版本）的指纹编辑器，都在工作区内。

### 2. 本地自动化 API

运行于 `127.0.0.1:40325` 的 axum HTTP 服务器（端口可在
*Settings → Automation API* 配置）。适合在自己的代码中驱动启动器 —
Python、Go、curl，任何能说 HTTP 的语言。除 `GET /health` 外的每个端点
都要求 *Settings → Automation API* 中显示的 Bearer JWT（重新生成会即时
轮换签名密钥）。服务器仅绑定 `127.0.0.1`，并额外校验 Host 头只允许
`127.0.0.1` / `localhost` / `[::1]`，封堵 DNS rebinding 攻击路径。

* **参考文档：** [https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle](https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **OpenAPI schema：** [openapi.yaml](openapi.yaml)

启动一个环境并获取 CDP 端点：

```bash
TOKEN="<来自 Settings → Automation API>"
BASE="http://127.0.0.1:40325"

# 以 CDP 模式启动环境 — 返回浏览器监听的 websocket。
# 可接入任何 CDP 客户端（puppeteer、原生 WS、patchright、自研等）。
curl -s -X POST "$BASE/profiles/win-rtx4060/start?cdp=true&headless=false" \
     -H "Authorization: Bearer $TOKEN" | jq .
# → {"id":"win-rtx4060","cdp_url":"ws://127.0.0.1:53217/devtools/browser/…","pid":48211}

# 停止环境。
curl -s -X POST "$BASE/profiles/win-rtx4060/stop" \
     -H "Authorization: Bearer $TOKEN"
```

端点覆盖环境（创建 / 编辑 / 删除 / 启动 / 停止 / 列出运行中）、代理
（增 / 删 / 列）、指纹（生成、列出库）、文件夹、Cookie（导入 / 导出）
以及指纹生成器 — 完整清单见 OpenAPI 文件。

### 3. MCP 服务器

面向 Claude Desktop、Cursor 及其他 MCP 客户端的
[Model Context Protocol](https://modelcontextprotocol.io) 服务器。同时封装
启动器的 HTTP API 与经 CDP 的浏览器控制（基于 patchright），让语言模型
可以：

* 通过启动器管理环境 / 代理 / 指纹 / 文件夹 / Cookie；
* 在运行中的 ShardX 环境里导航 / 点击 / 输入 / 等待 / 截图，需要时自动
  启动环境。

应用本身不运行该服务器 — 打开 *Settings → MCP server → Download MCP
server*，源码会安装到启动器便携数据目录的 `mcp` 下。在其中安装依赖后，
把 `<启动器数据目录>/mcp/index.js` 注册到你的 MCP 客户端。完整的安装
步骤、环境变量与工具清单见 **[mcp/README.md](mcp/README.md)**。

最小 stdio 注册示例：

```json
{
  "mcpServers": {
    "shardx": {
      "command": "node",
      "args": ["/ABSOLUTE/PATH/TO/LAUNCHER-DATA/mcp/index.js"],
      "env": {
        "SHARDX_API": "http://127.0.0.1:40325",
        "SHARDX_TOKEN": "<Bearer token>"
      }
    }
  }
}
```

### 4. 独立 SDK（Python / Node / Rust）

**完全不需要桌面应用**的自包含客户端库 — 首次使用时下载同一套引擎 +
指纹库，直接经子进程启动环境并附带浏览器控制客户端 — Python/Node 用
[patchright](https://github.com/Kaliiiiiiiiii-Vinyzu/patchright)
（隐身 Playwright），Rust 用 [chromiumoxide](https://docs.rs/chromiumoxide)
（CDP）。与启动器相同的启动前管线：UDP 探测 → 条件启用 QUIC、自动字段
geo 解析、屏幕策略、感知宿主的硬件随机化。

想在采集脚本 / CI 任务 / 服务端程序里把 ShardX 当库用、又不想装 GUI 时，
选 SDK。

* **Python** — [sdks/python/README.md](sdks/python/README.md) — `pip install shardx`
* **Node** — [sdks/node/README.md](sdks/node/README.md) — `npm install @proxyshard/shardx`
* **Rust** — [sdks/rust/README.md](sdks/rust/README.md) — `cargo add shardx`

---

## 许可

**启动器**（本仓库的全部内容 — Tauri 外壳、React UI 与 Rust 源码）以
**MIT License** 开源 — 见 [LICENSE](LICENSE)。可自由使用、fork、修改、
发布，包括商业用途。

**浏览器引擎**（启动器首次运行时从 CDN 下载的 Chromium 157 补丁版二进制）
以**闭源二进制**形式分发。其源代码不在本仓库或任何其他地方公开，且明确
**不允许**：

* 逆向工程、反汇编、反编译，或任何提取 / 还原引擎源代码的尝试；
* 再分发引擎的修改版本；
* 将引擎 — 或由它衍生的任何二进制（无论是否修改）— 用于商业反检测 /
  浏览器 / 指纹伪装产品或服务。

个人使用、网页采集、多账号以及与启动器自动化 API 的集成均不受限制。
若想基于引擎构建商业产品，请先与我们联系。
