"""Runtime cache: download ShardX engine + Widevine CDM + fingerprint library
from the ProxyShard CDN, extract into a per-user cache dir, place Widevine
inside the engine bundle, and remember etags so subsequent runs are
zero-network. Mirrors src-tauri/src/runtime.rs in the launcher."""
from __future__ import annotations

import json
import os
import platform
import shutil
import stat
import subprocess
import sys
import tarfile
import threading
import zipfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Callable, Optional

import httpx

from .storage import atomic_write, read_json_with_backup

PUB_BASE = "https://pub-e57a7c60f6934eb09a6600bf2fc59cdc.r2.dev"
CHROMIUM_VERSION = "149.0.7827.103"
# Version manifest (GitHub raw) — one tiny GET tells us every archive's current
# etag, so we never poll R2/S3 (no per-archive HEAD). Updated archives are then
# pulled from PUB_BASE only when their etag changed.
MANIFEST_URL = "https://raw.githubusercontent.com/ProxyShard/ShardBrowser/main/runtime.json"

# Default cache: ~/Library/Application Support/shardx-sdk (mac),
# %LOCALAPPDATA%\shardx-sdk (win), ~/.cache/shardx-sdk (linux).
def _default_cache_dir() -> Path:
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "shardx-sdk"
    if sys.platform == "win32":
        return Path(os.environ.get("LOCALAPPDATA", Path.home())) / "shardx-sdk"
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "shardx-sdk"

RUNTIME_DIR = _default_cache_dir()


@dataclass(frozen=True)
class Archive:
    key: str           # filename in R2 bucket
    label: str         # human-readable for progress callbacks


@dataclass(frozen=True)
class HostSpec:
    browser: Archive
    widevine: Optional[Archive]
    binary_subpath: tuple[str, ...]   # path under runtime/ to the executable
    widevine_subpath: tuple[str, ...] # destination for the WidevineCdm dir


def host_spec() -> HostSpec:
    sysname = sys.platform
    arch = platform.machine().lower()
    if sysname == "darwin" and arch in ("arm64", "aarch64"):
        return HostSpec(
            browser=Archive("ShardX-Mac-arm64.zip", "ShardX browser (macOS arm64)"),
            widevine=Archive("ShardX-Widevine-Mac-arm64.zip", "Widevine CDM"),
            binary_subpath=("ShardX-Mac-arm64", "ShardX.app", "Contents", "MacOS", "ShardX"),
            widevine_subpath=("ShardX-Mac-arm64", "ShardX.app", "Contents", "Frameworks",
                              "ShardX Framework.framework", "Versions", CHROMIUM_VERSION,
                              "Libraries", "WidevineCdm"),
        )
    if sysname == "win32" and arch in ("amd64", "x86_64"):
        return HostSpec(
            browser=Archive("ShardX-Windows.zip", "ShardX browser (Windows x64)"),
            widevine=Archive("ShardX-Widevine-Win.zip", "Widevine CDM"),
            binary_subpath=("ShardX-Windows", "chrome.exe"),
            widevine_subpath=("ShardX-Windows", "WidevineCdm"),
        )
    if sysname.startswith("linux") and arch in ("x86_64", "amd64"):
        return HostSpec(
            browser=Archive("ShardX-Linux.zip", "ShardX browser (Linux x64)"),
            widevine=Archive("ShardX-Widevine-Linux.zip", "Widevine CDM"),
            binary_subpath=("ShardX-Linux", "chrome"),
            widevine_subpath=("ShardX-Linux", "WidevineCdm"),
        )
    raise RuntimeError(
        f"Unsupported host: {sysname}/{arch}. ShardX ships mac-arm64, win-x64, linux-x64."
    )


FINGERPRINTS_ARCHIVE = Archive("ShardX-Fingerprints.zip", "Fingerprint library")
FINGERPRINTS_TOP_DIR = "shardx-fingerprints"
MAX_ARCHIVE_BYTES = 16 * 1024 * 1024 * 1024
MAX_EXTRACTED_BYTES = 32 * 1024 * 1024 * 1024
MAX_ARCHIVE_ENTRIES = 250_000
DOWNLOAD_TIMEOUT = httpx.Timeout(connect=10.0, read=60.0, write=60.0, pool=10.0)


ProgressCb = Callable[[str, int, int], None]   # (label, received, total)


def apply_engine_version(
    config: dict,
    chromium_version: str,
    grease_brand: Optional[str] = None,
    grease_version: Optional[str] = None,
) -> None:
    """Normalise a profile config's spoofed Chrome version to `chromium_version`
    (e.g. "149.0.7827.103") so it always matches the running engine — bumps
    `navigator.user_agent` (Chrome/<major>.0.0.0) and the version fields in
    `client_hints`: brand_version / brand_full_version / chrome_build /
    chrome_patch (derived from the version) plus, when supplied, grease_brand /
    grease_version / grease_full_version (GREASE rotates per release, so it
    can't be derived — it comes from the manifest). Leaves platform_version,
    architecture, etc. intact. Mutates `config` in place. SDK equivalent of the
    launcher's post-update profile migration."""
    parts = chromium_version.split(".")
    if len(parts) != 4:
        return
    major = parts[0]
    try:
        build = int(parts[2])
        patch = int(parts[3])
    except ValueError:
        build = patch = None

    nav = config.get("navigator")
    if isinstance(nav, dict) and isinstance(nav.get("user_agent"), str):
        ua = nav["user_agent"]
        idx = ua.find("Chrome/")
        if idx >= 0:
            rest = ua[idx + 7:]
            end = rest.find(" ")
            tail = rest[end:] if end >= 0 else ""
            nav["user_agent"] = f"{ua[:idx]}Chrome/{major}.0.0.0{tail}"

    ch = config.get("client_hints")
    if isinstance(ch, dict):
        ch["brand_version"] = major
        ch["brand_full_version"] = chromium_version
        if build is not None:
            ch["chrome_build"] = build
        if patch is not None:
            ch["chrome_patch"] = patch
        if grease_brand:
            ch["grease_brand"] = grease_brand
        if grease_version:
            ch["grease_version"] = grease_version
            ch["grease_full_version"] = f"{grease_version}.0.0.0"


class Runtime:
    """Owns the cache dir and the install/update lifecycle."""

    def __init__(
        self,
        cache_dir: Optional[str | Path] = None,
        progress: Optional[ProgressCb] = None,
        profiles_dir: Optional[str | Path] = None,
    ):
        self.root = Path(cache_dir) if cache_dir else RUNTIME_DIR
        self.root.mkdir(parents=True, exist_ok=True)
        # Per-profile user-data-dir tree.  Defaults to `./shardx-profiles/`
        # next to the running script so the user can find cookies / cache
        # easily; override with `profiles_dir=...`.  Engine assets stay
        # in `cache_dir`.
        self._profiles_root = Path(profiles_dir).resolve() if profiles_dir else None
        self._progress = progress
        self._spec = host_spec()
        # Engine chromium version (manifest-driven; set on install()). Used by
        # launch to normalise profile UA + client_hints to the running engine.
        self._chromium_version = CHROMIUM_VERSION
        # GREASE brand/version from the manifest (rotates per release; can't be
        # derived from the version number). Applied to profiles on launch.
        self._grease_brand: Optional[str] = None
        self._grease_version: Optional[str] = None
        self._install_lock = threading.Lock()
        # Set to True after a successful in-process install() so subsequent
        # launches in the same process skip the R2 HEAD round-trip (~1 s
        # over a clean connection).  Cleared by `install(force=True)`.
        self._checked_in_process = False

    @property
    def profiles_root(self) -> Path:
        d = self._profiles_root if self._profiles_root else self.root / "profiles"
        d.mkdir(parents=True, exist_ok=True)
        return d

    # ---- paths ----

    @property
    def manifest_path(self) -> Path:
        return self.root / "manifest.json"

    @property
    def binary_path(self) -> Path:
        return self.root.joinpath(*self._spec.binary_subpath)

    @property
    def fingerprints_dir(self) -> Path:
        d = self.root / "fingerprints"
        d.mkdir(parents=True, exist_ok=True)
        return d

    @property
    def installed(self) -> bool:
        return self.binary_path.exists()

    @property
    def chromium_version(self) -> str:
        """Engine chromium version (manifest-driven; set on install())."""
        return self._chromium_version

    @property
    def grease_brand(self) -> Optional[str]:
        """GREASE brand from the manifest (e.g. "Not)A;Brand"); set on install()."""
        return self._grease_brand

    @property
    def grease_version(self) -> Optional[str]:
        """GREASE version from the manifest (e.g. "24"); set on install()."""
        return self._grease_version

    def _installed_engine_version(self) -> Optional[str]:
        """Chromium version of the engine actually on disk — read from the
        mac Framework `Versions/<ver>/` dir or the win `<ver>.manifest` file.
        Returns None on Linux (no on-disk version marker) or when unreadable."""
        try:
            if sys.platform == "darwin":
                versions = self.root / "ShardX-Mac-arm64" / "ShardX.app" / "Contents" / \
                    "Frameworks" / "ShardX Framework.framework" / "Versions"
                for entry in versions.iterdir():
                    if entry.name != "Current" and entry.name[:1].isdigit():
                        return entry.name
                return None
            if sys.platform == "win32":
                # Only accept a `<version>.manifest` whose stem parses as a
                # version, so a stray/leftover manifest can't pin a bogus version.
                for entry in (self.root / "ShardX-Windows").iterdir():
                    if entry.suffix == ".manifest" and entry.stem[:1].isdigit() and "." in entry.stem:
                        return entry.stem
                return None
            return None
        except OSError:
            return None

    def _effective_installed_version(self, local: dict) -> Optional[str]:
        """Effective installed version. Trusts the version recorded at install
        time (authoritative — written only after a successful extract) over
        re-reading it off disk, which can carry stale files from a previous
        version. On-disk detection is the fallback for legacy installs."""
        return local.get("installed_chromium_version") or self._installed_engine_version()

    # ---- manifest ----

    def _load_manifest(self) -> dict:
        if not self.manifest_path.exists():
            return {}
        return read_json_with_backup(self.manifest_path)

    def _save_manifest(self, m: dict) -> None:
        atomic_write(self.manifest_path, json.dumps(m, indent=2))

    # ---- install ----

    def install(self, force: bool = False) -> None:
        """Idempotent — re-checks remote etag, skips when nothing changed.
        Within a single process, subsequent calls are no-ops unless `force=True`.
        """
        with self._install_lock:
            self._install_locked(force)

    def _install_locked(self, force: bool) -> None:
        if self._checked_in_process and not force:
            return
        self._recover_interrupted_swaps()
        local = self._load_manifest()
        manifest = self._fetch_manifest()
        remote = manifest.get("archives") if isinstance(manifest.get("archives"), dict) else {}
        # Remember the engine version so launch can normalise profiles to it.
        self._chromium_version = manifest.get("chromium_version") or CHROMIUM_VERSION
        self._grease_brand = manifest.get("grease_brand") or None
        self._grease_version = manifest.get("grease_version") or None
        # Browser. Re-download when the engine's on-disk version differs from
        # the manifest's chromium version — VERSION-based, not etag, so it fires
        # for users who updated the SDK but whose stored etag already matched.
        # A None manifest (unreachable) must NOT force a re-download when installed.
        need_browser = force or not self.installed
        if not need_browser and manifest.get("chromium_version"):
            need_browser = self._effective_installed_version(local) != manifest["chromium_version"]
        if need_browser:
            browser_etag, widevine_etag = self._install_complete_runtime()
            local["browser_etag"] = browser_etag
            local["widevine_etag"] = widevine_etag
        elif self._spec.widevine and not local.get("widevine_etag"):
            local["widevine_etag"] = self._install_widevine()
        # Fingerprints — additive seed (etag changed → re-extract, never
        # overwrites user-renamed files).
        fp_remote = remote.get(FINGERPRINTS_ARCHIVE.key)
        need_fp = force or not any(self.fingerprints_dir.glob("*.json")) or \
            (fp_remote is not None and local.get("fingerprints_etag") != fp_remote)
        if need_fp:
            self._install_fingerprints()
            if fp_remote is not None:
                local["fingerprints_etag"] = fp_remote
        # Authoritative: we just extracted exactly this version (old tree wiped
        # first). Recording the known value beats re-reading it off disk.
        local["installed_chromium_version"] = self._chromium_version
        self._save_manifest(local)
        # Linux/mac archives produced on Windows lose every Unix exec bit;
        # restore +x on every ELF/Mach-O file under the engine tree (not
        # just the main binary — chrome spawns chrome_crashpad_handler,
        # chrome_sandbox, etc., and they need the exec bit too).
        if sys.platform != "win32":
            _fix_unix_exec_bits(self.root)
        self._checked_in_process = True

    def _fetch_manifest(self) -> dict:
        """Fetch the version manifest (GitHub raw) — one request that yields
        every archive's current etag + the chromium version, replacing
        per-archive HEADs against R2/S3. Returns the parsed manifest, or {}
        when unreachable."""
        try:
            with httpx.Client(timeout=8.0, follow_redirects=True) as c:
                r = c.get(MANIFEST_URL)
                if r.status_code != 200:
                    return {}
                data = r.json()
                return data if isinstance(data, dict) else {}
        except Exception:
            return {}

    def _install_complete_runtime(self) -> tuple[str, Optional[str]]:
        stage = self.root / ".runtime-stage"
        shutil.rmtree(stage, ignore_errors=True)
        stage.mkdir(parents=True)
        try:
            browser_etag = self._download_and_extract(self._spec.browser, stage)
            widevine_etag = None
            if self._spec.widevine:
                widevine_etag = self._download_and_extract(self._spec.widevine, stage)
                self._place_widevine(stage)
            self._validate_runtime(stage, self._spec.widevine is not None)
            if sys.platform != "win32":
                _fix_unix_exec_bits(stage)
            self._replace_directory(
                stage / self._spec.binary_subpath[0],
                self.root / self._spec.binary_subpath[0],
                self.root / f".{self._spec.binary_subpath[0]}.rollback",
            )
            return browser_etag, widevine_etag
        finally:
            shutil.rmtree(stage, ignore_errors=True)

    def _install_widevine(self) -> str:
        if not self._spec.widevine:
            raise RuntimeError("Widevine is unavailable on this host")
        stage = self.root / ".runtime-stage"
        shutil.rmtree(stage, ignore_errors=True)
        stage.mkdir(parents=True)
        try:
            etag = self._download_and_extract(self._spec.widevine, stage)
            self._place_widevine(stage)
            staged = stage.joinpath(*self._spec.widevine_subpath)
            self._validate_widevine(staged)
            self._replace_directory(
                staged,
                self.root.joinpath(*self._spec.widevine_subpath),
                self.root / ".WidevineCdm.rollback",
            )
            return etag
        finally:
            shutil.rmtree(stage, ignore_errors=True)

    @staticmethod
    def _replace_directory(staged: Path, live: Path, rollback: Path) -> None:
        if not staged.is_dir():
            raise RuntimeError(f"staged Runtime directory is missing: {staged}")
        shutil.rmtree(rollback, ignore_errors=True)
        had_live = live.exists()
        if had_live:
            live.rename(rollback)
        try:
            staged.rename(live)
        except Exception:
            if had_live and rollback.exists() and not live.exists():
                rollback.rename(live)
            raise
        try:
            shutil.rmtree(rollback)
        except FileNotFoundError:
            pass
        except OSError as error:
            print(f"[shardx] installed Runtime but could not remove rollback {rollback}: {error}", file=sys.stderr)

    def _recover_interrupted_swaps(self) -> None:
        engine = self.root / self._spec.binary_subpath[0]
        pairs = (
            (engine, self.root / f".{self._spec.binary_subpath[0]}.rollback"),
            (self.root.joinpath(*self._spec.widevine_subpath), self.root / ".WidevineCdm.rollback"),
        )
        for live, rollback in pairs:
            if not rollback.exists():
                continue
            if live.exists():
                try:
                    shutil.rmtree(rollback)
                except OSError as error:
                    print(f"[shardx] old Runtime cleanup is still pending for {rollback}: {error}", file=sys.stderr)
            else:
                rollback.rename(live)
        shutil.rmtree(self.root / ".runtime-stage", ignore_errors=True)

    def _validate_runtime(self, root: Path, require_widevine: bool) -> None:
        binary = root.joinpath(*self._spec.binary_subpath)
        if not binary.is_file() or binary.stat().st_size == 0:
            raise RuntimeError(f"staged Runtime binary is missing or empty: {binary}")
        if sys.platform == "win32":
            engine = root / self._spec.binary_subpath[0]
            for name in ("chrome.dll", "resources.pak"):
                path = engine / name
                if not path.is_file() or path.stat().st_size == 0:
                    raise RuntimeError(f"staged Runtime file is missing or empty: {path}")
        if require_widevine:
            self._validate_widevine(root.joinpath(*self._spec.widevine_subpath))

    @staticmethod
    def _validate_widevine(root: Path) -> None:
        manifest = root / "manifest.json"
        if not manifest.is_file() or manifest.stat().st_size == 0:
            raise RuntimeError(f"staged Widevine manifest is missing or empty: {manifest}")

    def _download_and_extract(self, arch: Archive, dest: Path) -> str:
        url = f"{PUB_BASE}/{arch.key}"
        tmp = dest / f".{arch.key}.tmp"
        tmp.parent.mkdir(parents=True, exist_ok=True)
        try:
            with httpx.stream("GET", url, timeout=DOWNLOAD_TIMEOUT, follow_redirects=True) as r:
                r.raise_for_status()
                etag = r.headers.get("etag", "").strip('"')
                total = int(r.headers.get("content-length", 0))
                if total > MAX_ARCHIVE_BYTES:
                    raise RuntimeError(f"{arch.key} exceeds the 16 GiB archive limit")
                received = 0
                with tmp.open("wb") as f:
                    for chunk in r.iter_bytes(chunk_size=1 << 16):
                        received += len(chunk)
                        if received > MAX_ARCHIVE_BYTES:
                            raise RuntimeError(f"{arch.key} exceeds the 16 GiB archive limit")
                        f.write(chunk)
                        if self._progress:
                            self._progress(arch.label, received, total)
                    f.flush()
                    os.fsync(f.fileno())
            _validate_archive(tmp, dest)
            if sys.platform == "win32":
                with zipfile.ZipFile(tmp) as z:
                    z.extractall(dest)
            else:
                _system_unzip(tmp, dest)
            return etag
        finally:
            tmp.unlink(missing_ok=True)

    def _place_widevine(self, root: Path) -> None:
        if not self._spec.widevine:
            return
        # Source dir inside the extracted Widevine archive (mirrors the
        # `ShardX-Widevine-<plat>/WidevineCdm` layout from the launcher).
        wrapper_name = self._spec.widevine.key.removesuffix(".zip")
        src = root / wrapper_name / "WidevineCdm"
        if not src.exists():
            raise RuntimeError(f"staged Widevine directory is missing: {src}")
        dst = root.joinpath(*self._spec.widevine_subpath)
        if dst.exists():
            shutil.rmtree(dst)
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(src), str(dst))
        shutil.rmtree(root / wrapper_name, ignore_errors=True)

    def _install_fingerprints(self) -> None:
        staging = self.fingerprints_dir / ".staging"
        if staging.exists():
            shutil.rmtree(staging)
        staging.mkdir(parents=True, exist_ok=True)
        try:
            self._download_and_extract(FINGERPRINTS_ARCHIVE, staging)
            src_dir = staging / FINGERPRINTS_TOP_DIR
            walk = src_dir if src_dir.exists() else staging
            for p in walk.iterdir():
                if p.suffix == ".json":
                    shutil.copy(p, self.fingerprints_dir / p.name)
        finally:
            shutil.rmtree(staging, ignore_errors=True)


def _validate_archive(archive: Path, dest: Path) -> None:
    with zipfile.ZipFile(archive) as z:
        infos = z.infolist()
        if len(infos) > MAX_ARCHIVE_ENTRIES:
            raise RuntimeError("Runtime archive contains too many entries")
        extracted = 0
        symlinks: set[str] = set()
        for info in infos:
            name = info.filename
            path = PurePosixPath(name.rstrip("/"))
            if (not name or "\\" in name or path.is_absolute()
                    or any(part in ("", ".", "..") for part in path.parts)
                    or (path.parts and ":" in path.parts[0])):
                raise RuntimeError(f"Runtime archive contains an unsafe path: {name}")
            for link in symlinks:
                if name.startswith(f"{link}/"):
                    raise RuntimeError(f"Runtime archive writes through a symbolic link: {name}")
            mode = info.external_attr >> 16
            if stat.S_ISLNK(mode):
                if sys.platform == "win32":
                    raise RuntimeError("Runtime archive contains an unsupported symbolic link")
                target = z.read(info).decode("utf-8", errors="strict")
                target_path = PurePosixPath(target)
                if (target_path.is_absolute() or "\\" in target
                        or any(part in ("", ".", "..") for part in target_path.parts)):
                    raise RuntimeError(f"Runtime archive contains an unsafe symbolic link: {name}")
                symlinks.add(name.rstrip("/"))
            extracted += info.file_size
            if extracted > MAX_EXTRACTED_BYTES:
                raise RuntimeError("Runtime archive expands beyond the 32 GiB safety limit")
        if shutil.disk_usage(dest).free < extracted + 512 * 1024 * 1024:
            raise RuntimeError(
                f"not enough disk space to extract Runtime ({extracted} bytes plus reserve required)"
            )


_NATIVE_MAGIC = (
    b"\x7fELF",                                              # Linux/BSD ELF
    b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe",               # Mach-O 64-bit BE / LE
    b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe",               # Mach-O 32-bit BE / LE
    b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca",               # Mach-O universal
)


def _fix_unix_exec_bits(root: Path) -> None:
    """Walk `root` and add +x to every file whose first 4 bytes are an
    ELF / Mach-O magic.  Required because Windows zip producers don't
    store Unix exec bits, so chrome / chrome_crashpad_handler / chrome_sandbox
    all come out non-executable on Linux."""
    for p in root.rglob("*"):
        try:
            if not p.is_file() or p.is_symlink():
                continue
            with p.open("rb") as f:
                head = f.read(4)
            if any(head.startswith(m) for m in _NATIVE_MAGIC):
                p.chmod(p.stat().st_mode | 0o111)
        except OSError:
            pass


def _system_unzip(archive: Path, dest: Path) -> None:
    """Extract via /usr/bin/unzip — preserves symlinks and permission
    bits that Python's zipfile silently drops.  Required for any
    macOS .app bundle (Versions/Current symlinks + Helper exec bits).

    Accepts exit code 0 (clean) and 1 (warnings — e.g. "backslashes in
    path" for archives zipped on Windows; extraction still completes
    correctly).  Only 2+ are real fatal errors per unzip(1).
    """
    dest.mkdir(parents=True, exist_ok=True)
    try:
        proc = subprocess.run(
            ["unzip", "-q", "-o", str(archive), "-d", str(dest)],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
    except FileNotFoundError as e:
        raise RuntimeError(
            "system `unzip` not found — install with "
            "`apt install unzip` / `brew install unzip`"
        ) from e
    if proc.returncode > 1:
        raise RuntimeError(
            f"unzip failed for {archive.name} (exit {proc.returncode}): "
            f"{proc.stderr.decode(errors='replace')[:400]}"
        )
