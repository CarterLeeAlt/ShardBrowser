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
//   --zips dir containing the runtime-archive release assets:
//          ShardX-Windows.zip, ShardX-Widevine-Win.zip, ShardX-Fingerprints.zip,
//          runtime-archive.json
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
  const ps = `Expand-Archive -LiteralPath '${zipPath}' -DestinationPath '${dest}' -Force`;
  await run("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", ps]);
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
  const ps = `Compress-Archive -Path (Join-Path '${workDir}' '${BUNDLE_DIR_NAME}') -DestinationPath '${destination}' -CompressionLevel Optimal -Force`;
  await run("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", ps]);
}

function compareVersions(a, b) {
  const parts = (value) => value.split(".").map((segment) => Number.parseInt(segment, 10) || 0);
  const [pa, pb] = [parts(a), parts(b)];
  const length = Math.max(pa.length, pb.length);
  for (let index = 0; index < length; index += 1) {
    const diff = (pa[index] ?? 0) - (pb[index] ?? 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

/// Mirror runtime.rs `installed_engine_version`: a dotted-numeric
/// `<version>.manifest` sidecar beside chrome.exe is the on-disk truth.
function detectInstalledEngineVersion(engineDir) {
  let best = null;
  for (const entry of fs.readdirSync(engineDir)) {
    if (!entry.endsWith(".manifest")) continue;
    const stem = entry.slice(0, -".manifest".length);
    if (stem.split(".").length < 2 || !/^[0-9]/.test(stem)) continue;
    if (best === null || compareVersions(stem, best) > 0) best = stem;
  }
  return best;
}

const metaPath = join(zipsDir, "runtime-archive.json");
if (!fs.existsSync(metaPath)) {
  throw new Error(
    `runtime-archive.json is missing in ${zipsDir}; run the 'Sync runtime archive' workflow first.`,
  );
}
const meta = JSON.parse(fs.readFileSync(metaPath, "utf8"));
for (const field of ["browser_etag", "widevine_etag", "fingerprints_etag", "chromium_version"]) {
  if (typeof meta[field] !== "string" || !meta[field]) {
    throw new Error(`runtime-archive.json is missing a valid '${field}'.`);
  }
}
assertNonEmptyFile(exePath, "launcher executable");
for (const key of ["ShardX-Windows.zip", "ShardX-Widevine-Win.zip", "ShardX-Fingerprints.zip"]) {
  assertNonEmptyFile(join(zipsDir, key), `runtime archive ${key}`);
}

const workDir = join(dirname(outZip), ".selfcontained-work");
fs.rmSync(workDir, { recursive: true, force: true });
const bundleRoot = join(workDir, BUNDLE_DIR_NAME);
const dataRoot = join(bundleRoot, "shardx-launcher");
const runtimeRoot = join(dataRoot, "runtime");
const engineDir = join(runtimeRoot, ENGINE_DIR);

log(`extracting browser runtime into ${engineDir}`);
await unzip(join(zipsDir, "ShardX-Windows.zip"), runtimeRoot);

log("placing WidevineCdm beside chrome.exe");
const widevineExtract = join(workDir, "widevine-extract");
await unzip(join(zipsDir, "ShardX-Widevine-Win.zip"), widevineExtract);
const stagedWidevine = join(widevineExtract, WIDEVINE_STAGE_DIR, "WidevineCdm");
if (!fs.existsSync(stagedWidevine)) {
  throw new Error(`${WIDEVINE_STAGE_DIR}/WidevineCdm not found in the Widevine archive`);
}
fs.rmSync(join(engineDir, "WidevineCdm"), { recursive: true, force: true });
fs.renameSync(stagedWidevine, join(engineDir, "WidevineCdm"));
fs.rmSync(widevineExtract, { recursive: true, force: true });

log("seeding the fingerprint library");
const fingerprintsExtract = join(workDir, "fingerprints-extract");
await unzip(join(zipsDir, "ShardX-Fingerprints.zip"), fingerprintsExtract);
// Mirror runtime.rs install_fingerprints: prefer the zip's wrapper directory,
// fall back to the extraction root, copy only top-level *.json files.
const wrapperDir = join(fingerprintsExtract, "shardx-fingerprints");
const templatesDir = fs.existsSync(wrapperDir) ? wrapperDir : fingerprintsExtract;
const fingerprintsDir = join(dataRoot, "fingerprints");
fs.mkdirSync(fingerprintsDir, { recursive: true });
let fingerprintCount = 0;
for (const entry of fs.readdirSync(templatesDir)) {
  if (!entry.endsWith(".json")) continue;
  fs.copyFileSync(join(templatesDir, entry), join(fingerprintsDir, entry));
  fingerprintCount += 1;
}
fs.rmSync(fingerprintsExtract, { recursive: true, force: true });

log("validating the assembled runtime tree");
for (const required of ["chrome.exe", "chrome.dll", "resources.pak"]) {
  assertNonEmptyFile(join(engineDir, required), `engine file ${required}`);
}
assertNonEmptyFile(join(engineDir, "WidevineCdm", "manifest.json"), "Widevine manifest");
if (fingerprintCount === 0) {
  throw new Error("the fingerprint archive yielded no .json templates");
}

const installedVersion = detectInstalledEngineVersion(engineDir) ?? meta.chromium_version;
const manifest = {
  browser_etag: meta.browser_etag,
  widevine_etag: meta.widevine_etag,
  fingerprints_etag: meta.fingerprints_etag,
  applied_chromium_version: meta.chromium_version,
  applied_signature: `${meta.chromium_version}|${meta.grease_brand ?? ""}|${meta.grease_version ?? ""}`,
  installed_chromium_version: installedVersion,
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
