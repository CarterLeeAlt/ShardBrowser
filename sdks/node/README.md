# @proxyshard/shardx (Node)

[ProxyShard](https://proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
团队出品的 **ShardX 反检测浏览器** 自包含 Node/TypeScript SDK。

**不**依赖桌面启动器。首次使用时，它会从我们的 CDN 将打过补丁的
Chromium 152 引擎、Widevine CDM 以及包含 170 个环境的指纹库下载到本地
缓存，然后按需启动隔离的浏览器会话。

由 [patchright](https://github.com/Kaliiiiiiiiii-Vinyzu/patchright)
（打过反检测补丁的 Playwright）驱动——`sdk.session()` 返回一个开箱即用
的 `Browser` 实例，无需手动拼装 `connectOverCDP`。

## 安装

```bash
npm install @proxyshard/shardx
```

支持的主机平台：**macOS arm64**、**Windows x64**、**Linux x64**。Node ≥ 18。

### Linux 系统依赖

内置的 Chromium 引擎需要 `unzip` 以及任何 Chromium 分支都会链接的标准
共享库集合。在全新的 Debian / Ubuntu 上：

```bash
sudo apt install -y \
  unzip ca-certificates fonts-liberation \
  libnss3 libnspr4 libatk1.0-0 libatk-bridge2.0-0 libcups2 \
  libxkbcommon0 libxcomposite1 libxdamage1 libxfixes3 libxrandr2 \
  libgbm1 libpango-1.0-0 libcairo2 libasound2 libxshmfence1
```

以 **root** 身份或在 **Docker** 中启动时，通过 `extraArgs` 传入
`--no-sandbox` 和 `--disable-dev-shm-usage`：

```ts
await sdk.session(profile, { extraArgs: ["--no-sandbox", "--disable-dev-shm-usage"] });
```

## 快速开始

```ts
import { ShardX } from "@proxyshard/shardx";

const sdk = new ShardX();
// Engine + Widevine + fingerprint library auto-download from CDN on
// the first session/launch/listProfiles call (~170 MB once, etag-cached
// afterward).  No separate install step.

// Create a persistent profile from a library template (or createProfile() for
// a random one). Library templates aren't launched directly — this freezes an
// enriched copy under a unique id you can return to. Do it once.
const profile = await sdk.createProfile("win-rtx4060");

// Launch + drive in one call. Returns the patchright Browser.
const { browser, session, close } = await sdk.session(profile, {
  proxy: "socks5://user:pass@host:port",
});
try {
  const ctx = browser.contexts()[0];
  const page = await ctx.newPage();
  await page.goto("https://browserleaks.com/quic");
  console.log(await page.title());

  // Inspect what the SDK resolved before launch:
  console.log(session.geo);             // { countryCode: 'DE', timezone: 'Europe/Berlin', ... }
  console.log(session.proxyUdpMs,       // UDP RTT in ms or null
              session.quicEnabled,      // boolean
              session.webrtcMode);      // "auto" | "tcp_only" | "block"
} finally {
  await close();                        // tears down patchright + the engine
}
```

### 随机环境

```ts
// createProfile() with no id freezes a random library template (filter the
// pool with { platform: "Windows" }). It already randomises hw_concurrency /
// RAM / platform_version once, at creation.
const profile = await sdk.createProfile(undefined, { platform: "Windows" });
const { browser, close } = await sdk.session(profile);
try {
  const page = await browser.contexts()[0].newPage();
  // ...
} finally {
  await close();
}
```

### 浏览内置环境

```ts
console.log(sdk.listProfiles().slice(0, 5));
// [ 'linux-gt1030', 'linux-gtx1050', 'mac-m1-air13', 'mac-m1-imac24', 'mac-m1-max-mbp14' ]

console.log(sdk.listProfiles({ platform: "Windows" }).slice(0, 5));

const profile = sdk.randomProfile({ platform: "macOS" });
console.log(profile.id, profile.config.webgl.renderer);
```

### 绑定前校验代理

```ts
console.log(await sdk.checkProxy("socks5://user:pass@host:port"));
// {
//   udpMs: 142,
//   geo: { countryCode: 'DE', timezone: 'Europe/Berlin', ... },
//   wouldEnableQuic: true,
//   wouldSetWebrtc: 'auto',
// }
```

## 持久化环境

`listProfiles()` / `randomProfile()` 返回的是**指纹库模板**——启动其中
一个，每次读取的都是同一个模板。若想要一个可以**回头继续使用**（指纹
*和* cookie/缓存都相同）或可**删除**的环境，请创建*保存的环境*：它会
把一个模板（或随机模板）连同随机化的 hardware/platform_version 一起
冻结到一个全新的唯一 id 下，存入其专属文件夹
`<cache>/profiles/<id>/`，与桌面启动器的 "create profile" 完全一致。

```ts
const sdk = new ShardX();

// Create once (random template, or pass a library id like "win-rtx4060").
const profile = await sdk.createProfile();
console.log(profile.id);

console.log(sdk.listSavedProfiles());     // ['<id>', ...]

// Launch it. randomize stays false → the frozen fingerprint is stable;
// cookies/cache persist in the profile's folder across runs.
const { browser, close } = await sdk.session(profile);
// ...
await close();

// Later — even a different process — reopen by id: same fingerprint + state.
const reopened = sdk.openProfile("<id>");

// Remove the profile and all its state when you're done.
sdk.deleteProfile("<id>");
```

保存的环境从不改动内置的 S3 指纹库：模板以只读形式存放在
`<cache>/fingerprints/*.json`，保存的环境存放在 `<cache>/profiles/<id>/`
（内含 `profile.json` + 浏览器的 user-data-dir）。

## 反指纹噪声

按向量注入的噪声（canvas / WebGL / audio / DOMRect / sensors / fonts）
默认**关闭**。`setNoise(...)` 是**声明式的**——恰好只启用你列出的向量
（带软默认值），其余向量一律关闭：

```ts
profile.setNoise("canvas", "audio", "webgl");   // only these three on
profile.setNoise("canvas");                       // audio + webgl now off again
profile.setNoise();                               // all off
sdk.saveProfile(profile);                          // persist the choice
```

种子在启动时**按环境**派生——跨次运行保持稳定、不同环境互不相同——
因此两个启用了相同向量的环境仍会产生不同的 canvas/audio/WebGL 指纹。
软默认值：WebGL `intensity 0.0005`，DOMRect `maxOffset 1`。

## 启动前检查

每次调用 `sdk.session()` / `sdk.launch()` 都会执行桌面启动器所用的同一
条启动前流水线：

1. **`resolveAutoFields`** —— 若环境的 `timezone`、`navigator.language`
   或 `geolocation.mode` 带有 `"auto"` 哨兵值，SDK 会通过绑定的代理发起
   实时 geo 查询（默认 `ip-api.com`）。具体值会被写回：timezone（取自
   API 返回值；当 geo 提供方未返回 timezone 时，回落到内置的国家码 →
   IANA 时区静态表）、`accept_language` 链、`languages`、`icu_locale`
   （总会被覆盖，以使 `Intl.*` 与 `navigator.language` 一致）以及经纬
   度（lat/lng）。走代理查询失败 → 直连 geo 查询 → 以主机的
   `Intl.DateTimeFormat().resolvedOptions().timeZone` 作为最后兜底。
   解析出的 geo 会暴露在 `session.geo` 上。
2. **`applyScreenStrategy`** —— 见下文。
3. **`probeUdp`** —— SOCKS5 UDP_ASSOCIATE 往返探测。若失败，QUIC 会被
   强制禁用，WebRTC 自动切换为 `tcp_only`。

### 屏幕策略

`session()` / `launch()` 的 `screenMode` 选项：

* **`"profile"`** —— 保持指纹声明的原样。
* **`"cap_to_host"`** —— *macOS 默认。*若主机显示器小于指纹（FP）声明
  的屏幕，则按比例下调 `screen.*` + `window.*`；否则不做任何操作。
* **`"use_host"`** —— *Windows / Linux 默认。*用真实显示器尺寸（扣除
  40 px 的 Windows 任务栏）覆盖 `screen.*`，并重新计算 `window.outer*` /
  `window.inner*`。

默认模式根据 `navigator.platform` 选定。可在每次启动时覆盖：

```ts
await sdk.session(profile, { screenMode: "profile" });
```

### 感知主机的硬件随机化

`randomize: true` 会在启动前重新挑选 `hardware_concurrency`、
`device_memory` 和 `platform_version`——采用与桌面启动器相同的逻辑：

* **macOS** 环境按 id 使用精选的 `MAC_HW_CONFIGS` 表。
* **Windows / Linux** 环境从真实 x86 集合
  `[4, 6, 8, 12, 16, 20, 24, 28, 32]` 中选取落在主机逻辑 CPU 数
  `[host − 4, host + 2]` 范围内的值；`device_memory` 以核心数托底
  （≥ 12 → 16，否则 8），上限由 `hostRamBucketGb()` 决定（从
  `sysctl hw.memsize` / `/proc/meminfo` /
  `Get-CimInstance Win32_ComputerSystem` 归入 8 / 16 / 32 GiB 档）。

因此，在 8 核 / 16 GB 笔记本上启动的环境绝不会声称拥有 32 核 / 128 GB
内存。

### 覆盖指纹字段

```ts
const profile = sdk.library
  .load("win-rtx4060")
  .withOverride({
    name: "my-account",
    timezone: "Europe/Berlin",
    navigator: { language: "de-DE" },
  });

const { browser, close } = await sdk.session(profile, { proxy: "socks5://..." });
```

### 使用你自己的指纹 JSON

```ts
import { Profile } from "@proxyshard/shardx";

const profile = Profile.fromFile("/path/to/my-custom.json");
const { browser, close } = await sdk.session(profile);
```

### WebRTC 策略

```ts
await sdk.session(profile, {
  proxy: "socks5://...",
  webrtc: "tcp_only",                // "auto" (default) | "block" | "tcp_only"
  webrtcPublicIp: "203.0.113.42",    // advertised in ICE candidates
});
```

### 首次下载期间的进度回调

第一次 `session`/`launch`/`listProfiles` 调用会触发下载。可在构造函数
上挂一个进度回调来跟踪它：

```ts
const sdk = new ShardX({
  progress: (label, received, total) => {
    const pct = total ? Math.floor((received / total) * 100) : 0;
    console.log(`${label}: ${pct}%`);
  },
});
const profile = await sdk.createProfile("win-rtx4060");
const { browser, close } = await sdk.session(profile);
```

## 进阶：不使用 patchright 的裸启动

如果你想改用其他 CDP 客户端（原生 `chrome-remote-interface`、
puppeteer-core 的 `connect`、你自己的 WebSocket）驱动浏览器，可跳过
`session()`，直接使用 `launch()`：

```ts
const profile = await sdk.createProfile("win-rtx4060");
const sess = await sdk.launch(profile, { proxy: "socks5://...", cdp: true });
console.log(sess.cdpUrl);              // ws://127.0.0.1:54113/devtools/browser/...
// ... drive it yourself ...
await sess.stop();
```

`launch()` 执行同样的启动前流水线（auto 解析、屏幕策略、UDP 探测、
硬件随机化），并返回一个 `BrowserSession`，带有 `cdpUrl`、`geo`、
`proxyUdpMs`、`quicEnabled`、`webrtcMode`、`userDataDir` 和 `stop()`。

## 缓存布局

```
~/Library/Application Support/shardx-sdk/    (mac)
%LOCALAPPDATA%\shardx-sdk\                   (win)
~/.cache/shardx-sdk/                         (linux)
├── manifest.json             ← etag cache
├── ShardX-Mac-arm64/         ← extracted engine
├── fingerprints/             ← 170 bundled .json templates (read-only)
└── profiles/<id>/            ← saved profile (createProfile) + its state
    ├── profile.json          ← the frozen fingerprint config
    └── …                     ← user-data-dir: cookies, IndexedDB, cache
```

覆盖方式：

```ts
const sdk = new ShardX({ cacheDir: "/data/shardx" });
```

## 更新运行时

SDK 在每个进程的第一次 `session`/`launch`/`listProfiles` 调用时，会通过
一次 GET 获取 GitHub raw 上的 runtime.json 版本清单（其中包含各档案的
etag、chromium 版本和 GREASE 信息）；引擎的重新下载按 chromium 版本比对
触发（磁盘上的引擎版本与清单不一致时重下）。要在进程中途强制重新下载：

```ts
await sdk.runtime.install({ force: true });
```

## 许可证

MIT（本 SDK）。它在运行时下载的 Chromium 分支引擎二进制文件是闭源
产品——引擎许可请参见
[主仓库](https://github.com/ProxyShard/ShardBrowser)。
