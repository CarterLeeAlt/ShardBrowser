# `shardx` — Rust SDK

[ProxyShard](https://proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
团队为 **ShardX 反检测浏览器**提供的自包含 Rust SDK。对外接口与
[Python](../python)、[Node](../node) SDK 相同：首次使用时从 ProxyShard CDN
把引擎、Widevine CDM 和内置指纹库下载到按用户隔离的缓存目录，然后以桌面
启动器所用的一整套伪装参数启动隔离环境。

支持的平台：**macOS arm64**、**Windows x64**、**Linux x64**。
macOS/Linux 上使用系统 `unzip` 解压（保留符号链接和可执行位）——可通过
`brew install unzip` / `apt install unzip` 安装。

## 安装

```toml
[dependencies]
shardx = "0.1"
tokio = { version = "1", features = ["full"] }
```

## 快速开始 —— 启动**并驱动**浏览器

`session()` 一次调用即可启动引擎并附加一个 [chromiumoxide](https://docs.rs/chromiumoxide)
CDP 浏览器（相当于 Python/Node SDK 中 patchright 的 Rust 对应物）。它由
默认的 `control` feature 提供。

```rust
use shardx::{ShardX, ShardXOptions, LaunchOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sdk = ShardX::new(ShardXOptions::default())?;

    // Create a persistent profile from a library template (or None for a
    // random one): enriches a COPY with randomized hw/platform_version and
    // freezes it under a unique id — the fingerprint library is read-only and
    // never launched directly. Do this once per profile.
    let profile = sdk.create_profile(Some("win-rtx4060")).await?;

    // Launch it through a proxy and get a driven browser. randomize: false
    // keeps the frozen fingerprint stable; cookies/cache persist across runs.
    let session = sdk
        .session(
            profile.clone(),
            LaunchOptions {
                proxy: Some("socks5://user:pass@host:1080".into()),
                randomize: false,
                ..Default::default()
            },
        )
        .await?;

    println!(
        "pid={}  quic={}  webrtc={:?}",
        session.engine.pid, session.engine.quic_enabled, session.engine.webrtc_mode,
    );

    // Drive it with chromiumoxide.
    let page = session.new_page("https://example.com").await?;
    println!("title: {:?}", page.get_title().await?);
    // `session.browser` is the full `chromiumoxide::Browser` for anything else.

    session.close().await?; // disconnect + stop the engine
    Ok(())
}
```

向 `create_profile` 传 `None` 可得到**随机**模板。要启动你自己的指纹，请
构造一个 `Profile`（`Profile::from_file(path)` 或
`Profile::new(serde_json::json!({ ... }), None)`）并交给 `session` ——
`launch`/`session` 只接受 `Profile`，从不接受原始指纹库 id。

### 无驱动（更轻量的构建）

如果不需要 CDP 客户端，可禁用该 feature
（`shardx = { version = "0.1", default-features = false }`），改用
`launch`（无 CDP）或 `launch_cdp`（把 `session.cdp_url` 暴露给你自己的
客户端）：

```rust
let profile = sdk.create_profile(None).await?;
let mut engine = sdk.launch_cdp(profile, LaunchOptions::default()).await?;
println!("CDP: {:?}", engine.cdp_url);
engine.stop().await?;
```

## 持久化环境

`list_profiles()` / 随机启动返回的是**指纹库模板**——每次启动都会重新读取
同一个模板。要得到一个可以**回访**（相同指纹*和* cookies/缓存）或**删除**
的环境，请创建*保存的环境*：它把一个模板（或随机模板）连同随机化的
hardware/platform_version 冻结在一个全新唯一 id 下，放入独立目录
`<cache>/profiles/<id>/`，与桌面启动器的“create profile”操作完全一致。

```rust
use shardx::{ShardX, ShardXOptions, LaunchOptions};

let sdk = ShardX::new(ShardXOptions::default())?;

// Create once (random template, or Some("win-rtx4060")).
let mut profile = sdk.create_profile(None).await?;
println!("{}", profile.id);
println!("{:?}", sdk.list_saved_profiles()?);          // ["<id>", ...]

// Launch it — randomize: false keeps the frozen fingerprint stable; cookies /
// cache persist in the profile's folder across runs.
let session = sdk.session(
    profile.clone(),
    LaunchOptions { randomize: false, ..Default::default() },
).await?;
// ... drive it, then session.close().await?; ...

// Later — even another process — reopen by id: same fingerprint + state.
let profile = sdk.open_profile(&profile.id)?;

// Remove the profile and all its state when you're done.
sdk.delete_profile(&profile.id)?;
```

保存的环境从不触碰内置的 S3 指纹库：模板以只读方式留在
`<cache>/fingerprints/*.json`，保存的环境存放在 `<cache>/profiles/<id>/`
（内含 `profile.json` + 浏览器的 user-data-dir）。

## 反指纹噪声

按向量注入的噪声（canvas / WebGL / audio / DOMRect / sensors / fonts）
**默认关闭**。`set_noise(...)` 是**声明式的**——恰好只启用你列出的向量
（附带软默认值），其余全部关闭：

```rust
profile.set_noise(&["canvas", "audio", "webgl"]); // only these three on
profile.set_noise(&["canvas"]);                    // audio + webgl now off again
profile.set_noise(&[]);                            // all off
sdk.save_profile(&profile)?;                        // persist the choice
```

种子在启动时**按环境**派生——跨运行保持稳定、每个环境唯一——因此两个
启用了相同向量的环境仍会产生不同的 canvas/audio/WebGL 指纹。软默认值：
WebGL `intensity 0.0005`，DOMRect `max_offset 1`。

## 绑定前校验代理

```rust
let res = sdk.check_proxy("socks5://user:pass@host:1080").await?;
println!(
    "udp={:?}ms  quic={}  webrtc={:?}  exit={} ({})",
    res.udp_ms, res.would_enable_quic, res.would_set_webrtc,
    res.geo.ip, res.geo.country_code,
);
```

启动器所运行的同一个 SOCKS5 `UDP_ASSOCIATE` 探测决定是否启用 QUIC，以及
WebRTC 是否被强制为 `tcp_only`。

## `launch` 为你做了什么

在拉起引擎之前，SDK 会复现启动器的预检流程：

* **auto-resolve** —— 从*经由所绑定代理*的实时 geo 查询填充 `"auto"`
  时区 / 语言 / 地理位置（[`resolve_auto_fields`]）。
* **屏幕策略** —— macOS 上为 `CapToHost`，Win/Linux 上为 `UseHost`
  （[`apply_screen_strategy`]）；可通过 `LaunchOptions::screen_mode` 覆盖。
* **UDP 探测** —— 依据实时中继探测决定 QUIC + WebRTC 策略。

## 更底层的构建块

门面内部使用的一切都是公开且可复用的：

```rust
use shardx::{Runtime, FingerprintLibrary, parse_proxy, probe_udp, geo_check_via,
             randomize_hardware, host_screen_size};
```

`Runtime`（下载/缓存/解压）、`FingerprintLibrary` + `Profile`、
`parse_proxy` / `proxy_to_arg` / `probe_udp`、`geo_check_via`、
`randomize_hardware` / `randomize_platform_version`、`host_*` 探测，以及
`screen` / `auto_resolve` 辅助函数。

`Runtime` 另有两个只读查询接口，其值来自上游 `runtime.json` 版本清单
（`MANIFEST_URL`），在 `install()` 时设置：

```rust
println!("chromium: {}", sdk.runtime.chromium_version());
println!("grease: {:?}", sdk.runtime.grease());
```

* `Runtime::chromium_version()` —— 引擎的 Chromium 版本（由清单驱动）。
* `Runtime::grease()` —— 清单中的 GREASE `(brand, version)` 元组；GREASE
  品牌/版本随每次发布轮换，无法从版本号推导，因此作为数据随清单一并下发。

## 缓存布局

```
~/Library/Application Support/shardx-sdk/    (mac)
%LOCALAPPDATA%\shardx-sdk\                   (win)
~/.cache/shardx-sdk/                         (linux)
├── manifest.json             ← etag 缓存
├── ShardX-Mac-arm64/         ← 解压后的引擎
├── fingerprints/             ← 内置的 .json 模板（只读）
└── profiles/<id>/            ← 保存的环境（create_profile）及其状态
    ├── profile.json          ← 冻结的指纹配置
    └── …                     ← user-data-dir：cookies、IndexedDB、缓存
```

覆盖默认目录：

```rust
let sdk = ShardX::new(ShardXOptions {
    cache_dir: Some("/data/shardx".into()),
    ..Default::default()
})?;
```

## 运行时更新

运行时更新跟随上游 `runtime.json` 版本清单（`MANIFEST_URL`）。每个进程内
第一次 `session` / `launch` / `list_profiles` 调用都会拉取清单并核对：引擎
按 Chromium 版本比对触发重新下载（基于版本而非 etag），Widevine 随引擎
一起重下，指纹库按 etag 比对。目录替换通过暂存目录 + 回滚目录完成，中断
（断电、杀进程）后下一次调用会自动恢复或清理残留。要在进程内强制重新
下载：

```rust
sdk.runtime.install(true).await?;
```

## 链接

* **站点：** [https://proxyshard.com](https://proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **文档：** [https://docs.proxyshard.com](https://docs.proxyshard.com?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)
* **用法：** [https://docs.proxyshard.com/eng/usage-instructions/shardx-browser](https://docs.proxyshard.com/eng/usage-instructions/shardx-browser?utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher)

采用 MIT 许可证。
