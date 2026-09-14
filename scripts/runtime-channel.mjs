// Constants and helpers for the runtime distribution channel, shared by
// scripts/sync-runtime-archive.mjs and scripts/build-selfcontained.mjs.
//
// The runtime archives and their manifest are upstream's: the manifest lives
// in ProxyShard/ShardBrowser (served through three mirrors) and the archives
// live in an R2 bucket. The sync script snapshots the Windows archives into
// this repository's `runtime-archive` GitHub release, and the build script
// assembles the self-contained release bundle from that snapshot. Keep these
// constants in sync with src-tauri/src/runtime.rs.

export const PUB_BASE = "https://pub-e57a7c60f6934eb09a6600bf2fc59cdc.r2.dev";
export const MANIFEST_REPO = "ProxyShard/ShardBrowser";
export const MANIFEST_BRANCH = "main";
export const MANIFEST_FILE = "runtime.json";

export const BROWSER_ARCHIVE_KEY = "ShardX-Windows.zip";
export const WIDEVINE_ARCHIVE_KEY = "ShardX-Widevine-Win.zip";
export const FINGERPRINTS_ARCHIVE_KEY = "ShardX-Fingerprints.zip";

/// Manifest mirrors in the same priority order as the launcher uses.
export function manifestUrls() {
  return [
    `https://raw.githubusercontent.com/${MANIFEST_REPO}/${MANIFEST_BRANCH}/${MANIFEST_FILE}`,
    `https://api.github.com/repos/${MANIFEST_REPO}/contents/${MANIFEST_FILE}?ref=${MANIFEST_BRANCH}`,
    `https://cdn.jsdelivr.net/gh/${MANIFEST_REPO}@${MANIFEST_BRANCH}/${MANIFEST_FILE}`,
  ];
}

/// ETags travel with and without HTTP quoting; the launcher stores them
/// trimmed (`trim_matches('"')` in runtime.rs), so do the same here.
export function normalizeEtag(value) {
  return value.trim().replace(/^"+/, "").replace(/"+$/, "");
}

/// Fetch the runtime manifest with the launcher's mirror fallback. The GitHub
/// contents API mirror wraps the file in a base64 envelope; the other two
/// serve the JSON directly. Requires `chromium_version` so an HTML error page
/// or API error is rejected and the next mirror is tried instead.
export async function fetchRuntimeManifest() {
  const errors = [];
  for (const url of manifestUrls()) {
    try {
      const resp = await fetch(url, {
        headers: { "user-agent": "ShardBrowser-release-bot" },
        signal: AbortSignal.timeout(20_000),
      });
      if (!resp.ok) {
        errors.push(`${url}: HTTP ${resp.status}`);
        continue;
      }
      let parsed = JSON.parse(await resp.text());
      if (parsed && parsed.encoding === "base64" && typeof parsed.content === "string") {
        const decoded = Buffer.from(parsed.content.replace(/\n/g, ""), "base64");
        parsed = JSON.parse(decoded.toString("utf8"));
      }
      if (typeof parsed?.chromium_version !== "string") {
        errors.push(`${url}: manifest has no chromium_version`);
        continue;
      }
      return parsed;
    } catch (error) {
      errors.push(`${url}: ${error.message}`);
    }
  }
  throw new Error(`All runtime manifest mirrors failed:\n  ${errors.join("\n  ")}`);
}

export async function withRetries(label, attempts, fn) {
  let lastError;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    try {
      return await fn();
    } catch (error) {
      lastError = error;
      if (attempt < attempts) {
        console.error(`[sync] ${label} failed (attempt ${attempt}/${attempts}): ${error.message}; retrying`);
        await new Promise((resolve) => setTimeout(resolve, 2_000 * attempt));
      }
    }
  }
  throw new Error(`${label} failed after ${attempts} attempts: ${lastError.message}`);
}
