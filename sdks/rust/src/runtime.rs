//! Runtime cache: download ShardX engine + Widevine CDM + fingerprint
//! library from the ProxyShard CDN, extract into a per-user cache dir, place
//! Widevine inside the engine bundle, remember etags so subsequent runs are
//! zero-network. Mirrors `src-tauri/src/runtime.rs` in the launcher and the
//! Node/Python SDK `runtime` module.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

pub const PUB_BASE: &str = "https://pub-e57a7c60f6934eb09a6600bf2fc59cdc.r2.dev";
pub const CHROMIUM_VERSION: &str = "149.0.7827.103";
/// Version manifest (GitHub raw) — one tiny GET yields every archive's current
/// etag, so we never poll R2/S3 (no per-archive HEAD).
pub const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/ProxyShard/ShardBrowser/main/runtime.json";

const FINGERPRINTS_KEY: &str = "ShardX-Fingerprints.zip";
const FINGERPRINTS_TOP_DIR: &str = "shardx-fingerprints";
const MAX_ARCHIVE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 250_000;

/// Download-progress callback: `(label, received_bytes, total_bytes)`.
pub type ProgressCb = Arc<dyn Fn(&str, u64, u64) + Send + Sync>;

/// One downloadable archive on the CDN.
#[derive(Clone, Debug)]
pub struct Archive {
    pub key: String,
    pub label: String,
}

/// Per-host archive set + extracted paths.
#[derive(Clone, Debug)]
pub struct HostSpec {
    pub browser: Archive,
    pub widevine: Option<Archive>,
    pub binary_subpath: Vec<String>,
    pub widevine_subpath: Vec<String>,
}

fn arc(key: &str, label: &str) -> Archive {
    Archive {
        key: key.into(),
        label: label.into(),
    }
}

/// Archives + layout for the current host; errors on unsupported platforms.
#[allow(clippy::needless_return)]
pub fn host_spec() -> Result<HostSpec> {
    let p = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Ok(HostSpec {
            browser: arc("ShardX-Mac-arm64.zip", "ShardX browser (macOS arm64)"),
            widevine: Some(arc("ShardX-Widevine-Mac-arm64.zip", "Widevine CDM")),
            binary_subpath: p(&["ShardX-Mac-arm64", "ShardX.app", "Contents", "MacOS", "ShardX"]),
            widevine_subpath: p(&[
                "ShardX-Mac-arm64",
                "ShardX.app",
                "Contents",
                "Frameworks",
                "ShardX Framework.framework",
                "Versions",
                CHROMIUM_VERSION,
                "Libraries",
                "WidevineCdm",
            ]),
        });
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return Ok(HostSpec {
            browser: arc("ShardX-Windows.zip", "ShardX browser (Windows x64)"),
            widevine: Some(arc("ShardX-Widevine-Win.zip", "Widevine CDM")),
            binary_subpath: p(&["ShardX-Windows", "chrome.exe"]),
            widevine_subpath: p(&["ShardX-Windows", "WidevineCdm"]),
        });
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Ok(HostSpec {
            browser: arc("ShardX-Linux.zip", "ShardX browser (Linux x64)"),
            widevine: Some(arc("ShardX-Widevine-Linux.zip", "Widevine CDM")),
            binary_subpath: p(&["ShardX-Linux", "chrome"]),
            widevine_subpath: p(&["ShardX-Linux", "WidevineCdm"]),
        });
    }
    #[allow(unreachable_code)]
    Err(anyhow!(
        "Unsupported host. ShardX ships mac-arm64, win-x64, linux-x64."
    ))
}

/// Default per-user cache dir (mirrors the Node SDK layout, `shardx-sdk`).
#[allow(clippy::needless_return)]
pub fn default_cache_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    {
        return home.join("Library").join("Application Support").join("shardx-sdk");
    }
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.clone())
            .join("shardx-sdk");
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        return std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("shardx-sdk");
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    browser_etag: Option<String>,
    widevine_etag: Option<String>,
    fingerprints_etag: Option<String>,
    /// Chromium version of the engine last written to disk — the fallback when
    /// the on-disk version marker can't be read (e.g. Linux).
    #[serde(default)]
    installed_chromium_version: Option<String>,
}

pub struct Runtime {
    pub root: PathBuf,
    pub spec: HostSpec,
    profiles_override: Option<PathBuf>,
    progress: Option<ProgressCb>,
    checked: AtomicBool,
    install_lock: tokio::sync::Mutex<()>,
    /// Engine chromium version (manifest-driven; set on install()).
    engine_version: std::sync::Mutex<String>,
    /// GREASE brand/version from the manifest (rotates per release; can't be
    /// derived from the version). `(brand, version)`, set on install().
    grease: std::sync::Mutex<(Option<String>, Option<String>)>,
}

impl Runtime {
    pub fn new(
        cache_dir: Option<PathBuf>,
        profiles_dir: Option<PathBuf>,
        progress: Option<ProgressCb>,
    ) -> Result<Self> {
        let root = cache_dir.unwrap_or_else(default_cache_dir);
        fs::create_dir_all(&root).with_context(|| format!("create cache dir {root:?}"))?;
        Ok(Self {
            root,
            spec: host_spec()?,
            profiles_override: profiles_dir,
            progress,
            checked: AtomicBool::new(false),
            install_lock: tokio::sync::Mutex::new(()),
            engine_version: std::sync::Mutex::new(CHROMIUM_VERSION.to_string()),
            grease: std::sync::Mutex::new((None, None)),
        })
    }

    /// GREASE `(brand, version)` from the manifest (set on install()).
    pub fn grease(&self) -> (Option<String>, Option<String>) {
        self.grease.lock().unwrap().clone()
    }

    /// Engine chromium version (manifest-driven; set on install()).
    pub fn chromium_version(&self) -> String {
        self.engine_version.lock().unwrap().clone()
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    pub fn binary_path(&self) -> PathBuf {
        let mut p = self.root.clone();
        for seg in &self.spec.binary_subpath {
            p.push(seg);
        }
        p
    }

    pub fn fingerprints_dir(&self) -> PathBuf {
        let d = self.root.join("fingerprints");
        let _ = fs::create_dir_all(&d);
        d
    }

    /// Per-profile user-data-dir root. `<cache>/profiles/` unless overridden.
    pub fn profiles_root(&self) -> PathBuf {
        let d = self
            .profiles_override
            .clone()
            .unwrap_or_else(|| self.root.join("profiles"));
        let _ = fs::create_dir_all(&d);
        d
    }

    pub fn installed(&self) -> bool {
        self.binary_path().exists()
    }

    /// Chromium version of the engine actually on disk — read from the mac
    /// Framework `Versions/<ver>/` dir or the win `<ver>.manifest` file. `None`
    /// on Linux (no on-disk version marker) or when unreadable.
    fn installed_engine_version(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            let versions = self
                .root
                .join("ShardX-Mac-arm64")
                .join("ShardX.app")
                .join("Contents")
                .join("Frameworks")
                .join("ShardX Framework.framework")
                .join("Versions");
            for ent in fs::read_dir(&versions).ok()?.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if name != "Current" && name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    return Some(name);
                }
            }
            None
        }
        #[cfg(target_os = "windows")]
        {
            // Only accept a `<version>.manifest` whose stem parses as a version,
            // so a stray/leftover manifest can't pin a bogus version.
            for ent in fs::read_dir(self.root.join("ShardX-Windows")).ok()?.flatten() {
                let p = ent.path();
                if p.extension().and_then(|s| s.to_str()) == Some("manifest") {
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        if stem.contains('.')
                            && stem.starts_with(|c: char| c.is_ascii_digit())
                        {
                            return Some(stem.to_string());
                        }
                    }
                }
            }
            None
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            None
        }
    }

    /// Effective installed version. Trusts the version recorded at install time
    /// (authoritative — written only after a successful extract) over re-reading
    /// it off disk, which can carry stale files from a previous version. On-disk
    /// detection is the fallback for legacy installs with no recorded version.
    fn effective_installed_version(&self, local: &Manifest) -> Option<String> {
        local
            .installed_chromium_version
            .clone()
            .or_else(|| self.installed_engine_version())
    }

    fn load_manifest(&self) -> Manifest {
        let path = self.manifest_path();
        if !path.exists() {
            return Manifest::default();
        }
        crate::storage::read_json_with_backup(&path).unwrap_or_default()
    }

    fn save_manifest(&self, m: &Manifest) -> Result<()> {
        crate::storage::atomic_write(
            &self.manifest_path(),
            serde_json::to_string_pretty(m)?.as_bytes(),
        )?;
        Ok(())
    }

    /// Ensure the engine + Widevine + fingerprints are present and current.
    /// Cheap no-op after the first successful call in-process unless `force`.
    pub async fn install(&self, force: bool) -> Result<()> {
        let _install_guard = self.install_lock.lock().await;
        if self.checked.load(Ordering::Relaxed) && !force {
            return Ok(());
        }
        self.recover_interrupted_swaps()?;
        let mut local = self.load_manifest();
        let remote = fetch_manifest().await;
        // Remember the engine version + grease so launch can normalise profiles.
        *self.engine_version.lock().unwrap() = remote
            .chromium_version
            .clone()
            .unwrap_or_else(|| CHROMIUM_VERSION.to_string());
        *self.grease.lock().unwrap() =
            (remote.grease_brand.clone(), remote.grease_version.clone());

        // Re-download when the engine's on-disk version differs from the
        // manifest's chromium version — VERSION-based, not etag, so it fires for
        // users who updated the SDK but whose stored etag already matched. A None
        // manifest (unreachable) must NOT force a re-download when installed.
        let mut need_browser = force || !self.installed();
        if !need_browser {
            if let Some(rv) = remote.chromium_version.as_deref() {
                need_browser = self.effective_installed_version(&local).as_deref() != Some(rv);
            }
        }
        if need_browser {
            let (browser_etag, widevine_etag) = self.install_complete_runtime().await?;
            local.browser_etag = Some(browser_etag);
            local.widevine_etag = widevine_etag;
        } else if local.widevine_etag.is_none() && self.spec.widevine.is_some() {
            local.widevine_etag = Some(self.install_widevine().await?);
        }

        let remote_fp = remote.archives.get(FINGERPRINTS_KEY);
        let fp_has_json = fs::read_dir(self.fingerprints_dir())
            .map(|it| {
                it.flatten()
                    .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            })
            .unwrap_or(false);
        let need_fp = force
            || !fp_has_json
            || remote_fp
                .map(|rf| local.fingerprints_etag.as_deref() != Some(rf.as_str()))
                .unwrap_or(false);
        if need_fp {
            self.install_fingerprints().await?;
            if let Some(rf) = remote_fp {
                local.fingerprints_etag = Some(rf.clone());
            }
        }

        // Authoritative: we just extracted exactly this version (old tree wiped
        // first). Recording the known value beats re-reading it off disk.
        local.installed_chromium_version = Some(self.chromium_version());

        self.save_manifest(&local)?;

        #[cfg(unix)]
        fix_unix_exec_bits(&self.root);

        self.checked.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn install_complete_runtime(&self) -> Result<(String, Option<String>)> {
        let stage = self.root.join(".runtime-stage");
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(&stage)?;
        let result = async {
            let browser_etag = self.download_and_extract(&self.spec.browser, &stage).await?;
            let widevine_etag = if let Some(widevine) = &self.spec.widevine {
                let etag = self.download_and_extract(widevine, &stage).await?;
                self.place_widevine(&stage)?;
                Some(etag)
            } else {
                None
            };
            self.validate_runtime(&stage, self.spec.widevine.is_some())?;
            #[cfg(unix)]
            fix_unix_exec_bits(&stage);
            let engine_name = self
                .spec
                .binary_subpath
                .first()
                .context("Runtime engine path is empty")?;
            replace_directory(
                &stage.join(engine_name),
                &self.root.join(engine_name),
                &self.root.join(format!(".{engine_name}.rollback")),
            )?;
            Ok::<_, anyhow::Error>((browser_etag, widevine_etag))
        }
        .await;
        let _ = fs::remove_dir_all(&stage);
        result
    }

    async fn install_widevine(&self) -> Result<String> {
        let widevine = self.spec.widevine.as_ref().context("Widevine unavailable")?;
        let stage = self.root.join(".runtime-stage");
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(&stage)?;
        let result = async {
            let etag = self.download_and_extract(widevine, &stage).await?;
            self.place_widevine(&stage)?;
            let staged = joined(&stage, &self.spec.widevine_subpath);
            validate_widevine(&staged)?;
            replace_directory(
                &staged,
                &joined(&self.root, &self.spec.widevine_subpath),
                &self.root.join(".WidevineCdm.rollback"),
            )?;
            Ok::<_, anyhow::Error>(etag)
        }
        .await;
        let _ = fs::remove_dir_all(&stage);
        result
    }

    fn recover_interrupted_swaps(&self) -> Result<()> {
        let engine_name = self
            .spec
            .binary_subpath
            .first()
            .context("Runtime engine path is empty")?;
        for (live, rollback) in [
            (
                self.root.join(engine_name),
                self.root.join(format!(".{engine_name}.rollback")),
            ),
            (
                joined(&self.root, &self.spec.widevine_subpath),
                self.root.join(".WidevineCdm.rollback"),
            ),
        ] {
            if !rollback.exists() {
                continue;
            }
            if live.exists() {
                if let Err(error) = fs::remove_dir_all(&rollback) {
                    eprintln!(
                        "[shardx] old Runtime cleanup is still pending for {}: {error}",
                        rollback.display()
                    );
                }
            } else {
                fs::rename(rollback, live)?;
            }
        }
        let _ = fs::remove_dir_all(self.root.join(".runtime-stage"));
        Ok(())
    }

    fn validate_runtime(&self, root: &Path, require_widevine: bool) -> Result<()> {
        let binary = joined(root, &self.spec.binary_subpath);
        let metadata = fs::metadata(&binary)
            .with_context(|| format!("staged Runtime binary is missing: {}", binary.display()))?;
        if !metadata.is_file() || metadata.len() == 0 {
            anyhow::bail!("staged Runtime binary is empty: {}", binary.display());
        }
        #[cfg(windows)]
        {
            let engine = root.join(&self.spec.binary_subpath[0]);
            for name in ["chrome.dll", "resources.pak"] {
                let path = engine.join(name);
                let metadata = fs::metadata(&path)?;
                if !metadata.is_file() || metadata.len() == 0 {
                    anyhow::bail!("staged Runtime file is missing or empty: {}", path.display());
                }
            }
        }
        if require_widevine {
            validate_widevine(&joined(root, &self.spec.widevine_subpath))?;
        }
        Ok(())
    }

    async fn download_and_extract(&self, archive: &Archive, dest: &Path) -> Result<String> {
        let url = format!("{PUB_BASE}/{}", archive.key);
        fs::create_dir_all(dest)?;
        let tmp = dest.join(format!(".{}.tmp", archive.key));

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(60))
            .build()?;
        let mut resp = client
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("download {}", archive.key))?;
        let total = resp.content_length().unwrap_or(0);
        if total > MAX_ARCHIVE_BYTES {
            anyhow::bail!("{} exceeds the 16 GiB archive limit", archive.key);
        }
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_default();

        let operation = async {
            let mut out = tokio::fs::File::create(&tmp).await?;
            let mut received: u64 = 0;
            while let Some(chunk) = resp.chunk().await? {
                received += chunk.len() as u64;
                if received > MAX_ARCHIVE_BYTES {
                    anyhow::bail!("{} exceeds the 16 GiB archive limit", archive.key);
                }
                out.write_all(&chunk).await?;
                if let Some(cb) = &self.progress {
                    cb(&archive.label, received, total);
                }
            }
            out.flush().await?;
            drop(out);

            let zip_path = tmp.clone();
            let dest = dest.to_path_buf();
            tokio::task::spawn_blocking(move || extract_zip(&zip_path, &dest)).await??;
            Ok::<_, anyhow::Error>(etag)
        }
        .await;
        let _ = fs::remove_file(&tmp);
        operation
    }

    async fn install_fingerprints(&self) -> Result<()> {
        let dir = self.fingerprints_dir();
        let staging = dir.join(".staging");
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;
        let archive = Archive {
            key: FINGERPRINTS_KEY.into(),
            label: "Fingerprint library".into(),
        };
        let result = async {
            self.download_and_extract(&archive, &staging).await?;
            let src = staging.join(FINGERPRINTS_TOP_DIR);
            let walk = if src.exists() { src } else { staging.clone() };
            for ent in fs::read_dir(&walk)? {
                let ent = ent?;
                let p = ent.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let dst = dir.join(p.file_name().unwrap());
                fs::copy(&p, &dst)?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = fs::remove_dir_all(&staging);
        result
    }

    fn place_widevine(&self, root: &Path) -> Result<()> {
        let Some(wv) = &self.spec.widevine else { return Ok(()) };
        let wrapper = wv.key.trim_end_matches(".zip");
        let src = root.join(wrapper).join("WidevineCdm");
        if !src.exists() {
            anyhow::bail!("staged Widevine directory is missing: {}", src.display());
        }
        let mut dst = root.to_path_buf();
        for seg in &self.spec.widevine_subpath {
            dst.push(seg);
        }
        if dst.exists() {
            let _ = fs::remove_dir_all(&dst);
        }
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::rename(&src, &dst)?;
        let _ = fs::remove_dir_all(root.join(wrapper));
        Ok(())
    }
}

fn joined(root: &Path, parts: &[String]) -> PathBuf {
    parts.iter().fold(root.to_path_buf(), |path, part| path.join(part))
}

fn validate_widevine(root: &Path) -> Result<()> {
    let manifest = root.join("manifest.json");
    let metadata = fs::metadata(&manifest)
        .with_context(|| format!("staged Widevine manifest is missing: {}", manifest.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        anyhow::bail!("staged Widevine manifest is empty: {}", manifest.display());
    }
    Ok(())
}

fn replace_directory(staged: &Path, live: &Path, rollback: &Path) -> Result<()> {
    if !staged.is_dir() {
        anyhow::bail!("staged Runtime directory is missing: {}", staged.display());
    }
    if rollback.exists() {
        fs::remove_dir_all(rollback)?;
    }
    let had_live = live.exists();
    if had_live {
        fs::rename(live, rollback)?;
    }
    if let Err(error) = fs::rename(staged, live) {
        if had_live {
            if let Err(rollback_error) = fs::rename(rollback, live) {
                anyhow::bail!(
                    "failed to publish Runtime: {error}; restoring the old Runtime also failed: {rollback_error}"
                );
            }
        }
        return Err(error).context("publish staged Runtime");
    }
    if rollback.exists() {
        if let Err(error) = fs::remove_dir_all(rollback) {
            eprintln!(
                "[shardx] installed Runtime but could not remove rollback {}: {error}",
                rollback.display()
            );
        }
    }
    Ok(())
}

#[derive(Default)]
struct RemoteManifest {
    archives: std::collections::HashMap<String, String>,
    chromium_version: Option<String>,
    /// GREASE brand/version (rotates per release; travels in the manifest as
    /// data since it can't be derived from the version number).
    grease_brand: Option<String>,
    grease_version: Option<String>,
}

/// Fetch the version manifest (GitHub raw) — one request that yields every
/// archive's current etag + the chromium version, replacing per-archive HEADs
/// against R2/S3. Empty/None when unreachable.
async fn fetch_manifest() -> RemoteManifest {
    async fn inner() -> Option<RemoteManifest> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .read_timeout(std::time::Duration::from_secs(10))
            .build()
            .ok()?;
        let resp = client.get(MANIFEST_URL).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        let archives = v
            .get("archives")
            .and_then(|a| a.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let str_field = |k: &str| v.get(k).and_then(|s| s.as_str()).map(String::from);
        Some(RemoteManifest {
            archives,
            chromium_version: str_field("chromium_version"),
            grease_brand: str_field("grease_brand"),
            grease_version: str_field("grease_version"),
        })
    }
    inner().await.unwrap_or_default()
}

/// Extract `zip_path` into `dest`. On Unix shell out to system `unzip`
/// (preserves symlinks + exec bits the `zip` crate drops); on Windows use
/// the `zip` crate (no symlinks/exec bits to preserve there).
fn extract_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    validate_zip_archive(&mut archive, cfg!(windows))?;
    #[cfg(unix)]
    {
        use std::process::Command;
        drop(archive);
        let out = Command::new("unzip")
            .arg("-q")
            .arg("-o")
            .arg(zip_path)
            .arg("-d")
            .arg(dest)
            .output()
            .map_err(|e| {
                anyhow!("system `unzip` not found ({e}); install via `apt install unzip` / `brew install unzip`")
            })?;
        let code = out.status.code().unwrap_or(-1);
        if code > 1 {
            anyhow::bail!(
                "unzip failed for {} (exit {}): {}",
                zip_path.display(),
                code,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let relative = entry
                .enclosed_name()
                .context("Runtime archive contains an unsafe path")?
                .to_path_buf();
            let output = dest.join(relative);
            if entry.is_dir() {
                fs::create_dir_all(output)?;
                continue;
            }
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::File::create(output)?;
            std::io::copy(&mut entry, &mut file)?;
            file.sync_all()?;
        }
        Ok(())
    }
}

fn validate_zip_archive<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    reject_symlinks: bool,
) -> Result<()> {
    use std::io::Read;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        anyhow::bail!("Runtime archive contains too many entries");
    }
    let mut extracted = 0u64;
    let mut names = Vec::with_capacity(archive.len());
    let mut symlinks = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.enclosed_name().is_none() || entry.name().contains('\\') {
            anyhow::bail!("Runtime archive contains an unsafe path: {}", entry.name());
        }
        let name = entry.name().trim_end_matches('/').to_string();
        let entry_size = entry.size();
        let is_symlink = entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000);
        if is_symlink {
            if reject_symlinks {
                anyhow::bail!("Runtime archive contains an unsupported symbolic link");
            }
            let mut target = String::new();
            entry.take(4097).read_to_string(&mut target)?;
            let target_path = Path::new(&target);
            if target.len() > 4096
                || target_path.is_absolute()
                || target_path.components().any(|component| {
                    matches!(component, std::path::Component::ParentDir | std::path::Component::Prefix(_))
                })
            {
                anyhow::bail!("Runtime archive contains an unsafe symbolic link: {name}");
            }
            symlinks.push(name.clone());
        }
        extracted = extracted
            .checked_add(entry_size)
            .context("Runtime archive size overflow")?;
        if extracted > MAX_EXTRACTED_BYTES {
            anyhow::bail!("Runtime archive expands beyond the 32 GiB safety limit");
        }
        names.push(name);
    }
    for name in names {
        if symlinks
            .iter()
            .any(|link| name.len() > link.len() && name.starts_with(link) && name.as_bytes()[link.len()] == b'/')
        {
            anyhow::bail!("Runtime archive writes through a symbolic link: {name}");
        }
    }
    Ok(())
}

/// Add +x to every ELF/Mach-O file under `root` (Windows zip producers drop
/// Unix exec bits, so chrome + its helpers come out non-executable).
#[cfg(unix)]
fn fix_unix_exec_bits(root: &Path) {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    const MAGIC: &[[u8; 4]] = &[
        [0x7f, b'E', b'L', b'F'],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xfe, 0xed, 0xfa, 0xce],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
        [0xbe, 0xba, 0xfe, 0xca],
    ];
    fn walk(dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for ent in entries.flatten() {
            let p = ent.path();
            let Ok(ft) = ent.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                walk(&p);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let mut head = [0u8; 4];
            let Ok(mut f) = fs::File::open(&p) else { continue };
            if f.read_exact(&mut head).is_err() {
                continue;
            }
            if !MAGIC.contains(&head) {
                continue;
            }
            if let Ok(meta) = fs::metadata(&p) {
                let mut perm = meta.permissions();
                perm.set_mode(perm.mode() | 0o111);
                let _ = fs::set_permissions(&p, perm);
            }
        }
    }
    walk(root);
}
