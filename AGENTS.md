# ShardBrowser Agent Instructions

## 沟通与协作

- 始终使用简体中文回复。
- 用户已经明确要求直接推送时，直接向 `origin/main` 执行 fast-forward 推送；禁止擅自创建分支、推送临时分支、创建 PR 或把 GitHub 自动显示的 PR 链接当作交付结果。
- 除非用户明确要求，不创建 PR、不推送、不提交。
- 执行任何外部 GitHub 操作前，遵循本机网络路由规则；推送前先 `fetch origin main` 并确认只能 fast-forward，禁止 force-push。
- 不触碰未跟踪的 `.zcode/`；不要把它或其他未关联的本地文件纳入提交。

## 发布与版本

- 一次 Release 只有在所有 job 都成功时才算发布成功，必须包含：版本解析、质量门禁、portable EXE 构建、artifact 上传、GitHub Release 创建/上传，以及最后的版本回写提交。
- GitHub Release 或 EXE 已创建但后续版本回写失败时，整次 Release 仍是失败状态。禁止因此直接推进到下一个版本。
- 失败发布的恢复顺序：先精确确认错误版本对应的 Release、tag、公开资产和 Actions artifact；删除错误 Release、tag 与公开资产；将源码版本恢复到原发布版本；使用修复后的流程重新构建并重新发布相同版本。不要跳过失败版本。
- GitHub Actions artifact 的删除和 run 取消需要令牌拥有 Actions 写权限。遇到 `403 Resource not accessible by personal access token` 时，如实报告权限不足；不得声称已取消或已删除，也不得尝试绕过权限。
- 版本回写必须自动同步所有声明：`package.json`、根 `package-lock.json` 的根版本与 `packages[""]` 版本、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`。提交前必须执行 `node scripts/check-version-consistency.mjs`。
- 版本回写逻辑改动必须在隔离副本中实际验证一次，例如 `2.0.5 -> 2.0.6`，并运行版本一致性检查；不要只凭阅读 workflow 就推送。
- Tauri release 构建直接调用本地 CLI：`.\node_modules\.bin\tauri.cmd build --no-bundle --target x86_64-pc-windows-msvc -- --locked`。不要使用 `npx` 或 `npm exec` 传递内部 `-- --locked`，它们会重新解析或丢失参数分隔符。

## 工具链与清理

- 编译、测试和依赖安装只能使用仓库内 `.cache/` 的 Node、Rust、Python、Cargo 与 npm 缓存；不得回退到用户级或系统级开发工具链。
- 代理自行创建的临时测试目录放在项目 `.tmp/`；验证结束后，先确认目标路径和内容，再使用 Windows 回收站 API 清理。禁止使用 `rm`、`del` 或 `Remove-Item` 永久删除。
- 保留 `.cache/`，它承载工作区固定工具链和缓存。根 `node_modules`、`dist`、SDK `node_modules`、SDK `dist` 与 Python `__pycache__` 等仅为本轮验证生成的产物，应在确认后移入回收站。

## 已确认的安全与产品边界

- 同机进程均可信；MCP HTTP 不增加 Bearer Token、mTLS 或额外客户端认证。
- 本地敏感数据优先使用 Windows DPAPI。跨 Windows 用户无法解密时，API secret 重新生成且旧 JWT 失效；代理凭据标记为不可用并要求重新录入。
- 运行时浏览器、Widevine、指纹库和 MCP 压缩包均由上游提供，默认信任上游。不要额外增加签名验真、SHA-256 工件验真、未验证提示或供应链流程改造。
- 发布机器人可以直接向 `main` 写入版本回写提交。
