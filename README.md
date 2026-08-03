# ShardX Launcher

## 本仓库相对原项目的主要改进

本仓库基于 [ProxyShard/ShardBrowser](https://github.com/ProxyShard/ShardBrowser)
持续维护，保留原项目的 Chromium 指纹能力、自动化 API、MCP 与多语言 SDK，
并重点补强了 Windows 便携使用、数据安全、代理测试、会话稳定性和批量管理体验。
原项目的英文说明完整保留在下方。

* **严格的 Windows x64 便携化** — 使用无需安装的便携 EXE，运行时、配置、
  浏览器资料、Cookie、导出文件和 MCP 下载均保存在程序旁的
  `shardx-launcher` 目录；启动时会校验安装路径及目录可写性，避免静默写入
  系统用户目录，也不再生成 MSI 安装包。
* **更可靠的 Runtime 生命周期** — 首次缺少 Runtime 时自动安装；已有
  Runtime 不会因远端版本变化而在后台被替换，更新检查与修复必须由用户主动
  触发。安装过程采用暂存、完整性校验、原子切换和失败回滚，并同步强化了
  Node、Python、Rust SDK 与 MCP 的下载和依赖安全。
* **代理管理与测试重构** — 支持 SOCKS5、HTTP、HTTPS 批量导入、去重和输入
  顺序保持；新增内容统一追加到列表末尾。单个坏代理不会阻塞其他结果，批量测试
  会逐条刷新 UI，所有 TCP、UDP 和 Geo-IP 测试统一使用 5 秒超时；手动测试拥有
  优先通道，定时自动测试不会挤占它。Geo-IP 支持六个服务、可配置首选服务，
  遇到超时、HTTP 429 或无效 JSON 时会自动回退到下一服务。
* **浏览器会话与账号保护** — 浏览器正常关闭时等待其完整退出，避免粗暴终止
  损坏登录态；存在运行中的浏览器时阻止误退出启动器，并禁止修改其配置或代理。
  每个浏览器还会锁定已验证的代理网络身份，同一绑定发生国家或时区跳变时会在
  启动前阻止暴露 Google、ChatGPT 等账号会话。
* **更低的指纹重复风险** — 新建浏览器优先使用尚未使用或使用次数最少的指纹
  模板；Canvas 与 WebGL 默认启用稳定的每配置噪声，ClientRects、Audio、
  Sensors 和 Fonts 默认保持真实值。新建与克隆时会检测有效指纹碰撞并重新生成
  唯一种子，同时普通编辑和完整备份恢复不会擅自改变已有浏览器指纹。
* **完整配置、Cookie 与浏览器备份** — 关键 JSON 使用原子写入并保留 `.bak`
  恢复副本；Cookie 导入采用原子替换。完整浏览器备份包含指纹配置、整个
  `user-data`、绑定代理和 Chromium 加密密钥，导入前会校验清单、大小、路径和
  内容，失败时回滚，恢复后仍保留原登录态与指纹身份。
* **更符合批量工作的配置管理** — 浏览器与代理都支持手动排序；批量添加保持
  输入顺序并整体追加到底部，单个新增同样追加到底部。克隆、导入和恢复遵循相同
  的显示顺序规则，并补充安全命名、运行中操作锁定和更清晰的错误反馈。
* **Windows 与界面体验改进** — 默认浅色主题，统一 Inter 字体和图标体系；
  为每个浏览器生成稳定的独立任务栏图标与名称徽标，并持续优化列表列宽、状态
  显示、行内重命名、批量工具栏、通知和窗口交互。移除了代理购买入口、推广按钮
  和启动器自动更新检查，使便携发行更独立、可控。

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

A project by the **[ProxyShard](https://proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)** team — the
proxy service with full **SOCKS5 UDP relay** (RFC 1928 §7) and active
**p0f TCP-fingerprint spoofing** on the exit (so the OS the proxy
claims to be on actually matches the SYN/ACK shape sites see). ShardX
is the in-house anti-detect browser stack we built to get the most out
of those proxies: the launcher manages profiles, binds proxies, and
ships the patched **Chromium 149** browser that does the actual
spoofing at the engine level.

* **Site:**     [https://proxyshard.com](https://proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **Docs:**     [https://docs.proxyshard.com](https://docs.proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **Usage:**    [https://docs.proxyshard.com/eng/usage-instructions/shardx-browser](https://docs.proxyshard.com/eng/usage-instructions/shardx-browser?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **UDP info:** [https://docs.proxyshard.com/eng/our-products/about-udp](https://docs.proxyshard.com/eng/our-products/about-udp?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **p0f info:** [https://docs.proxyshard.com/eng/our-products/p0f-spoofing](https://docs.proxyshard.com/eng/our-products/p0f-spoofing?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)

Drive ShardX whichever way fits the job — all four read from the same
on-disk state, so a profile is reachable from every entry-point with
no sync step:

* **Desktop UI** — workspace for day-to-day work (profiles, proxies,
  cookies, fingerprint editor).
* **Local HTTP API** — Bearer-JWT auth on `127.0.0.1:40325`; create /
  start / stop profiles and grab a CDP endpoint from any language.
* **MCP server** — drops into Claude Desktop / Cursor for
  natural-language profile orchestration (HTTP API + browser-over-CDP).
* **Standalone SDKs** — Python, Node + Rust libraries that ship the engine
  themselves and need no GUI at all; ideal for scrapers / CI / servers.

Setup for each lives in [Usage](#usage) below.

<p align="center">
  <img src="docs/screenshots/00-launcher-workspace.jpg" alt="ShardX Launcher" width="820">
</p>

---

## What it is

**A free, open-source anti-detect browser for web scraping and
multi-accounting.**

Run hundreds of isolated browser identities side by side, each one a
fully-formed device with its own GPU, screen, fonts, audio stack,
timezone, locale, WebGL/WebGPU caps, TLS ClientHello, UA-CH, WebRTC
policy, geolocation and cookies — every signal coherent with the
others, and every signal **spoofed inside Chromium's C++ engine**
(Blink / V8 / network stack), not via JS injection that detectors trip
on instantly.

You get 170 ready-made device profiles out of the box (mac M1–M5,
Windows desktops/laptops with RTX/GTX/Intel/AMD GPUs, Linux
workstations), bind a SOCKS5 / HTTP proxy to each one, and the
launcher handles the rest — auto-resolved timezone + locale +
geolocation from the proxy's exit country, isolated `user-data-dir`,
persistent cookies, Widevine pre-warm, QUIC disabled by launcher policy,
and WebRTC blocked by default.

Free for any use — pair with [ProxyShard](https://proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
proxies for stable TCP and optional UDP relay support, or bring your own.

What this gives you out of the box:

| Test                                                            | Result                                                                   |
|-----------------------------------------------------------------|--------------------------------------------------------------------------|
| [browserleaks.com/quic](https://browserleaks.com/quic)          | QUIC `True`, JA4 matches real Chrome, MTU 1232 over SOCKS5 UDP relay     |
| [fingerprint.com](https://fingerprint.com/demo)                 | Bot / VPN / DevTools / browser-tampering all `Not detected`              |
| [browserscan.net](https://www.browserscan.net)                  | Authenticity **100%**                                                    |
| [pixelscan.net](https://pixelscan.net)                          | Fingerprint **consistent**, no proxy / automation detected               |
| [fp.haru.gay](https://fp.haru.gay)                              | `isBot: false`, every sub-signal `false`                                 |
| [antcpt.com/score_detector](https://antcpt.com/score_detector/) | reCAPTCHA v3 score **0.9**                                               |
| [networktest.twilio.com](https://networktest.twilio.com)        | TURN UDP / TCP / TLS + Voice — all **Pass** (no real-IP leak)            |

---

## Fingerprint surfaces patched

All overrides live inside the browser engine — there is no JavaScript
shim layer that detectors can spot, so spoofed values are consistent
across iframes, web workers, devtools and headless inspection.

* **Device identity** — user agent, platform, vendor, CPU cores, RAM,
  touch points, full Sec-CH-UA stack (brand, version, architecture,
  bitness, mobile, model) with stable GREASE.
* **Graphics** — WebGL renderer / vendor / extensions / limits, WebGPU
  adapter + limits, deterministic per-profile noise for Canvas, DOMRect
  and ClientRects, color gamut and HDR claims.
* **Audio** — sample rate, channel count, optional per-profile noise on
  raw audio samples.
* **Screen & window** — full resolution + available area + DPR + color
  depth, max-size cap so the OS won't resize past the claimed
  dimensions.
* **Locale** — timezone, ICU locale, primary language and the
  Accept-Language header auto-derived from the bound proxy's country.
* **Geolocation** — coordinates either set manually or derived from the
  proxy's exit IP; host GPS / Wi-Fi is never used.
* **Network capability** — connection type, downlink, RTT, save-data,
  storage quota, JS heap limit, battery state, media-device counts.
* **TLS ClientHello** — Chrome-149 cipher + signature-algorithm
  selection, extension shuffling, so JA4 / Akamai / Peetprint fingerprints
  match real Chrome.
* **UDP relay remains available** — SOCKS5 UDP support is still tested and
  retained for WebRTC policy and SDK use; the desktop launcher forces QUIC off.
* **WebRTC policy** — `block` / `tcp_only` / `auto`. In `auto` traffic
  rides the proxy's UDP relay; otherwise WebRTC candidates report the
  proxy exit IP, never the host. STUN / TURN targets on private
  networks are dropped.
* **Speech voices** — full per-OS `speechSynthesis.getVoices()`
  enumeration (200+ macOS voices, SAPI + Google for Windows, Google-only
  for Linux).
* **Fonts** — system font enumeration pinned to a per-profile set so
  font-list probes return the claimed device's fonts, not the host's.
* **WebGPU on Linux** — disabled to match what real Linux Chrome
  actually exposes (most distros ship WebGPU off).
* **Google validation headers** — the headers real Google Chrome adds to
  requests against Google properties (notably `x-client-data` — its
  absence is the loudest reCAPTCHA bot signal) are reproduced correctly.
* **WebAuthn** — platform-authenticator availability matches the
  claimed device.
* **Hardening** — Widevine pre-warmed per profile, headless markers
  stripped, devtools-protocol side-channels closed, sync hard-disabled,
  no keychain prompts, no Google account telemetry, no Privacy Sandbox
  enrollment data leaked.

---

## Launcher features

* **Profile workspace** — per-profile `user-data-dir`, persistent
  Chrome sessions ("Continue where you left off" without the
  crash-restore bubble), bulk import, folder / tag organisation, pin
  to top, clone.
* **Fingerprint library** — 170 starter profiles shipped via CDN
  (31 mac-arm64 / 120 windows-x64 / 19 linux-x64). Profile editor
  randomises CPU / RAM / platform-version when you change the GPU.
* **Proxy manager** — SOCKS5 / HTTP / HTTPS, bulk paste-import,
  per-proxy live test (TCP + UDP_ASSOCIATE probe + geo lookup), bind a
  proxy to a profile by id or inline-on-launch. Auto-resolves timezone
  / locale / geolocation from the proxy's exit country.
* **Auto-runtime** — first launch pulls the patched ShardX Chromium
  build, Widevine CDM and the fingerprint library from CDN, places
  Widevine beside the Windows browser runtime, persists an etag so subsequent
  launches are zero-network.
* **Local automation API** — axum HTTP server on `127.0.0.1`,
  JWT-Bearer auth. Full reference at
  [docs.proxyshard.com/eng/shardx-launcher-api](https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle?fallback=true&utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher),
  raw schema in [openapi.yaml](openapi.yaml). Create / start / stop
  profiles and get a CDP WebSocket URL programmatically.
* **MCP server bundled** — drop into Claude Desktop / IDE for
  natural-language profile orchestration.
* **Cookie I/O** — export the profile's Chromium Cookies, export cookies plus
  its active fingerprint as two files, or replace cookies atomically from an
  import, using Windows AES-256-GCM + DPAPI decryption.
* **Windows x64 portable launcher** — launcher, browser runtime, profiles,
  cookies, MCP downloads and exports all stay beside the executable. Profiles
  may still emulate Windows, macOS or Linux fingerprints inside Chromium.

---

## Screenshots

### Network engine capability — QUIC + WebRTC over SOCKS5

The engine supports QUIC through the SOCKS5 UDP relay, although the desktop
launcher now forces QUIC off. With WebRTC enabled explicitly,
every WebRTC probe (UDP / TCP / TLS) passes against Twilio's test
suite without leaking the host IP.

| browserleaks.com/quic — QUIC `True`, JA4 matches Chrome 149 | networktest.twilio.com — every probe `Pass`               |
|-------------------------------------------------------------|------------------------------------------------------------|
| ![QUIC](docs/screenshots/01-browserleaks-quic.jpg)          | ![Twilio](docs/screenshots/04-twilio-webrtc.jpg)           |

### Bot / automation detection

| fingerprint.com — Bot / VPN / DevTools / tampering `Not detected` | fp.haru.gay — `isBot: false`, every signal `false`     |
|-------------------------------------------------------------------|---------------------------------------------------------|
| ![FP](docs/screenshots/03-fingerprint-com.jpg)                    | ![Haru](docs/screenshots/07-haru-bot-detect.jpg)        |

### Fingerprint consistency

| ProxyShard's own browser-checker — no issues across 9 categories | pixelscan.net — Fingerprint **consistent**             |
|-------------------------------------------------------------------|---------------------------------------------------------|
| ![ProxyShard](docs/screenshots/02-proxyshard-checker.jpg)         | ![Pixelscan](docs/screenshots/06-pixelscan.jpg)         |

### Authenticity score

| browserscan.net — Authenticity 100 %, locale honoured     | antcpt.com — reCAPTCHA v3 score **0.9**                    |
|-----------------------------------------------------------|------------------------------------------------------------|
| ![Browserscan](docs/screenshots/05-browserscan.jpg)       | ![reCAPTCHA](docs/screenshots/08-recaptcha-score.jpg)      |

---

## Comparison with other anti-detect browsers

All three are patched Chromium forks — the differentiation is in *which*
surfaces each one bothers to patch, *how cleanly*, and what's wrapped
around the engine.

| Feature                                                       | ShardX (this project)        | CloakBrowser                 | Multilogin / AdsPower / Dolphin                |
|---------------------------------------------------------------|------------------------------|------------------------------|------------------------------------------------|
| WebGPU spoofing (`navigator.gpu` adapter + every limit)       | ✅ full                       | ❌ untouched — host GPU leaks | ✅ full                                         |
| Client Hints (Sec-CH-UA-* full stack with GREASE)             | ✅ full                       | ❌ partial / inconsistent     | ✅ full on Multilogin / AdsPower, ❌ Dolphin     |
| Font enumeration pinned per profile                           | ✅ system-level               | ❌ JS-only, host fonts still leak via CSS / canvas font-render | ⚠️ partial          |
| V8 / CDP side-channel hardening (preview-getters, inspector)  | ✅ closed                     | ❌ open — CDP automation detectable | ⚠️ partial                                |
| TLS ClientHello fingerprint (JA4)                             | ✅ matches real Chrome 149    | ⚠️ static / drifts on uprev   | ✅ matches the forked Chrome version            |
| QUIC / HTTP-3 over SOCKS5                                     | ✅ stable end-to-end via UDP relay | ⚠️ implemented but unstable — falls back to TCP / drops mid-session | ❌ disabled when proxy is set |
| WebRTC over SOCKS5 (no real-IP leak via STUN)                 | ✅ proxy UDP relay or synth candidates | ⚠️ same UDP relay path, same instability | ⚠️ disable-only            |
| Consistency of generated profiles                             | ✅ coherent device (GPU ↔ CPU ↔ RAM ↔ UA ↔ fonts) | ❌ frequent contradictions (Win UA + Mac GPU, mobile UA + desktop screen, etc.) | ⚠️ varies |
| Bundled fingerprint library                                   | 170 real-device profiles      | ❌ random generator — incoherent fingerprints (Win UA + Mac GPU, mobile UA + desktop screen, etc.) | catalog (subscription) |
| Pricing                                                       | **Free** — only proxy costs   | **Free** — engine only        | Paid / freemium                                |
| Management UI                                                 | ✅ desktop app (this launcher) | ⚠️ CLI only — no GUI, profiles managed by hand / scripts | ✅ desktop app                |
| Launcher source                                               | **Open** (MIT, this repo)     | **Open** (CLI)                | Closed                                         |

### Why this matters in practice

Public "is my browser human?" checkers — fingerprint.com,
pixelscan.net, browserscan.net, fp.haru.gay, antcpt's reCAPTCHA score
detector — generally don't bother to probe most of the surfaces below,
so an anti-detect that fails any of them can still light up all green
on those pages and feel like everything's fine.

Real production anti-fraud stacks do check them, and the gap is
exactly where accounts get flagged a few sessions in instead of
immediately:

* `navigator.gpu.requestAdapter()` returns the **host** GPU on
  CloakBrowser, so a profile claiming an RTX 4060 on Windows leaks the
  Mac M-series adapter underneath. ShardX (and the paid anti-detects)
  return the claimed GPU with full WebGPU limits.
* CDP wrappers, V8 inspector preview-getters and `Object.toString`
  side-channels are wide open on CloakBrowser and only partially closed
  on the paid anti-detects. ShardX patches close every documented side
  channel — automation stays invisible.
* Font lists scraped via canvas font rendering or
  `document.fonts.check()` return the **host** font list on
  CloakBrowser no matter what the profile claims. ShardX pins the font
  enumeration at the system level so the result matches the device.
* CloakBrowser's profile generator routinely emits incoherent
  fingerprints (Win32 platform with macOS user-agent, mobile UA with
  1920×1080 screen, RTX GPU with `hardwareConcurrency=2`). ShardX's
  library is derived from real-device samples so every signal agrees
  with the others.
* The desktop launcher forces QUIC / HTTP-3 off for every profile so
  authentication and other state-changing requests use the TCP/TLS path.
  SOCKS5 UDP support remains available to proxy tests, WebRTC policy, and
  direct SDK launches.

---

## Quick start

### Option A — grab a pre-built release

Download the Windows x64 build from [GitHub Releases](../../releases). Use the
portable `.exe`; no installer build is produced.

The release is not Authenticode-signed. If SmartScreen shows *"Windows
protected your PC"*, click **More info** → **Run anyway**. Repeated launches
do not re-prompt.

### Option B — build from source

```powershell
npm ci
npm run tauri dev
# Windows x64 portable EXE only
npm run tauri:build:windows-x64
```

The executable is written to
`src-tauri/target/x86_64-pc-windows-msvc/release/`. Move the EXE together with
its adjacent `shardx-launcher` data directory when relocating the launcher.

### First launch

The app downloads the Windows x64 patched browser (~150 MB), Widevine (~16 MB)
and the fingerprint library (~470 KB) from the CDN. All persistent data is
placed under `<launcher executable directory>\shardx-launcher\`, preserving the
strict portable layout. Then you can bind a proxy and launch the first profile.

---

## Usage

Four interchangeable ways to drive ShardX — pick whichever matches the
job. All four read from the same on-disk state, so a profile created in
the UI is reachable from the API, the MCP server and the SDKs without
any sync step.

### 1. Desktop UI

Day-to-day workflow lives here. Open the app, add a proxy
(*Proxies* → *Add proxy* — paste `socks5://user:pass@host:port` or
bulk-paste a list, hit *Test* to run a TCP + UDP_ASSOCIATE + geo
probe), bind it to a profile (*Profiles* → pick one → *Bind proxy*),
and hit *Start*. The launcher takes care of:

* downloading the engine + Widevine + 170 starter profiles on first
  launch (etag-cached afterward);
* per-profile `user-data-dir` so cookies / cache / extensions stay
  isolated;
* resolving timezone / locale / geolocation from the proxy's exit
  country before each launch;
* forcing QUIC off on every launch while retaining the live UDP probe for
  proxy diagnostics and WebRTC policy; new profiles default to WebRTC block;
* re-binding to the same `user-data-dir` next time so you get
  *"Continue where you left off"* without the crash-restore bubble.

Bulk import / export, folders, tags, pin-to-top, clone, cookie
import / export (Chromium SQLite v10 / DPAPI), and a fingerprint
editor that randomises coherent hardware (CPU ↔ RAM ↔ platform version)
are all in the workspace.

### 2. Local automation API

An axum HTTP server bound to `127.0.0.1:40325` (port configurable in
*Settings → Automation API*). Use this when you want to drive the
launcher from your own code — Python, Go, curl, anything that speaks
HTTP. Every endpoint except `GET /health` requires a Bearer JWT shown
in *Settings → Automation API* (regenerate rotates the signing secret
live).

* **Reference docs:** [https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle](https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **OpenAPI schema:** [openapi.yaml](openapi.yaml)

Launching a profile and getting a CDP endpoint:

```bash
TOKEN="<from Settings → Automation API>"
BASE="http://127.0.0.1:40325"

# Start the profile in CDP mode — returns the websocket the browser
# is listening on. Reuse it with any CDP client (puppeteer, raw WS,
# patchright, your own).
curl -s -X POST "$BASE/profiles/win-rtx4060/start?cdp=true&headless=false" \
     -H "Authorization: Bearer $TOKEN" | jq .
# → {"id":"win-rtx4060","cdp_url":"ws://127.0.0.1:53217/devtools/browser/…","pid":48211}

# Stop it.
curl -s -X POST "$BASE/profiles/win-rtx4060/stop" \
     -H "Authorization: Bearer $TOKEN"
```

Endpoints cover profiles (create / edit / delete / start / stop / list
running), proxies (add / delete / list), fingerprints (generate, list
library), folders, cookies (export / import) and a fingerprint
generator — full list in the OpenAPI file.

### 3. MCP server

A [Model Context Protocol](https://modelcontextprotocol.io) server for
Claude Desktop, Cursor and any other MCP client. Wraps both the
launcher's HTTP API and the browser-over-CDP (via patchright) so a
language model can:

* manage profiles / proxies / fingerprints / folders / cookies through
  the launcher;
* navigate / click / type / wait / screenshot in a live ShardX
  profile, with the profile auto-starting when needed.

The app doesn't run the server itself — open *Settings → MCP server →
Download MCP server*, pick a folder, then `npm install` and register
with your MCP client. Full setup, env vars and tool list in
**[mcp/README.md](mcp/README.md)**.

Minimal stdio registration:

```json
{
  "mcpServers": {
    "shardx": {
      "command": "node",
      "args": ["/ABSOLUTE/PATH/mcp/index.js"],
      "env": {
        "SHARDX_API": "http://127.0.0.1:40325",
        "SHARDX_TOKEN": "<Bearer token>"
      }
    }
  }
}
```

### 4. Standalone SDKs (Python / Node / Rust)

Self-contained client libraries that **don't need the desktop app at
all** — they download the same engine + fingerprint library on first
use and launch profiles directly via subprocess, with a browser-control
client attached — [patchright](https://github.com/Kaliiiiiiiiii-Vinyzu/patchright)
(stealth Playwright) for Python/Node, [chromiumoxide](https://docs.rs/chromiumoxide)
(CDP) for Rust. Same pre-launch pipeline as the launcher: UDP probe →
conditional QUIC, geo-resolve for auto fields, screen strategy, host-aware
hardware randomisation.

Use the SDK when you want ShardX as a library inside a scraper / CI
job / server-side worker without installing the GUI.

* **Python** — [sdks/python/README.md](sdks/python/README.md) — `pip install shardx`
* **Node** — [sdks/node/README.md](sdks/node/README.md) — `npm install @proxyshard/shardx`
* **Rust** — [sdks/rust/README.md](sdks/rust/README.md) — `cargo add shardx`

---

## Licensing

The **launcher** (everything in this `rust/shardx-launcher/` directory
— Tauri shell, React UI, Rust source) is open source under the **MIT
License** — see [LICENSE](LICENSE). Use it, fork it, modify it, ship
it, commercially or otherwise.

The **browser engine** (the patched Chromium 149 binary that the
launcher downloads from our CDN on first run) is distributed as a
**closed-source binary**. Its source is not published in this
repository or elsewhere, and the following are explicitly **not
permitted**:

* reverse engineering, disassembly, decompilation, or any attempt to
  extract or reconstruct the engine source;
* redistributing a modified version of the engine;
* using the engine — or any binary derived from it, with or without
  modification — as part of a commercial anti-detect / browser /
  fingerprint-spoofing product or service.

Personal use, web scraping, multi-accounting and integration with the
launcher's automation API are all fine. If you want to build something
commercial on top of the engine, contact us first.
