// Runtime cache: download ShardX engine + Widevine CDM + fingerprint
// library from the ProxyShard CDN, extract into a per-user cache dir,
// place Widevine inside the engine bundle, remember etags so subsequent
// runs are zero-network. Mirrors src-tauri/src/runtime.rs in the launcher.
import { closeSync, createWriteStream, existsSync, mkdirSync, openSync, readdirSync, readSync, renameSync, rmSync, statSync, statfsSync, chmodSync, copyFileSync, lstatSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { homedir, platform as osPlatform, arch as osArch } from "node:os";
import { join, dirname, resolve } from "node:path";
import { pipeline } from "node:stream/promises";
import { Readable } from "node:stream";
import { spawnSync } from "node:child_process";
import AdmZip from "adm-zip";
import { atomicWriteFileSync, readJsonWithBackupSync } from "./fsUtil.js";

export const PUB_BASE = "https://pub-e57a7c60f6934eb09a6600bf2fc59cdc.r2.dev";
export const CHROMIUM_VERSION = "149.0.7827.103";
// Version manifest (GitHub raw) — one tiny GET yields every archive's current
// etag, so we never poll R2/S3 (no per-archive HEAD). Changed archives are then
// pulled from PUB_BASE.
export const MANIFEST_URL = "https://raw.githubusercontent.com/ProxyShard/ShardBrowser/main/runtime.json";

export function defaultCacheDir(): string {
  const plat = osPlatform();
  if (plat === "darwin") return join(homedir(), "Library", "Application Support", "shardx-sdk");
  if (plat === "win32")  return join(process.env.LOCALAPPDATA ?? homedir(), "shardx-sdk");
  return join(process.env.XDG_CACHE_HOME ?? join(homedir(), ".cache"), "shardx-sdk");
}

export interface Archive { key: string; label: string; }

export interface HostSpec {
  browser: Archive;
  widevine: Archive | null;
  binarySubpath: string[];
  widevineSubpath: string[];
}

export function hostSpec(): HostSpec {
  const plat = osPlatform();
  const arch = osArch();
  if (plat === "darwin" && arch === "arm64") {
    return {
      browser:  { key: "ShardX-Mac-arm64.zip",          label: "ShardX browser (macOS arm64)" },
      widevine: { key: "ShardX-Widevine-Mac-arm64.zip", label: "Widevine CDM" },
      binarySubpath:   ["ShardX-Mac-arm64", "ShardX.app", "Contents", "MacOS", "ShardX"],
      widevineSubpath: ["ShardX-Mac-arm64", "ShardX.app", "Contents", "Frameworks",
                        "ShardX Framework.framework", "Versions", CHROMIUM_VERSION,
                        "Libraries", "WidevineCdm"],
    };
  }
  if (plat === "win32" && arch === "x64") {
    return {
      browser:  { key: "ShardX-Windows.zip",     label: "ShardX browser (Windows x64)" },
      widevine: { key: "ShardX-Widevine-Win.zip", label: "Widevine CDM" },
      binarySubpath:   ["ShardX-Windows", "chrome.exe"],
      widevineSubpath: ["ShardX-Windows", "WidevineCdm"],
    };
  }
  if (plat === "linux" && arch === "x64") {
    return {
      browser:  { key: "ShardX-Linux.zip",         label: "ShardX browser (Linux x64)" },
      widevine: { key: "ShardX-Widevine-Linux.zip", label: "Widevine CDM" },
      binarySubpath:   ["ShardX-Linux", "chrome"],
      widevineSubpath: ["ShardX-Linux", "WidevineCdm"],
    };
  }
  throw new Error(`Unsupported host: ${plat}/${arch}. ShardX ships mac-arm64, win-x64, linux-x64.`);
}

export const FINGERPRINTS_ARCHIVE: Archive = {
  key: "ShardX-Fingerprints.zip",
  label: "Fingerprint library",
};
const FINGERPRINTS_TOP_DIR = "shardx-fingerprints";
const MAX_ARCHIVE_BYTES = 16 * 1024 * 1024 * 1024;
const MAX_EXTRACTED_BYTES = 32 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES = 250_000;
const FETCH_TIMEOUT_MS = 60_000;

export type ProgressCb = (label: string, received: number, total: number) => void;

interface Manifest {
  browser_etag?: string;
  widevine_etag?: string;
  fingerprints_etag?: string;
  /** Chromium version of the engine binary last extracted on disk. The update
   *  is detected by comparing this (or the on-disk version) to the manifest's
   *  chromium version — robust where the etag check failed. */
  installed_chromium_version?: string;
}

export class Runtime {
  readonly root: string;
  readonly spec: HostSpec;
  private readonly progress?: ProgressCb;
  private readonly _profilesRoot?: string;
  /** Set after a successful in-process install() so subsequent launches
   *  skip the R2 HEAD round-trip (~1 s over a clean connection).  Cleared
   *  by `install({force: true})`. */
  private _checkedInProcess = false;
  /** Engine chromium version from the manifest (fallback to the build-time
   *  constant). Used by launch to normalise profile UA + client_hints. */
  private _chromiumVersion: string = CHROMIUM_VERSION;
  /** GREASE brand/version from the manifest (rotates per release; can't be
   *  derived from the version number). Applied to profiles on launch. */
  private _greaseBrand?: string;
  private _greaseVersion?: string;
  private _installTail: Promise<void> = Promise.resolve();

  constructor(opts: { cacheDir?: string; progress?: ProgressCb; profilesDir?: string } = {}) {
    this.root = opts.cacheDir ?? defaultCacheDir();
    mkdirSync(this.root, { recursive: true });
    this._profilesRoot = opts.profilesDir ? resolve(opts.profilesDir) : undefined;
    this.progress = opts.progress;
    this.spec = hostSpec();
  }

  get manifestPath(): string  { return join(this.root, "manifest.json"); }
  get binaryPath(): string    { return join(this.root, ...this.spec.binarySubpath); }
  get fingerprintsDir(): string {
    const d = join(this.root, "fingerprints");
    mkdirSync(d, { recursive: true });
    return d;
  }
  /** Per-profile user-data-dir root. Defaults to `<cacheDir>/profiles/`;
   *  override via `new ShardX({ profilesDir })` or per-launch
   *  `userDataDir`. Resolved path is logged at launch time. */
  get profilesRoot(): string {
    const d = this._profilesRoot ?? join(this.root, "profiles");
    mkdirSync(d, { recursive: true });
    return d;
  }
  get installed(): boolean    { return existsSync(this.binaryPath); }
  /** Engine chromium version (manifest-driven; set on install()). */
  get chromiumVersion(): string { return this._chromiumVersion; }
  /** GREASE brand from the manifest (e.g. "Not)A;Brand"); set on install(). */
  get greaseBrand(): string | undefined { return this._greaseBrand; }
  /** GREASE version from the manifest (e.g. "24"); set on install(). */
  get greaseVersion(): string | undefined { return this._greaseVersion; }

  /** Chromium version of the engine actually on disk (mac Framework
   *  `Versions/<ver>/`, win `<ver>.manifest`), or undefined on Linux. */
  private installedEngineVersion(): string | undefined {
    try {
      const plat = osPlatform();
      if (plat === "darwin") {
        const versions = join(this.root, "ShardX-Mac-arm64", "ShardX.app", "Contents",
          "Frameworks", "ShardX Framework.framework", "Versions");
        const v = readdirSync(versions).find((n) => n !== "Current" && /^\d/.test(n));
        return v;
      }
      if (plat === "win32") {
        // Only accept a `<version>.manifest` whose stem parses as a version,
        // so a stray/leftover manifest can't feed a bogus version.
        return readdirSync(join(this.root, "ShardX-Windows"))
          .filter((f) => f.endsWith(".manifest"))
          .map((f) => f.replace(/\.manifest$/, ""))
          .find((s) => /^\d/.test(s) && s.includes("."));
      }
      return undefined; // linux: no on-disk version marker
    } catch { return undefined; }
  }

  /** Effective installed version. Trusts the version recorded at install time
   *  (authoritative — written only after a successful extract) over re-reading
   *  it off disk, which can carry stale files from a previous version. On-disk
   *  detection is the fallback for legacy installs with no recorded version. */
  private effectiveInstalledVersion(local: Manifest): string | undefined {
    return local.installed_chromium_version ?? this.installedEngineVersion();
  }

  // ---- manifest ----

  private loadManifest(): Manifest {
    if (!existsSync(this.manifestPath)) return {};
    return readJsonWithBackupSync<Manifest>(this.manifestPath);
  }
  private saveManifest(m: Manifest): void {
    atomicWriteFileSync(this.manifestPath, JSON.stringify(m, null, 2));
  }

  // ---- install ----

  async install(opts: { force?: boolean } = {}): Promise<void> {
    const operation = this._installTail.then(() => this.installLocked(opts));
    this._installTail = operation.catch(() => {});
    return operation;
  }

  private async installLocked(opts: { force?: boolean }): Promise<void> {
    const force = !!opts.force;
    if (this._checkedInProcess && !force) return;
    this.recoverInterruptedSwaps();
    const local = this.loadManifest();
    const remote = await this.fetchManifest();
    // Remember the engine version + grease so launch can normalise profiles.
    this._chromiumVersion = remote.chromiumVersion ?? CHROMIUM_VERSION;
    this._greaseBrand = remote.greaseBrand;
    this._greaseVersion = remote.greaseVersion;

    // Re-download the engine when its on-disk version differs from the
    // manifest's chromium version — VERSION-based, not etag, so it fires for
    // users who updated the SDK but whose stored etag already matched. Manifest
    // unreachable (undefined) → don't force a re-download when installed.
    let needBrowser = force || !this.installed;
    if (!needBrowser && remote.chromiumVersion !== undefined) {
      needBrowser = this.effectiveInstalledVersion(local) !== remote.chromiumVersion;
    }
    if (needBrowser) {
      const installed = await this.installCompleteRuntime();
      local.browser_etag = installed.browserEtag;
      local.widevine_etag = installed.widevineEtag;
    } else if (this.spec.widevine && !local.widevine_etag) {
      local.widevine_etag = await this.installWidevine();
    }
    const remoteFp = remote.archives[FINGERPRINTS_ARCHIVE.key];
    const fpDirHasJson = readdirSync(this.fingerprintsDir).some((f) => f.endsWith(".json"));
    if (force || !fpDirHasJson || (remoteFp !== undefined && local.fingerprints_etag !== remoteFp)) {
      await this.installFingerprints();
      if (remoteFp !== undefined) local.fingerprints_etag = remoteFp;
    }
    // Authoritative: we just extracted exactly this version (old tree wiped
    // first). Recording the known value beats re-reading it off disk.
    local.installed_chromium_version = this._chromiumVersion;
    this.saveManifest(local);

    // Linux/mac archives produced on Windows lose every Unix exec bit;
    // restore +x on every ELF/Mach-O file under the engine tree (not
    // just the main binary — chrome spawns chrome_crashpad_handler,
    // chrome_sandbox, etc., and they need the exec bit too).
    if (osPlatform() !== "win32") {
      fixUnixExecBits(this.root);
    }
    this._checkedInProcess = true;
  }

  // ---- helpers ----

  /** Fetch the version manifest (GitHub raw) — one request that yields every
   *  archive's current etag + the chromium version, replacing per-archive HEADs
   *  against R2/S3. Empty archives / undefined version when unreachable. */
  private async fetchManifest(): Promise<{ archives: Record<string, string>; chromiumVersion?: string; greaseBrand?: string; greaseVersion?: string }> {
    try {
      const r = await fetchWithTimeout(MANIFEST_URL);
      if (!r.ok) return { archives: {} };
      const data = await r.json() as { archives?: Record<string, string>; chromium_version?: string; grease_brand?: string; grease_version?: string };
      const str = (v: unknown) => (typeof v === "string" ? v : undefined);
      return {
        archives: (data && typeof data.archives === "object" && data.archives) || {},
        chromiumVersion: str(data?.chromium_version),
        greaseBrand: str(data?.grease_brand),
        greaseVersion: str(data?.grease_version),
      };
    } catch { return { archives: {} }; }
  }

  private async installCompleteRuntime(): Promise<{ browserEtag: string; widevineEtag?: string }> {
    const stage = join(this.root, ".runtime-stage");
    rmSync(stage, { recursive: true, force: true });
    mkdirSync(stage, { recursive: true });
    try {
      const browserEtag = await this.downloadAndExtract(this.spec.browser, stage);
      let widevineEtag: string | undefined;
      if (this.spec.widevine) {
        widevineEtag = await this.downloadAndExtract(this.spec.widevine, stage);
        this.placeWidevine(stage);
      }
      this.validateRuntime(stage, !!this.spec.widevine);
      if (osPlatform() !== "win32") fixUnixExecBits(stage);
      this.replaceDirectory(
        join(stage, this.spec.binarySubpath[0]),
        join(this.root, this.spec.binarySubpath[0]),
        join(this.root, `.${this.spec.binarySubpath[0]}.rollback`),
      );
      return { browserEtag, widevineEtag };
    } finally {
      rmSync(stage, { recursive: true, force: true });
    }
  }

  private async installWidevine(): Promise<string> {
    if (!this.spec.widevine) throw new Error("Widevine is unavailable on this host");
    const stage = join(this.root, ".runtime-stage");
    rmSync(stage, { recursive: true, force: true });
    mkdirSync(stage, { recursive: true });
    try {
      const etag = await this.downloadAndExtract(this.spec.widevine, stage);
      this.placeWidevine(stage);
      const staged = join(stage, ...this.spec.widevineSubpath);
      this.validateWidevine(staged);
      this.replaceDirectory(
        staged,
        join(this.root, ...this.spec.widevineSubpath),
        join(this.root, ".WidevineCdm.rollback"),
      );
      return etag;
    } finally {
      rmSync(stage, { recursive: true, force: true });
    }
  }

  private replaceDirectory(staged: string, live: string, rollback: string): void {
    if (!existsSync(staged) || !statSync(staged).isDirectory()) {
      throw new Error(`staged Runtime directory is missing: ${staged}`);
    }
    rmSync(rollback, { recursive: true, force: true });
    const hadLive = existsSync(live);
    if (hadLive) renameSync(live, rollback);
    try {
      renameSync(staged, live);
    } catch (error) {
      if (hadLive && existsSync(rollback) && !existsSync(live)) renameSync(rollback, live);
      throw error;
    }
    try {
      rmSync(rollback, { recursive: true, force: true });
    } catch (error) {
      console.warn(`[shardx] installed Runtime but could not remove rollback ${rollback}:`, error);
    }
  }

  private recoverInterruptedSwaps(): void {
    const engine = join(this.root, this.spec.binarySubpath[0]);
    for (const [live, rollback] of [
      [engine, join(this.root, `.${this.spec.binarySubpath[0]}.rollback`)],
      [join(this.root, ...this.spec.widevineSubpath), join(this.root, ".WidevineCdm.rollback")],
    ]) {
      if (!existsSync(rollback)) continue;
      if (existsSync(live)) {
        try { rmSync(rollback, { recursive: true, force: true }); }
        catch (error) { console.warn(`[shardx] old Runtime cleanup is still pending: ${rollback}`, error); }
      } else renameSync(rollback, live);
    }
    rmSync(join(this.root, ".runtime-stage"), { recursive: true, force: true });
  }

  private validateRuntime(root: string, requireWidevine: boolean): void {
    const binary = join(root, ...this.spec.binarySubpath);
    if (!existsSync(binary) || !statSync(binary).isFile() || statSync(binary).size === 0) {
      throw new Error(`staged Runtime binary is missing or empty: ${binary}`);
    }
    if (osPlatform() === "win32") {
      const engine = join(root, this.spec.binarySubpath[0]);
      for (const name of ["chrome.dll", "resources.pak"]) {
        const file = join(engine, name);
        if (!existsSync(file) || !statSync(file).isFile() || statSync(file).size === 0) {
          throw new Error(`staged Runtime file is missing or empty: ${file}`);
        }
      }
    }
    if (requireWidevine) this.validateWidevine(join(root, ...this.spec.widevineSubpath));
  }

  private validateWidevine(root: string): void {
    const manifest = join(root, "manifest.json");
    if (!existsSync(manifest) || !statSync(manifest).isFile() || statSync(manifest).size === 0) {
      throw new Error(`staged Widevine manifest is missing or empty: ${manifest}`);
    }
  }

  private async downloadAndExtract(arch: Archive, dest: string): Promise<string> {
    const url = `${PUB_BASE}/${arch.key}`;
    mkdirSync(dest, { recursive: true });
    const tmp = join(dest, `.${arch.key}.tmp`);

    const controller = new AbortController();
    let inactivityTimer: ReturnType<typeof setTimeout>;
    const resetInactivityTimeout = () => {
      clearTimeout(inactivityTimer);
      inactivityTimer = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
    };
    resetInactivityTimeout();
    try {
      const r = await fetch(url, { signal: controller.signal });
      if (!r.ok || !r.body) throw new Error(`download ${arch.key}: HTTP ${r.status}`);
      const etag = r.headers.get("etag")?.replace(/^"|"$/g, "") ?? "";
      const total = Number(r.headers.get("content-length") ?? 0);
      if (Number.isFinite(total) && total > MAX_ARCHIVE_BYTES) {
        throw new Error(`${arch.key} exceeds the 16 GiB archive limit`);
      }
      let received = 0;
      const reader = r.body.getReader();
      const stream = new Readable({
        async read() {
          try {
            const { value, done } = await reader.read();
            if (done) { this.push(null); return; }
            resetInactivityTimeout();
            received += value.byteLength;
            if (received > MAX_ARCHIVE_BYTES) {
              this.destroy(new Error(`${arch.key} exceeds the 16 GiB archive limit`));
              return;
            }
            this.push(Buffer.from(value));
          } catch (error) {
            this.destroy(error as Error);
          }
        },
      });
      if (this.progress) stream.on("data", () => this.progress!(arch.label, received, total));
      await pipeline(stream, createWriteStream(tmp));
      validateArchive(tmp, dest);
      if (osPlatform() === "win32") new AdmZip(tmp).extractAllTo(dest, true);
      else systemUnzip(tmp, dest);
      return etag;
    } finally {
      clearTimeout(inactivityTimer!);
      rmSync(tmp, { force: true });
    }
  }

  private placeWidevine(root: string): void {
    if (!this.spec.widevine) return;
    const wrapper = this.spec.widevine.key.replace(/\.zip$/, "");
    const src = join(root, wrapper, "WidevineCdm");
    if (!existsSync(src)) throw new Error(`staged Widevine directory is missing: ${src}`);
    const dst = join(root, ...this.spec.widevineSubpath);
    if (existsSync(dst)) rmSync(dst, { recursive: true, force: true });
    mkdirSync(dirname(dst), { recursive: true });
    renameSync(src, dst);
    rmSync(join(root, wrapper), { recursive: true, force: true });
  }

  private async installFingerprints(): Promise<void> {
    const staging = join(this.fingerprintsDir, ".staging");
    if (existsSync(staging)) rmSync(staging, { recursive: true, force: true });
    mkdirSync(staging, { recursive: true });
    try {
      await this.downloadAndExtract(FINGERPRINTS_ARCHIVE, staging);
      const srcDir = join(staging, FINGERPRINTS_TOP_DIR);
      const walk = existsSync(srcDir) ? srcDir : staging;
      for (const name of readdirSync(walk)) {
        if (!name.endsWith(".json")) continue;
        copyFileSync(join(walk, name), join(this.fingerprintsDir, name));
      }
    } finally {
      rmSync(staging, { recursive: true, force: true });
    }
  }
}

async function fetchWithTimeout(url: string): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
  try {
    return await fetch(url, { signal: controller.signal });
  } finally {
    clearTimeout(timer);
  }
}

function validateArchive(archivePath: string, dest: string): void {
  const entries = new AdmZip(archivePath).getEntries();
  if (entries.length > MAX_ARCHIVE_ENTRIES) throw new Error("Runtime archive contains too many entries");
  let extracted = 0;
  for (const entry of entries) {
    const name = entry.entryName;
    if (!name || name.includes("\\") || name.startsWith("/") || /^[A-Za-z]:/.test(name)) {
      throw new Error(`Runtime archive contains an unsafe path: ${name}`);
    }
    const parts = name.replace(/\/$/, "").split("/");
    if (parts.some((part) => !part || part === "." || part === "..")) {
      throw new Error(`Runtime archive contains an unsafe path: ${name}`);
    }
    extracted += entry.header.size;
    if (!Number.isSafeInteger(extracted) || extracted > MAX_EXTRACTED_BYTES) {
      throw new Error("Runtime archive expands beyond the 32 GiB safety limit");
    }
  }
  const fsStats = statfsSync(dest);
  const available = fsStats.bavail * fsStats.bsize;
  if (available < extracted + 512 * 1024 * 1024) {
    throw new Error(`not enough disk space to extract Runtime (${extracted} bytes plus reserve required)`);
  }
}

/** Extract via /usr/bin/unzip — preserves symlinks and permission
 *  bits that adm-zip silently drops.  Required for any macOS .app
 *  bundle (Versions/Current symlinks + Helper exec bits).
 *
 *  Accepts exit code 0 (clean) and 1 (warnings — e.g. "backslashes in
 *  path" for archives zipped on Windows; extraction still completes
 *  correctly).  Only 2+ are real fatal errors per unzip(1). */
function systemUnzip(archive: string, dest: string): void {
  mkdirSync(dest, { recursive: true });
  const r = spawnSync("unzip", ["-q", "-o", archive, "-d", dest], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  if (r.error) {
    if ((r.error as NodeJS.ErrnoException).code === "ENOENT") {
      throw new Error(
        "system `unzip` not found — install with `apt install unzip` / `brew install unzip`",
      );
    }
    throw r.error;
  }
  if ((r.status ?? 0) > 1) {
    const err = r.stderr?.toString().slice(0, 400) ?? `exit ${r.status}`;
    throw new Error(`unzip failed for ${archive} (exit ${r.status}): ${err}`);
  }
}

/** ELF + Mach-O magic bytes; first 4 bytes tell us a file is a native
 *  executable that needs the +x bit, regardless of what zip stored. */
const NATIVE_MAGIC: ReadonlyArray<Buffer> = [
  Buffer.from([0x7f, 0x45, 0x4c, 0x46]),                  // ELF
  Buffer.from([0xfe, 0xed, 0xfa, 0xcf]),                  // Mach-O 64 BE
  Buffer.from([0xcf, 0xfa, 0xed, 0xfe]),                  // Mach-O 64 LE
  Buffer.from([0xfe, 0xed, 0xfa, 0xce]),                  // Mach-O 32 BE
  Buffer.from([0xce, 0xfa, 0xed, 0xfe]),                  // Mach-O 32 LE
  Buffer.from([0xca, 0xfe, 0xba, 0xbe]),                  // Mach-O universal BE
  Buffer.from([0xbe, 0xba, 0xfe, 0xca]),                  // Mach-O universal LE
];

/** Walk `root` and add +x to every file whose first 4 bytes match a known
 *  native-binary magic.  Required because Windows zip producers don't
 *  store Unix exec bits, so chrome / chrome_crashpad_handler / chrome_sandbox
 *  all come out non-executable on Linux. */
function fixUnixExecBits(root: string): void {
  const walk = (dir: string): void => {
    for (const ent of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, ent.name);
      if (ent.isSymbolicLink()) continue;
      if (ent.isDirectory()) { walk(p); continue; }
      if (!ent.isFile()) continue;
      try {
        const fd = openSync(p, "r");
        const buf = Buffer.alloc(4);
        readSync(fd, buf, 0, 4, 0);
        closeSync(fd);
        if (NATIVE_MAGIC.some((m) => buf.equals(m))) {
          chmodSync(p, lstatSync(p).mode | 0o111);
        }
      } catch { /* skip unreadable / racing files */ }
    }
  };
  walk(root);
}
