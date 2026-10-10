# ShardX MCP server

一个 [MCP](https://modelcontextprotocol.io) 服务器，让 AI 客户端
（Claude Desktop、Cursor 等）驱动 **ShardX 启动器**：

- 本地自动化 **HTTP API** —— 创建/编辑/启动/关闭环境（profile），
  管理代理、指纹、文件夹和 cookie；
- 已启动环境的 **CDP 浏览器**，由
  [`patchright`](https://github.com/Kaliiiiiiiiii-Vinyzu/patchright-nodejs)
  （打过反检测补丁的 Playwright）驱动，使自动化不被检测到。

需要 **Node ≥ 18**。应用本身**不会**运行该服务器——它只是把源码下载到
自己的便携数据目录下的 `mcp/`（**Settings → MCP server → Download MCP
server**）。随后由你安装依赖并将其注册到你的 MCP 客户端。

### 1. 安装依赖

`connectOverCDP` 只是*连接*到已在运行的 ShardX 浏览器，因此完全用不到
patchright 自带的 Chromium——安装时跳过浏览器下载，以保持 `node_modules`
精简：

```bash
cd <launcher-data-directory>/mcp
PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 PATCHRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm ci
```

### 2. 在你的 MCP 客户端中注册（stdio）

```json
{
  "mcpServers": {
    "shardx": {
      "command": "node",
      "args": ["/ABSOLUTE/PATH/TO/LAUNCHER-DATA/mcp/index.js"],
      "env": {
        "SHARDX_API": "http://127.0.0.1:40325",
        "SHARDX_TOKEN": "<Bearer token from Settings → Automation API>"
      }
    }
  }
}
```

### HTTP 模式（可选，自托管）

如果你更愿意自行托管并通过 URL 连接，可在设置了 `MCP_HTTP_PORT` 的
情况下运行——此时它会在 `http://127.0.0.1:<port>/mcp` 提供服务：

```bash
MCP_HTTP_PORT=40326 SHARDX_API=http://127.0.0.1:40325 SHARDX_TOKEN=… node index.js
```

## 环境变量

| 变量            | 默认值                   | 说明                                                |
| --------------- | ------------------------ | --------------------------------------------------- |
| `SHARDX_API`    | `http://127.0.0.1:40325` | 启动器 API 的 base URL。                            |
| `SHARDX_TOKEN`  | —                        | Bearer token（Settings 中获取）。必填。             |
| `MCP_HTTP_PORT` | — (stdio)                | 设置后，在 `127.0.0.1:<port>/mcp` 上提供 HTTP 服务。 |

## 工具

**API**

- `list_profiles`, `get_profile`, `create_profile`, `create_temporary_profile`,
  `edit_profile`, `delete_profile`
- `new_fingerprint(platform?)`
- `start_profile(id, headless?)` → 返回 CDP endpoint，
  `stop_profile(id)`, `list_running`
- `list_proxies`, `add_proxy`, `delete_proxy`
- `list_fingerprints`, `list_folders`, `rename_folder`, `delete_folder`
- `export_cookies`, `import_cookies`

**浏览器（通过 patchright 走 CDP）** —— 若环境未在运行，会自动启动该
环境（CDP，可选无头）；操作作用于环境的*活动*标签页：

- 导航：`browser_navigate(url, headless?)`, `browser_back`,
  `browser_forward`, `browser_reload`, `browser_current_url`
- 等待：`browser_wait_for_selector(selector, state?, timeout_ms?)`,
  `browser_wait_for_load(state?)`, `browser_wait(ms)`,
  `browser_wait_for_url(url, timeout_ms?)`, `browser_wait_for_function(expression, timeout_ms?)`
- 读取：`browser_content`, `browser_text`, `browser_get_html(selector?)`,
  `browser_get_text(selector)`, `browser_get_attribute(selector, name)`,
  `browser_exists(selector)`, `browser_count(selector)`,
  `browser_element_state(selector)`, `browser_bounding_box(selector)`,
  `browser_links`, `browser_evaluate(expression)`, `browser_get_cookies`
- 交互：`browser_click(selector)`, `browser_double_click(selector)`,
  `browser_right_click(selector)`, `browser_fill(selector, text)`,
  `browser_type(selector, text, delay_ms?)`, `browser_press(key)`,
  `browser_hover(selector)`, `browser_select_option(selector, value, by?)`,
  `browser_set_checkbox(selector, checked)`, `browser_focus(selector)`,
  `browser_drag(from, to)`, `browser_mouse_click(x, y)`,
  `browser_scroll(selector? | dx/dy)`, `browser_scroll_to_bottom`,
  `browser_set_files(selector, paths)`
- 捕获：`browser_screenshot(full_page?)`,
  `browser_element_screenshot(selector)`, `browser_pdf`（无头），
  `browser_set_viewport(width, height)`
- 存储 / 网络：`browser_set_cookies(cookies)`, `browser_clear_cookies`,
  `browser_local_storage(action, key?, value?)`,
  `browser_set_extra_headers(headers)`, `browser_dialog(action, prompt_text?)`,
  `browser_block_resources(types)`
- 标签页：`browser_list_tabs`, `browser_open_tab(url?)`,
  `browser_switch_tab(index)`, `browser_close_tab(index?)`
- Frame：`browser_frames`, `browser_frame_evaluate(frame, expression)`
- 抓取 / 无障碍：`browser_get_texts(selector)`, `browser_input_value(selector)`,
  `browser_insert_text(text)`, `browser_aria_snapshot(selector?)`
- 网络：`browser_wait_for_response(url_pattern, timeout_ms?)`,
  `browser_capture_start` / `browser_capture_stop`（请求日志），
  `browser_mock(url_pattern, status?, body?, content_type?)` / `browser_unmock(url_pattern?)`,
  `browser_intercept(url_pattern, headers?, post_data?, abort?)`（修改在途请求），
  `browser_set_network_conditions(offline?, latency_ms?, download_kbps?, upload_kbps?)`
- 键盘：`browser_press_on(selector, key)`
- 下载：`browser_wait_for_download(dir, timeout_ms?)`

（所有工具都以 `profile_id` 作为第一个参数。）

## 典型 agent 流程

1. `create_profile`（或 `create_temporary_profile`）—— 可选地附带 `proxy`。
2. `browser_navigate(profile_id, "https://…")` —— 以 CDP 启动浏览器并
   打开页面。
3. `browser_evaluate` / `browser_screenshot` / `browser_click` / `browser_fill`。
4. 用完后 `stop_profile`（临时环境在关闭时会自动删除）。
