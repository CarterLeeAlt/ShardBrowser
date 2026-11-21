import assert from "node:assert/strict";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { Client } from "../mcp/node_modules/@modelcontextprotocol/sdk/dist/esm/client/index.js";
import { StdioClientTransport } from "../mcp/node_modules/@modelcontextprotocol/sdk/dist/esm/client/stdio.js";

const requests = [];
const api = createServer(async (request, response) => {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  const body = Buffer.concat(chunks).toString();
  requests.push({ method: request.method, url: request.url, body: body ? JSON.parse(body) : null });
  response.setHeader("Content-Type", "application/json");
  response.end(JSON.stringify(request.url === "/running" ? [{ profile_id: "running", cdp: null, cdp_error: null }] : { ok: true }));
});
await new Promise(resolve => api.listen(0, "127.0.0.1", resolve));
const client = new Client({ name: "contract-regression", version: "1.0.0" });
const transport = new StdioClientTransport({
  command: process.execPath,
  args: [fileURLToPath(new URL("../mcp/index.js", import.meta.url))],
  env: { ...process.env, SHARDX_API: `http://127.0.0.1:${api.address().port}`, SHARDX_TOKEN: "test-token" },
  stderr: "pipe",
});
try {
  await client.connect(transport);
  const { tools } = await client.listTools();
  for (const name of ["create_profile", "create_temporary_profile", "edit_profile"]) {
    assert.equal(tools.find(tool => tool.name === name).inputSchema.properties.extensions.type, "array");
  }
  for (const [toolName, args, url] of [
    ["create_profile", { fingerprint: {} }, "/profiles"],
    ["create_temporary_profile", {}, "/profiles/temporary"],
    ["edit_profile", { id: "profile" }, "/profiles/profile"],
    ["add_proxy", { host: "example.invalid", port: 1080 }, "/proxies"],
  ]) {
    const name = "  US-West_01 (v2.1) ";
    assert.match(tools.find(tool => tool.name === toolName).inputSchema.properties.name.description, /English letters, digits, spaces/);
    const result = await client.callTool({ name: toolName, arguments: { ...args, name } });
    assert.ok(!result.isError, JSON.stringify(result));
    assert.equal(requests.at(-1).url, url);
    assert.equal(requests.at(-1).body.name, name);
  }
  assert.match(tools.find(tool => tool.name === "delete_profile").description, /7-day recycle bin/);
  for (const [name, args, method, url] of [
    ["list_extensions", {}, "GET", "/extensions"],
    ["import_extension", { path: "fixture.zip" }, "POST", "/extensions"],
    ["delete_extension", { id: "extension" }, "DELETE", "/extensions/extension"],
    ["list_trash", {}, "GET", "/trash"],
    ["restore_trash", { id: "item" }, "POST", "/trash/item/restore"],
    ["purge_trash", { id: "item" }, "DELETE", "/trash/item"],
    ["empty_trash", {}, "DELETE", "/trash"],
  ]) {
    const result = await client.callTool({ name, arguments: args });
    assert.ok(!result.isError, JSON.stringify(result));
    assert.equal(requests.at(-1).method, method); assert.equal(requests.at(-1).url, url);
  }
  const id = "item /? #% 中";
  const segment = "item%20%2F%3F%20%23%25%20%E4%B8%AD";
  for (const [name, args, method, url] of [
    ["get_profile", { id }, "GET", `/profiles/${segment}`],
    ["edit_profile", { id }, "PATCH", `/profiles/${segment}`],
    ["delete_profile", { id }, "DELETE", `/profiles/${segment}`],
    ["start_profile", { id }, "POST", `/profiles/${segment}/start`],
    ["stop_profile", { id }, "POST", `/profiles/${segment}/stop`],
    ["export_cookies", { id }, "GET", `/profiles/${segment}/cookies`],
    ["import_cookies", { id, cookies: [] }, "POST", `/profiles/${segment}/cookies`],
    ["delete_proxy", { id }, "DELETE", `/proxies/${segment}`],
    ["rename_folder", { folder: id, name: "renamed" }, "PATCH", `/folders/${segment}`],
    ["create_profile", { folder: id, fingerprint: {} }, "POST", `/folders/${segment}/profiles`],
    ["delete_folder", { folder: id, delete_profiles: true }, "DELETE", `/folders/${segment}?delete_profiles=true`],
    ["delete_extension", { id }, "DELETE", `/extensions/${segment}`],
    ["restore_trash", { id }, "POST", `/trash/${segment}/restore`],
    ["purge_trash", { id }, "DELETE", `/trash/${segment}`],
  ]) {
    const result = await client.callTool({ name, arguments: args });
    assert.ok(!result.isError, JSON.stringify(result));
    assert.equal(requests.at(-1).method, method);
    assert.equal(requests.at(-1).url, url, name);
  }
  for (const invalid of ["", ".", ".."]) {
    const count = requests.length;
    const result = await client.callTool({ name: "get_profile", arguments: { id: invalid } });
    assert.equal(result.isError, true);
    assert.match(result.content[0].text, /Invalid launcher API path segment/);
    assert.equal(requests.length, count);
  }
  const startWithoutCdp = await client.callTool({ name: "browser_current_url", arguments: { profile_id: id } });
  assert.equal(startWithoutCdp.isError, true);
  assert.match(startWithoutCdp.content[0].text, /running without an available CDP/);
  assert.deepEqual(requests.slice(-2).map(({ method, url }) => [method, url]), [
    ["GET", "/running"], ["POST", `/profiles/${segment}/start`],
  ]);
  await client.callTool({ name: "edit_profile", arguments: { id: "profile", extensions: ["chosen"] } });
  assert.deepEqual(requests.at(-1).body, { extensions: ["chosen"] });
  const result = await client.callTool({ name: "browser_current_url", arguments: { profile_id: "running" } });
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /running without an available CDP/);
  assert.ok(!requests.some(request => request.url === "/profiles/running/start"));
  console.log("MCP regression passed: dynamic API paths, extension arguments, recycle-bin routes, and already-running CDP behavior.");
} finally {
  await client.close();
  await new Promise(resolve => api.close(resolve));
}
