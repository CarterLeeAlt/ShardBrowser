// Assemble the self-contained launcher bundle: the portable launcher EXE plus
// the runtime archives snapshotted on the `runtime-archive` GitHub release,
// laid out exactly the way the launcher expects them beside its executable.
// The result is a zip that boots straight to the workspace on first launch —
// `runtime/manifest.json` carries the snapshot's etags and Chromium version,
// so the launcher treats the runtime as installed and its own update check
// keeps working from there.
//
// Windows only: archive extraction/creation goes through 7-Zip (when
// installed) or PowerShell's Expand-Archive/Compress-Archive.
//
// Usage:
//   node scripts/build-selfcontained.mjs --exe <launcher.exe> --zips <dir> --out <bundle.zip>
//   --exe  the built portable launcher (staging/ShardX-Launcher-portable-win-x64.exe)
//   --zips dir containing the snapshot-prefixed assets from runtime.json
//   --out  destination zip (staging/ShardX-Launcher-selfcontained-win-x64.zip)
//
// The bundle layout mirrors the launcher's portable data root (store.rs
// `config_root`): <bundle>/ShardX-Launcher/shardx-launcher/{runtime,fingerprints}.

import { execFile, spawnSync } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { promisify } from "node:util";
import { parseArgs } from "node:util";

const run = promisify(execFile);

const { values } = parseArgs({
  options: {
    exe: { type: "string" },
    zips: { type: "string" },
    out: { type: "string" },
  },
});
if (!values.exe || !values.zips || !values.out) {
  console.error(
    "Usage: node scripts/build-selfcontained.mjs --exe <launcher.exe> --zips <dir> --out <bundle.zip>",
  );
  process.exit(1);
}
if (process.platform !== "win32") {
  console.error("build-selfcontained.mjs runs on Windows only (PowerShell archive tooling).");
  process.exit(1);
}

const exePath = resolve(values.exe);
const zipsDir = resolve(values.zips);
const outZip = resolve(values.out);
const BUNDLE_DIR_NAME = "ShardX-Launcher";
const ENGINE_DIR = "ShardX-Windows";
const WIDEVINE_STAGE_DIR = "ShardX-Widevine-Win";

function log(message) {
  console.error(`[selfcontained] ${message}`);
}

function assertNonEmptyFile(path, label) {
  const meta = fs.statSync(path);
  if (!meta.isFile() || meta.size === 0) {
    throw new Error(`${label} is missing or empty: ${path}`);
  }
}

async function runArchiveTool(file, args, opts) {
  try {
    await run(file, args, opts);
  } catch (error) {
    // 7-Zip exits 1 for recoverable warnings (e.g. duplicate entries).
    if (error.code === 1) return;
    throw error;
  }
}

function sevenZipUsable(candidate) {
  try {
    return !spawnSync(candidate, ["-h"], { stdio: "ignore" }).error;
  } catch {
    return false;
  }
}

function findSevenZip() {
  const candidates = [
    process.env.SEVENZIP,
    "7z",
    "C:\\Program Files\\7-Zip\\7z.exe",
    "C:\\Program Files (x86)\\7-Zip\\7z.exe",
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (candidate.includes("\\") && !fs.existsSync(candidate)) continue;
    if (sevenZipUsable(candidate)) return candidate;
  }
  return null;
}

async function unzip(zipPath, dest) {
  fs.mkdirSync(dest, { recursive: true });
  const seven = findSevenZip();
  if (seven) {
    await runArchiveTool(seven, ["x", "-y", "-bd", `-o${dest}`, zipPath]);
    return;
  }
  log("7-Zip not found; falling back to Expand-Archive (slower)");
  await run("pwsh", ["-NoProfile", "-Command", "$ErrorActionPreference = 'Stop'; Expand-Archive -LiteralPath $env:SHARDX_ZIP -DestinationPath $env:SHARDX_DEST -Force"], {
    env: { ...process.env, SHARDX_ZIP: zipPath, SHARDX_DEST: dest },
  });
}

async function zipBundle(workDir, destination) {
  fs.mkdirSync(dirname(destination), { recursive: true });
  // 7z's `a` adds to an existing archive, so start from a clean file.
  fs.rmSync(destination, { force: true });
  const seven = findSevenZip();
  if (seven) {
    await runArchiveTool(seven, ["a", "-tzip", "-mx=7", "-bd", destination, BUNDLE_DIR_NAME], {
      cwd: workDir,
    });
    return;
  }
  log("7-Zip not found; falling back to Compress-Archive (slower)");
  await run("pwsh", ["-NoProfile", "-Command", "$ErrorActionPreference = 'Stop'; Compress-Archive -LiteralPath $env:SHARDX_BUNDLE -DestinationPath $env:SHARDX_ZIP -CompressionLevel Optimal -Force"], {
    env: { ...process.env, SHARDX_BUNDLE: join(workDir, BUNDLE_DIR_NAME), SHARDX_ZIP: destination },
  });
}

const meta = JSON.parse(fs.readFileSync(new URL("../runtime.json", import.meta.url), "utf8"));
const snapshot = meta.windows_snapshot;
const archivePath = key => join(zipsDir, `${snapshot.id}-${key}`);
assertNonEmptyFile(exePath, "launcher executable");
for (const key of ["ShardX-Windows.zip", "ShardX-Widevine-Win.zip", "ShardX-Fingerprints.zip"]) {
  if (fs.statSync(archivePath(key)).size !== snapshot.sizes[key]) {
    throw new Error(`Runtime archive size differs from the pinned snapshot: ${key}`);
  }
}

const workDir = join(dirname(outZip), ".selfcontained-work");
fs.rmSync(workDir, { recursive: true, force: true });
const bundleRoot = join(workDir, BUNDLE_DIR_NAME);
const dataRoot = join(bundleRoot, "shardx-launcher");
const runtimeRoot = join(dataRoot, "runtime");
const engineDir = join(runtimeRoot, ENGINE_DIR);

log(`extracting browser runtime into ${engineDir}`);
await unzip(archivePath("ShardX-Windows.zip"), runtimeRoot);

log("placing WidevineCdm beside chrome.exe");
const widevineExtract = join(workDir, "widevine-extract");
await unzip(archivePath("ShardX-Widevine-Win.zip"), widevineExtract);
const stagedWidevine = join(widevineExtract, WIDEVINE_STAGE_DIR, "WidevineCdm");
if (!fs.existsSync(stagedWidevine)) {
  throw new Error(`${WIDEVINE_STAGE_DIR}/WidevineCdm not found in the Widevine archive`);
}
fs.rmSync(join(engineDir, "WidevineCdm"), { recursive: true, force: true });
fs.renameSync(stagedWidevine, join(engineDir, "WidevineCdm"));
fs.rmSync(widevineExtract, { recursive: true, force: true });

log("seeding the fingerprint library");
const fingerprintsExtract = join(workDir, "fingerprints-extract");
await unzip(archivePath("ShardX-Fingerprints.zip"), fingerprintsExtract);
// Use top-level JSON templates, inside the archive wrapper when present.
const wrapperDir = join(fingerprintsExtract, "shardx-fingerprints");
const templatesDir = fs.existsSync(wrapperDir) ? wrapperDir : fingerprintsExtract;
const fingerprintsDir = join(dataRoot, "fingerprints");
fs.mkdirSync(fingerprintsDir, { recursive: true });
let fingerprintCount = 0;
for (const entry of fs.readdirSync(templatesDir)) {
  if (!entry.endsWith(".json")) continue;
  const bytes = fs.readFileSync(join(templatesDir, entry));
  const template = JSON.parse(bytes);
  if (!template || typeof template !== "object" || Array.isArray(template)) {
    throw new Error(`Invalid fingerprint template: ${entry}`);
  }
  fs.writeFileSync(join(fingerprintsDir, entry), bytes);
  fingerprintCount += 1;
}
fs.rmSync(fingerprintsExtract, { recursive: true, force: true });

log("validating the assembled runtime tree");
for (const required of ["chrome.exe", "chrome.dll", "resources.pak"]) {
  assertNonEmptyFile(join(engineDir, required), `engine file ${required}`);
}
const cdm = join(engineDir, "WidevineCdm");
const widevine = JSON.parse(fs.readFileSync(join(cdm, "manifest.json"), "utf8"));
if (typeof widevine.version !== "string" || widevine.version.length === 0) {
  throw new Error("Widevine manifest has no version");
}
assertNonEmptyFile(join(cdm, "_platform_specific", "win_x64", "widevinecdm.dll"), "Widevine DLL");
if (fingerprintCount === 0) {
  throw new Error("the fingerprint archive yielded no .json templates");
}

const sidecars = fs.readdirSync(engineDir).filter(name => /^\d+(?:\.\d+){3}\.manifest$/.test(name));
const { stdout } = await run("powershell", ["-NoProfile", "-Command", "$ErrorActionPreference = 'Stop'; (Get-Item -LiteralPath $env:SHARDX_ENGINE_EXE).VersionInfo.ProductVersion"], {
  env: { ...process.env, SHARDX_ENGINE_EXE: join(engineDir, "chrome.exe") },
});
const installedVersion = stdout.trim();
if (installedVersion !== meta.chromium_version || sidecars.length !== 1 || sidecars[0] !== `${installedVersion}.manifest`) {
  throw new Error(`Pinned runtime ${meta.chromium_version}, executable ${installedVersion}, sidecars ${sidecars.join(", ")}`);
}
const manifest = {
  browser_etag: meta.archives["ShardX-Windows.zip"],
  widevine_etag: meta.archives["ShardX-Widevine-Win.zip"],
  fingerprints_etag: meta.archives["ShardX-Fingerprints.zip"],
  applied_chromium_version: meta.chromium_version,
  applied_signature: `${meta.chromium_version}|${meta.grease_brand ?? ""}|${meta.grease_version ?? ""}`,
  installed_chromium_version: installedVersion,
  installed_metadata: meta,
};
await fsp.writeFile(
  join(runtimeRoot, "manifest.json"),
  `${JSON.stringify(manifest, null, 2)}\n`,
);
log(`manifest written: chromium ${installedVersion} (applied ${manifest.applied_signature})`);

fs.copyFileSync(exePath, join(bundleRoot, "ShardX Launcher.exe"));

log(`zipping bundle to ${outZip}`);
await zipBundle(workDir, outZip);
fs.rmSync(workDir, { recursive: true, force: true });

const { size } = await fsp.stat(outZip);
log(`done: ${outZip} (${size} bytes, ${fingerprintCount} fingerprint templates)`);
