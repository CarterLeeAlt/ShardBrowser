import assert from "node:assert/strict";
import { createHmac, randomBytes } from "node:crypto";
import { spawn } from "node:child_process";
import { mkdir, readFile, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { dirname, join, resolve } from "node:path";
import { setTimeout as wait } from "node:timers/promises";

const executable = resolve(process.argv[2]);
const mode = process.argv[3] ?? "bundle";
assert.ok(["portable", "bundle"].includes(mode), `Unknown acceptance mode: ${mode}`);
const runtime = JSON.parse(await readFile(new URL("../runtime.json", import.meta.url), "utf8"));
const { version } = JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"));
const { chromium_version: expectedEngine } = runtime;
const data = join(dirname(executable), "shardx-launcher");
const secret = randomBytes(32).toString("hex");
function listen(handler) {
  const server = createServer(handler);
  return new Promise((resolve, reject) => server.once("error", reject).listen(0, "127.0.0.1", () => resolve(server)));
}
const closeServer = server => new Promise(resolve => server.close(resolve));
const pageServer = await listen((request, response) => {
  const header = request.headers.dnt ?? "absent";
  if (request.url === "/") {
    response.setHeader("content-type", "text/html; charset=utf-8");
    const escaped = header.replaceAll("&", "&amp;").replaceAll('"', "&quot;");
    response.end(`<!doctype html><body data-dnt="${escaped}">Release check</body>`);
  } else {
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify({ header, acceptLanguage: request.headers["accept-language"] }));
  }
});
const blockedServer = await listen((_, response) => response.end("reachable"));
const portReservation = await listen();
const apiPort = portReservation.address().port;
const pageUrl = `http://127.0.0.1:${pageServer.address().port}`;
const blockedPort = blockedServer.address().port;
const base = `http://127.0.0.1:${apiPort}`;
const encoded = value => Buffer.from(JSON.stringify(value)).toString("base64url");
const now = Math.floor(Date.now() / 1000);
const claims = `${encoded({ alg: "HS256", typ: "JWT" })}.${encoded({ sub: "shardx-api", iat: now, exp: now + 900 })}`;
const token = `${claims}.${createHmac("sha256", secret).update(claims).digest("base64url")}`;

const manifestPath = join(data, "runtime", "manifest.json");
async function readSnapshot() {
  const installed = JSON.parse(await readFile(manifestPath, "utf8"));
  assert.equal(installed.installed_chromium_version, expectedEngine);
  assert.equal(installed.applied_chromium_version, expectedEngine);
  assert.deepEqual(installed.installed_metadata.windows_snapshot, runtime.windows_snapshot);
  assert.equal(installed.browser_etag, runtime.archives["ShardX-Windows.zip"]);
  assert.equal(installed.widevine_etag, runtime.archives["ShardX-Widevine-Win.zip"]);
  assert.equal(installed.fingerprints_etag, runtime.archives["ShardX-Fingerprints.zip"]);
}
if (mode === "bundle") await readSnapshot();
else await mkdir(data);
await writeFile(join(data, "settings.json"), JSON.stringify({ api_secret: secret, api_port: apiPort, minimize_to_tray: false, screen_resolution_mode: "fingerprint" }), { flag: "wx" });
await closeServer(portReservation);
const launcher = spawn(executable, [], { cwd: dirname(executable), stdio: "inherit" });
const exited = new Promise((resolve, reject) => {
  launcher.once("exit", (code, signal) => resolve({ code, signal }));
  launcher.once("error", reject);
});

async function api(path, method = "GET", data) {
  const response = await fetch(`${base}${path}`, {
    method, headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: data === undefined ? undefined : JSON.stringify(data), signal: AbortSignal.timeout(45_000),
  });
  const text = await response.text();
  assert.ok(response.ok, `${method} ${path}: ${response.status} ${text}`);
  return JSON.parse(text);
}

async function withCdp(url, use) {
  const socket = new WebSocket(url);
  try {
    await new Promise((resolve, reject) => {
      socket.addEventListener("open", resolve, { once: true });
      socket.addEventListener("error", reject, { once: true });
      AbortSignal.timeout(5000).addEventListener("abort", () => reject(Error("Browser connection timed out")), { once: true });
    });
    let nextId = 0;
    const call = (method, params) => new Promise((resolve, reject) => {
      const id = ++nextId;
      function finish(error, result) {
        clearTimeout(timer);
        socket.removeEventListener("message", receive);
        if (error) reject(error);
        else resolve(result);
      }
      function receive({ data }) {
        const message = JSON.parse(data);
        if (message.id !== id) return;
        if (message.error || message.result.exceptionDetails) finish(Error(JSON.stringify(message)));
        else finish(null, message.result);
      }
      const timer = setTimeout(() => finish(Error(`${method} timed out`)), 12_000);
      socket.addEventListener("message", receive);
      socket.send(JSON.stringify({ id, method, params }));
    });
    return await use(call, socket);
  } finally { socket.close(); }
}

async function inspect(endpoint) {
  const remote = async (path, method = "GET") => {
    const response = await fetch(`${endpoint.http_url}${path}`, { method, signal: AbortSignal.timeout(5000) });
    assert.ok(response.ok, `${path}: ${response.status}`);
    return response.json();
  };
  const identity = await remote("/json/version");
  assert.equal(identity.Browser, `Chrome/${expectedEngine}`);
  return withCdp(identity.webSocketDebuggerUrl, async browser => {
    await browser("Browser.grantPermissions", { origin: pageUrl, permissions: ["geolocation"] });
    const tab = await remote("/json/new?about:blank", "PUT");
    return inspectTab(tab.webSocketDebuggerUrl);
  });
}

async function inspectTab(url) {
  return withCdp(url, async (call, socket) => {
    await call("Page.enable");
    const loaded = new Promise(resolve => {
      function onMessage({ data }) {
        if (JSON.parse(data).method === "Page.loadEventFired") {
          socket.removeEventListener("message", onMessage);
          resolve(true);
        }
      }
      socket.addEventListener("message", onMessage);
    });
    const navigation = await call("Page.navigate", { url: pageUrl });
    assert.equal(navigation.errorText, undefined);
    assert.ok(await Promise.race([loaded, wait(10_000, false, { ref: false })]), "Test page did not load");
    const { result } = await call("Runtime.evaluate", { awaitPromise: true, returnByValue: true, expression: `(async () => {
          const url = ${JSON.stringify(pageUrl)};
          if (location.origin !== url) throw new Error('Tab is not on the test origin');
          const page = await fetch(url + '/header', { signal: AbortSignal.timeout(5000) });
          const headers = await page.json();
          let reachable;
          try { await fetch('http://127.0.0.1:${blockedPort}/', { mode: 'no-cors', signal: AbortSignal.timeout(5000) }); reachable = true; }
          catch (error) { if (error.name !== 'TypeError') throw error; reachable = false; }
          const geolocation = await new Promise((resolve, reject) => navigator.geolocation.getCurrentPosition(
            ({ coords }) => resolve([coords.latitude, coords.longitude, coords.accuracy]), reject, { timeout: 3000 }));
          return { dnt: navigator.doNotTrack, navigationHeader: document.body.dataset.dnt, ...headers, reachable, language: navigator.language,
            timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone, geolocation, screen: [screen.width, screen.height] };
    })()` });
    return result.value;
  });
}

try {
  let health;
  for (let attempt = 0; attempt < 100 && !health; attempt++) {
    assert.equal(launcher.exitCode, null, "Launcher exited before its local API was ready");
    try { health = await api("/health"); } catch { await wait(250); }
  }
  assert.equal(health?.version, version);
  const unauthenticated = await fetch(`${base}/profiles`, { signal: AbortSignal.timeout(5000) });
  assert.equal(unauthenticated.status, 401);
  if (mode === "portable") {
    const stopAt = Date.now() + 8 * 60_000;
    for (;;) {
      assert.equal(launcher.exitCode, null, "Launcher exited during initial runtime installation");
      if (await stat(manifestPath).catch(error => {
        if (error.code !== "ENOENT") throw error;
      })) break;
      assert.ok(Date.now() < stopAt, "Portable first-run runtime installation did not publish its manifest");
      await wait(1000);
    }
    await readSnapshot();
  }
  const fixture = {
    name: "ReleaseCheck", webrtc: "block", timezone: "Asia/Tokyo", icu_locale: "ja-JP",
    navigator: { language: "ja-JP", languages: ["ja-JP", "ja"], accept_language: "ja-JP,ja;q=0.9" },
    geolocation: { mode: "manual", latitude: 35.68, longitude: 139.77, accuracy: 25 },
    screen: { width: 1366, height: 768, avail_width: 1366, avail_height: 728, color_depth: 24, pixel_depth: 24 },
  };
  const profile = await api("/profiles", "POST", { name: fixture.name, fingerprint: fixture });
  const path = `/profiles/${profile.id}`;
  for (const { enabled, headless } of [{ enabled: false, headless: true }, { enabled: true, headless: false }, { enabled: false, headless: false }, { enabled: true, headless: true }]) {
    await api(path, "PATCH", { fingerprint: { ...fixture, navigator: { ...fixture.navigator, do_not_track: enabled }, network: { blocked_ports: enabled ? [blockedPort] : [] } } });
    const run = await api(`${path}/start`, "POST", { headless });
    try {
      const { acceptLanguage, ...actual } = await inspect(run.cdp);
      const expectedHeader = enabled ? "1" : "absent";
      assert.match(acceptLanguage, /^ja-JP(?:,|$)/);
      assert.deepEqual(actual, { dnt: enabled ? "1" : null, navigationHeader: expectedHeader, header: expectedHeader, reachable: !enabled,
        language: "ja-JP", timeZone: "Asia/Tokyo", geolocation: [35.68, 139.77, 25], screen: [1366, 768] });
      console.log(`PASS ${mode} Launcher ${version} / ShardX ${expectedEngine}: headless=${headless}, DNT=${enabled}, port reachable=${actual.reachable}`);
    } finally { await api(`${path}/stop`, "POST"); }
  }
} finally {
  const stop = spawn("powershell", ["-NoProfile", "-Command", `$p = Get-Process -Id ${launcher.pid} -ErrorAction SilentlyContinue; if ($p) { $null = $p.CloseMainWindow() }`], { stdio: "ignore" });
  await new Promise(resolve => stop.once("exit", resolve));
  const result = await Promise.race([exited, wait(7500)]);
  if (!result) { launcher.kill(); await exited; }
  await closeServer(blockedServer);
  await closeServer(pageServer);
  assert.equal(result?.code, 0, "Release check launcher did not exit cleanly");
}
