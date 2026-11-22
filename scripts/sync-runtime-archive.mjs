// Snapshot the Windows runtime archives into this repository's
// `runtime-archive` GitHub release.
//
// The self-contained release bundle is assembled from that release's assets
// rather than from git or the live R2 bucket, so this script (run by the
// "Sync runtime archive" workflow) downloads only the bucket objects pinned
// in the source manifest.
//
// Usage:
//   node scripts/sync-runtime-archive.mjs --dir <output-dir>
// Writes the three snapshot-prefixed archives into --dir.
// Publishing to the release is left to the workflow (`gh release upload`).

import { createWriteStream } from "node:fs";
import { mkdir, readFile, stat } from "node:fs/promises";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import {
  BROWSER_ARCHIVE_KEY,
  FINGERPRINTS_ARCHIVE_KEY,
  PUB_BASE,
  WIDEVINE_ARCHIVE_KEY,
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

async function downloadArchive(key, dest, etag, size) {
  const resp = await fetch(`${PUB_BASE}/${key}`, {
    headers: { "If-Match": `"${etag}"` },
    signal: AbortSignal.timeout(300_000),
  });
  if (!resp.ok) {
    throw new Error(`GET ${key}: HTTP ${resp.status}`);
  }
  if (!resp.body) {
    throw new Error(`GET ${key}: empty response body`);
  }
  // Read the etag off the GET response itself: it is guaranteed to describe
  // exactly the bytes just streamed. A separate HEAD can race a bucket update
  // and pair a new etag with old bytes (or vice versa).
  const current = resp.headers.get("etag");
  if (current == null || normalizeEtag(current) !== etag) {
    throw new Error(`GET ${key}: upstream no longer serves the pinned archive ${etag}`);
  }
  await pipeline(Readable.fromWeb(resp.body), createWriteStream(dest));
  const actual = (await stat(dest)).size;
  if (actual !== size) {
    throw new Error(`GET ${key}: downloaded ${actual} bytes, expected ${size}`);
  }
}

const manifest = JSON.parse(await readFile(new URL("../runtime.json", import.meta.url), "utf8"));
const snapshot = manifest.windows_snapshot;
await mkdir(values.dir, { recursive: true });
for (const key of ARCHIVE_KEYS) {
  await withRetries(`download ${key}`, 3, () =>
    downloadArchive(key, join(values.dir, `${snapshot.id}-${key}`), manifest.archives[key], snapshot.sizes[key]),
  );
  console.error(`[sync] ${snapshot.id}-${key}: ${snapshot.sizes[key]} bytes`);
}
