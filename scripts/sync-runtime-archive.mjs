// Snapshot the Windows runtime archives into this repository's
// `runtime-archive` GitHub release.
//
// The self-contained release bundle is assembled from that release's assets
// rather than from git or the live R2 bucket, so this script (run by the
// "Sync runtime archive" workflow) downloads the current bucket archives and
// records their metadata at snapshot time. The bundle must carry the
// snapshot's etags — not live bucket HEADs — so a stale snapshot surfaces in
// the launcher as an available update instead of a false "Up to date".
//
// Usage:
//   node scripts/sync-runtime-archive.mjs --dir <output-dir>
// Writes into --dir:
//   ShardX-Windows.zip, ShardX-Widevine-Win.zip, ShardX-Fingerprints.zip,
//   runtime-archive.json
// Publishing to the release is left to the workflow (`gh release upload`).

import { createWriteStream } from "node:fs";
import { mkdir, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import {
  BROWSER_ARCHIVE_KEY,
  FINGERPRINTS_ARCHIVE_KEY,
  PUB_BASE,
  WIDEVINE_ARCHIVE_KEY,
  fetchRuntimeManifest,
  normalizeEtag,
  withRetries,
} from "./runtime-channel.mjs";

const { values } = parseArgs({
  options: { dir: { type: "string" } },
});
if (!values.dir) {
  console.error("Usage: node scripts/sync-runtime-archive.mjs --dir <output-dir>");
  process.exit(1);
}

const ARCHIVE_KEYS = [BROWSER_ARCHIVE_KEY, WIDEVINE_ARCHIVE_KEY, FINGERPRINTS_ARCHIVE_KEY];

async function currentBucketEtag(key) {
  const resp = await fetch(`${PUB_BASE}/${key}`, {
    method: "HEAD",
    signal: AbortSignal.timeout(15_000),
  });
  if (!resp.ok) {
    throw new Error(`HEAD ${key}: HTTP ${resp.status}`);
  }
  const etag = resp.headers.get("etag");
  if (!etag) {
    throw new Error(`HEAD ${key}: response carries no etag`);
  }
  return normalizeEtag(etag);
}

async function downloadArchive(key, dest) {
  // No signal: archive downloads can take minutes on slow links; failures are
  // handled by the caller's retry loop.
  const resp = await fetch(`${PUB_BASE}/${key}`);
  if (!resp.ok) {
    throw new Error(`GET ${key}: HTTP ${resp.status}`);
  }
  if (!resp.body) {
    throw new Error(`GET ${key}: empty response body`);
  }
  // Read the etag off the GET response itself: it is guaranteed to describe
  // exactly the bytes just streamed. A separate HEAD can race a bucket update
  // and pair a new etag with old bytes (or vice versa).
  const etag = resp.headers.get("etag");
  await pipeline(Readable.fromWeb(resp.body), createWriteStream(dest));
  const { size } = await stat(dest);
  if (size === 0) {
    throw new Error(`GET ${key}: downloaded file is empty`);
  }
  const expected = Number(resp.headers.get("content-length"));
  if (Number.isFinite(expected) && expected > 0 && expected !== size) {
    throw new Error(`GET ${key}: downloaded ${size} bytes but content-length was ${expected}`);
  }
  return { etag: etag ? normalizeEtag(etag) : null, size };
}

const manifest = await withRetries("fetch runtime manifest", 3, fetchRuntimeManifest);
await mkdir(values.dir, { recursive: true });

const etags = {};
const sizes = {};
for (const key of ARCHIVE_KEYS) {
  // Download first, then fall back for the etag: the GET response's own etag
  // describes exactly the downloaded bytes. Only when the CDN strips the etag
  // do we fall back to a bucket HEAD (racing a bucket update is possible
  // again) and finally to the manifest's etag map, matching
  // fetch_remote_update_metadata in runtime.rs.
  const downloaded = await withRetries(`download ${key}`, 3, () =>
    downloadArchive(key, join(values.dir, key)),
  );
  let etag = downloaded.etag;
  if (!etag) {
    etag = await withRetries(`etag for ${key}`, 3, () => currentBucketEtag(key)).catch(() => null);
  }
  etag = etag ?? manifest.archives?.[key];
  if (!etag) {
    console.error(`Could not resolve an etag for ${key} from the download, the bucket or the manifest.`);
    process.exit(1);
  }
  etags[key] = etag;
  sizes[key] = downloaded.size;
  console.error(`[sync] ${key}: ${sizes[key]} bytes (etag ${etags[key]})`);
}

const meta = {
  browser_etag: etags[BROWSER_ARCHIVE_KEY],
  widevine_etag: etags[WIDEVINE_ARCHIVE_KEY],
  fingerprints_etag: etags[FINGERPRINTS_ARCHIVE_KEY],
  chromium_version: manifest.chromium_version,
  grease_brand: manifest.grease_brand ?? "",
  grease_version: manifest.grease_version ?? "",
  synced_at: new Date().toISOString(),
};
const metaPath = join(values.dir, "runtime-archive.json");
await writeFile(metaPath, `${JSON.stringify(meta, null, 2)}\n`);
console.error(`[sync] ${metaPath}: chromium ${meta.chromium_version}`);
