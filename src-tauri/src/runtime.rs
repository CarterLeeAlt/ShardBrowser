//! Self-bootstrapping runtime: download ShardX browser + Widevine from R2.
//! Emits `runtime:progress` and `runtime:done` events to the Tauri frontend.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{Emitter, Window};
use tokio::io::AsyncWriteExt;

const PUB_BASE: &str = "https://pub-e57a7c60f6934eb09a6600bf2fc59cdc.r2.dev";
/// Version manifest (GitHub raw) — one tiny GET yields every archive's current
/// etag, so install/status checks never poll R2/S3 per-archive.
const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/ProxyShard/ShardBrowser/main/runtime.json";
const BUNDLED_MANIFEST_JSON: &str = include_str!("../../runtime.json");
/// Chromium version baked into the current Windows runtime bundle.
const CHROMIUM_VERSION: &str = "149.0.7827.103";
const MAX_RUNTIME_ARCHIVE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_RUNTIME_EXTRACTED_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_RUNTIME_ARCHIVE_ENTRIES: usize = 250_000;
const RUNTIME_STAGE_DIR: &str = ".runtime-stage";
const ENGINE_ROLLBACK_DIR: &str = ".ShardX-Windows.rollback";
const WIDEVINE_ROLLBACK_DIR: &str = ".WidevineCdm.rollback";
static RUNTIME_INSTALL_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
    std::sync::OnceLock::new();

fn runtime_install_lock() -> &'static tokio::sync::Mutex<()> {
    RUNTIME_INSTALL_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ArchiveSpec {
    pub key: String,
    pub label: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlatformSpec {
    pub browser: ArchiveSpec,
    pub widevine: ArchiveSpec,
}

/// Archives required by the Windows x64 runtime.
pub fn host_spec() -> PlatformSpec {
    PlatformSpec {
        browser: ArchiveSpec {
            key: "ShardX-Windows.zip".into(),
            label: "ShardX browser (Windows x64)".into(),
        },
        widevine: ArchiveSpec {
            key: "ShardX-Widevine-Win.zip".into(),
            label: "Widevine CDM".into(),
        },
    }
}

/// Portable runtime directory beside the launcher executable.
pub fn runtime_dir() -> Result<PathBuf> {
    Ok(crate::store::config_root()?.join("runtime"))
}

/// Path to the chrome binary inside the extracted runtime.
pub fn binary_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("ShardX-Windows").join("chrome.exe"))
}

fn manifest_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("manifest.json"))
}

/// Top-level dir (under runtime_dir) the engine archive extracts into. Wiped
/// before a re-extract so stale files from the previous version can't linger.
fn engine_root_dir() -> &'static str {
    "ShardX-Windows"
}

// Bundled multi-OS fingerprint library; seeds fingerprints dir on first run.
const FINGERPRINTS_ARCHIVE_KEY: &str = "ShardX-Fingerprints.zip";
const FINGERPRINTS_TOP_DIR: &str = "shardx-fingerprints";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Manifest {
    browser_etag: Option<String>,
    widevine_etag: Option<String>,
    fingerprints_etag: Option<String>,
    /// Chromium version the already-created profiles were last migrated to.
    /// Written after first-time setup or an explicit repair.
    #[serde(default)]
    applied_chromium_version: Option<String>,
    /// Signature (`<version>|<grease_brand>|<grease_version>`) of the engine
    /// descriptor used by the last setup or explicit repair migration.
    #[serde(default)]
    applied_signature: Option<String>,
    /// Chromium version of the engine binary currently extracted on disk.
    /// Manual update checks compare this value to the manifest's
    /// `chromium_version`; startup never performs that comparison.
    #[serde(default)]
    installed_chromium_version: Option<String>,
}

/// Chromium version of the engine actually on disk, read from the Windows
/// `<version>.manifest` sidecar beside chrome.exe.
fn installed_engine_version() -> Option<String> {
    let base = runtime_dir().ok()?;
    let dir = base.join("ShardX-Windows");
    // Require a dotted numeric version so a stray manifest cannot feed a bogus
    // value into the update check.
    let looks_like_version =
        |s: &str| s.split('.').count() >= 2 && s.starts_with(|c: char| c.is_ascii_digit());
    for ent in fs::read_dir(&dir).ok()?.flatten() {
        let p = ent.path();
        if p.extension().and_then(|s| s.to_str()) == Some("manifest") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                if looks_like_version(stem) {
                    return Some(stem.to_string());
                }
            }
        }
    }
    None
}

/// Effective installed engine version. Trusts the version recorded at install
/// time (authoritative — written only after a successful download+extract) over
/// re-reading it from a possibly stale `<old>.manifest`. On-disk detection is
/// the fallback for legacy installs that predate `installed_chromium_version`.
fn effective_installed_version(local: &Manifest) -> Option<String> {
    local
        .installed_chromium_version
        .clone()
        .or_else(installed_engine_version)
}

fn version_is_newer(candidate: &str, installed: Option<&str>) -> bool {
    let Some(installed) = installed else {
        return true;
    };
    let parse = |value: &str| {
        value
            .split('.')
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()
    };
    match (parse(candidate), parse(installed)) {
        (Some(candidate), Some(installed)) => candidate > installed,
        _ => candidate != installed,
    }
}

fn load_manifest() -> Manifest {
    let Ok(p) = manifest_path() else { return Manifest::default() };
    if !p.exists() {
        return Manifest::default();
    }
    match crate::store::load_json_with_backup(&p) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("[runtime] manifest recovery failed: {error}");
            Manifest::default()
        }
    }
}

fn save_manifest(m: &Manifest) -> Result<()> {
    let p = manifest_path()?;
    fs::create_dir_all(p.parent().unwrap())?;
    crate::store::atomic_write(&p, serde_json::to_string_pretty(m)?.as_bytes())?;
    Ok(())
}
#[derive(Serialize, Clone, Debug)]
pub struct RuntimeStatus {
    pub installed: bool,
    pub binary_path: Option<PathBuf>,
    pub initialized: bool,
    pub spec: PlatformSpec,
    pub fingerprints_installed: bool,
    pub widevine_installed: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct RuntimeUpdateStatus {
    pub chromium_installed: bool,
    pub chromium_installed_version: Option<String>,
    pub chromium_latest_version: Option<String>,
    pub chromium_update_available: bool,
    pub fingerprints_installed: bool,
    pub fingerprints_update_available: bool,
    pub widevine_installed: bool,
    pub widevine_update_available: bool,
}

fn fingerprints_are_installed(local: &Manifest) -> bool {
    local.fingerprints_etag.is_some()
        && crate::store::fingerprints_dir()
            .map(|d| {
                fs::read_dir(&d)
                    .map(|it| {
                        it.flatten().any(|e| {
                            e.path().extension().and_then(|s| s.to_str()) == Some("json")
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false)
}

fn widevine_is_installed(local: &Manifest) -> bool {
    local.widevine_etag.is_some()
        && runtime_dir()
            .map(|d| {
                d.join("ShardX-Windows")
                    .join("WidevineCdm")
                    .join("manifest.json")
                    .is_file()
            })
            .unwrap_or(false)
}

fn local_status(local: &Manifest) -> RuntimeStatus {
    let installed = binary_path()
        .ok()
        .and_then(|p| fs::metadata(&p).ok())
        .is_some_and(|meta| meta.is_file() && meta.len() > 0);
    RuntimeStatus {
        installed,
        binary_path: if installed { binary_path().ok() } else { None },
        initialized: local.browser_etag.is_some(),
        spec: host_spec(),
        fingerprints_installed: fingerprints_are_installed(local),
        widevine_installed: widevine_is_installed(local),
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RemoteManifest {
    archives: std::collections::HashMap<String, String>,
    chromium_version: Option<String>,
    /// GREASE brand/version the engine emits in `sec-ch-ua`. Not derivable from
    /// the version number (it rotates per major release), so it travels in the
    /// manifest as data — migration writes it into every profile/fingerprint.
    grease_brand: Option<String>,
    grease_version: Option<String>,
}

fn bundled_manifest() -> RemoteManifest {
    serde_json::from_str(BUNDLED_MANIFEST_JSON).unwrap_or_default()
}

/// Fetch the single remote manifest used for every manually requested update
/// comparison. Startup integrity checks never call this function.
async fn fetch_remote_manifest() -> Option<RemoteManifest> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(4))
        .timeout(std::time::Duration::from_secs(8))
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

fn prefer_newest_manifest(remote: RemoteManifest) -> RemoteManifest {
    let bundled = bundled_manifest();
    let bundled_is_newer = bundled.chromium_version.as_deref().is_some_and(|candidate| {
        version_is_newer(candidate, remote.chromium_version.as_deref())
    });
    if bundled_is_newer {
        eprintln!("[runtime] remote manifest is older than bundled metadata; using bundled");
        bundled
    } else {
        remote
    }
}

/// First-time setup and explicit repair may need CDN metadata. They fall back
/// to the manifest bundled with the launcher when GitHub is unreachable.
async fn fetch_manifest() -> RemoteManifest {
    match fetch_remote_manifest().await {
        Some(remote) => prefer_newest_manifest(remote),
        None => {
            eprintln!("[runtime] remote manifest unavailable; using bundled metadata");
            bundled_manifest()
        }
    }
}

/// Migrate every `*.json` in `dir` to a new engine descriptor: bump
/// `navigator.user_agent` (Chrome/<major>.0.0.0) and the version fields in
/// `client_hints` — `brand_version` / `brand_full_version` / `chrome_build` /
/// `chrome_patch` (derived from the version), plus `grease_brand` /
/// `grease_version` / `grease_full_version` (from the manifest, since GREASE
/// can't be derived from the version number). Leaves platform_version,
/// architecture, webgl, etc. intact. Returns the number of files changed.
fn migrate_dir_to(
    dir: &Path,
    chromium_version: &str,
    grease_brand: Option<&str>,
    grease_version: Option<&str>,
) -> Result<usize> {
    let parts: Vec<&str> = chromium_version.split('.').collect();
    if parts.len() != 4 {
        return Ok(0);
    }
    let major = parts[0];
    let build: i64 = parts[2].parse().unwrap_or(0);
    let patch: i64 = parts[3].parse().unwrap_or(0);

    let mut n = 0usize;
    for ent in fs::read_dir(dir)?.flatten() {
        let p = ent.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&p) else { continue };
        let Ok(mut cfg) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let mut changed = false;

        // navigator.user_agent: replace the Chrome/<ver> token with major.0.0.0.
        if let Some(ua) = cfg
            .pointer("/navigator/user_agent")
            .and_then(|v| v.as_str())
            .map(String::from)
        {
            if let Some(idx) = ua.find("Chrome/") {
                let rest = &ua[idx + 7..];
                let end = rest.find(' ').unwrap_or(rest.len());
                let new_ua = format!("{}Chrome/{}.0.0.0{}", &ua[..idx], major, &rest[end..]);
                if new_ua != ua {
                    if let Some(slot) = cfg.pointer_mut("/navigator/user_agent") {
                        *slot = serde_json::Value::String(new_ua);
                        changed = true;
                    }
                }
            }
        }

        if let Some(ch) = cfg.get_mut("client_hints").and_then(|v| v.as_object_mut()) {
            let mut wants: Vec<(&str, serde_json::Value)> = vec![
                ("brand_version", serde_json::json!(major)),
                ("brand_full_version", serde_json::json!(chromium_version)),
                ("chrome_build", serde_json::json!(build)),
                ("chrome_patch", serde_json::json!(patch)),
            ];
            // GREASE — only when the manifest carries it (rotates per release).
            if let Some(gb) = grease_brand {
                wants.push(("grease_brand", serde_json::json!(gb)));
            }
            if let Some(gv) = grease_version {
                wants.push(("grease_version", serde_json::json!(gv)));
                wants.push(("grease_full_version", serde_json::json!(format!("{gv}.0.0.0"))));
            }
            for (k, want) in wants {
                if ch.get(k) != Some(&want) {
                    ch.insert(k.to_string(), want);
                    changed = true;
                }
            }
        }

        if changed {
            crate::store::atomic_write(&p, serde_json::to_string_pretty(&cfg)?.as_bytes())?;
            n += 1;
        }
    }
    Ok(n)
}

/// Migrate both the saved profiles AND the fingerprint library (bundled +
/// user-added) to `chromium_version`. Bundled templates are already at the new
/// version after the seed; user-added fingerprints get their UA + client_hints
/// bumped here (their custom fields are preserved).
fn migrate_all_to(
    chromium_version: &str,
    grease_brand: Option<&str>,
    grease_version: Option<&str>,
) -> usize {
    let mut n = 0;
    if let Ok(d) = crate::store::profiles_dir() {
        n += migrate_dir_to(&d, chromium_version, grease_brand, grease_version).unwrap_or(0);
    }
    if let Ok(d) = crate::store::fingerprints_dir() {
        n += migrate_dir_to(&d, chromium_version, grease_brand, grease_version).unwrap_or(0);
    }
    n
}

/// Fast startup integrity check. It only reads local files and never contacts
/// GitHub or compares versions.
#[tauri::command]
pub fn runtime_local_status() -> Result<RuntimeStatus, String> {
    let base = runtime_dir().map_err(|error| error.to_string())?;
    fs::create_dir_all(&base).map_err(|error| error.to_string())?;
    if let Ok(_guard) = runtime_install_lock().try_lock() {
        recover_interrupted_runtime_swap(&base).map_err(|error| error.to_string())?;
    }
    let local = load_manifest();
    Ok(local_status(&local))
}

/// User-triggered update check. One manifest request compares Chromium,
/// fingerprint templates and Widevine without downloading or installing files.
#[tauri::command]
pub async fn runtime_check_updates() -> Result<RuntimeUpdateStatus, String> {
    let remote = fetch_remote_manifest()
        .await
        .ok_or_else(|| "Unable to reach the runtime update manifest".to_string())?;
    let manifest = prefer_newest_manifest(remote);
    let spec = host_spec();
    let local = load_manifest();
    let status = local_status(&local);
    let installed_version = effective_installed_version(&local);

    let chromium_update_available = status.installed
        && manifest
            .chromium_version
            .as_deref()
            .is_some_and(|candidate| version_is_newer(candidate, installed_version.as_deref()));
    let fingerprints_update_available = status.fingerprints_installed
        && manifest
            .archives
            .get(FINGERPRINTS_ARCHIVE_KEY)
            .is_some_and(|latest| local.fingerprints_etag.as_deref() != Some(latest.as_str()));
    let widevine_update_available = status.widevine_installed
        && manifest
            .archives
            .get(&spec.widevine.key)
            .is_some_and(|latest| local.widevine_etag.as_deref() != Some(latest.as_str()));

    Ok(RuntimeUpdateStatus {
        chromium_installed: status.installed,
        chromium_installed_version: installed_version,
        chromium_latest_version: manifest.chromium_version,
        chromium_update_available,
        fingerprints_installed: status.fingerprints_installed,
        fingerprints_update_available,
        widevine_installed: status.widevine_installed,
        widevine_update_available,
    })
}

#[tauri::command]
pub async fn runtime_install(window: Window, force: bool) -> Result<RuntimeStatus, String> {
    let _install_guard = runtime_install_lock().lock().await;
    let spec = host_spec();
    let base = runtime_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    recover_interrupted_runtime_swap(&base).map_err(|error| error.to_string())?;

    let local = load_manifest();
    let installed_now = local_status(&local).installed;
    let manifest = fetch_manifest().await;

    // Setup installs a missing engine; repair explicitly forces a coherent
    // reinstall. A newer remote version alone never triggers installation.
    let need_browser = force || !installed_now;

    // Repair Widevine when its required manifest is missing, regardless of the
    // persisted ETag. This is integrity repair, not an update check.
    let repair_widevine = !widevine_is_installed(&local);
    if (need_browser || repair_widevine)
        && !crate::process::Tracker::shared().active_profile_ids().is_empty()
    {
        return Err(
            "Stop all running or starting browsers before repairing the portable Runtime".into(),
        );
    }
    let (browser_etag, widevine_etag) = if need_browser {
        let (browser_etag, widevine_etag) =
            install_complete_runtime_transactionally(&window, &spec, &base)
                .await
                .map_err(|error| error.to_string())?;
        (browser_etag, Some(widevine_etag))
    } else {
        let widevine_etag = if repair_widevine {
            Some(
                install_widevine_transactionally(&window, &spec.widevine, &base)
                    .await
                    .map_err(|error| error.to_string())?,
            )
        } else {
            local.widevine_etag.clone()
        };
        (
            local.browser_etag.clone().unwrap_or_default(),
            widevine_etag,
        )
    };

    // Fingerprint seed: overwrites bundled templates, leaves user-added files;
    // skipped when the etag matches. User-added FP get version-migrated below.
    let fp_remote = manifest.archives.get(FINGERPRINTS_ARCHIVE_KEY).map(|s| s.as_str());
    let repair_fingerprints = !fingerprints_are_installed(&local);
    let fp_etag = install_fingerprints(
        &window,
        force || repair_fingerprints,
        local.fingerprints_etag.as_deref(),
        fp_remote,
    )
    .await
    .map_err(|e| e.to_string())?
    .or(local.fingerprints_etag);

    // Migrate already-created profiles AND the fingerprint library (incl.
    // user-added) to the new engine descriptor (UA + client_hints incl. grease).
    // Runs only when the version-or-grease signature changed since last time.
    let target_ver = manifest
        .chromium_version
        .clone()
        .unwrap_or_else(|| CHROMIUM_VERSION.to_string());
    let sig = format!(
        "{target_ver}|{}|{}",
        manifest.grease_brand.as_deref().unwrap_or(""),
        manifest.grease_version.as_deref().unwrap_or(""),
    );
    if local.applied_signature.as_deref() != Some(sig.as_str()) {
        let n = migrate_all_to(
            &target_ver,
            manifest.grease_brand.as_deref(),
            manifest.grease_version.as_deref(),
        );
        if n > 0 {
            eprintln!("[runtime] migrated {n} profile/fingerprint file(s) to {sig}");
        }
    }

    save_manifest(&Manifest {
        browser_etag: Some(browser_etag),
        widevine_etag,
        fingerprints_etag: fp_etag,
        applied_chromium_version: Some(target_ver.clone()),
        applied_signature: Some(sig),
        // Authoritative: we just successfully extracted exactly target_ver (the
        // old tree was wiped first). Recording the known value beats re-reading
        // it off disk, which is what let a leftover `<old>.manifest` keep the
        // version "stuck" and re-download every launch.
        installed_chromium_version: Some(target_ver),
    })
    .map_err(|e| e.to_string())?;

    let _ = window.emit("runtime:done", ());
    runtime_local_status()
}

async fn install_complete_runtime_transactionally(
    window: &Window,
    spec: &PlatformSpec,
    base: &Path,
) -> Result<(String, String)> {
    let stage = base.join(RUNTIME_STAGE_DIR);
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage)?;
    let prepared = async {
        let browser_etag = download_and_extract(window, &spec.browser, &stage).await?;
        let widevine_etag = download_and_extract(window, &spec.widevine, &stage).await?;
        place_widevine(&stage)?;
        validate_runtime_tree(&stage)?;
        Ok::<_, anyhow::Error>((browser_etag, widevine_etag))
    }
    .await;
    let (browser_etag, widevine_etag) = match prepared {
        Ok(value) => value,
        Err(error) => {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
    };

    replace_directory_transactionally(
        &stage.join(engine_root_dir()),
        &base.join(engine_root_dir()),
        &base.join(ENGINE_ROLLBACK_DIR),
    )?;
    let _ = fs::remove_dir_all(&stage);
    Ok((browser_etag, widevine_etag))
}

async fn install_widevine_transactionally(
    window: &Window,
    spec: &ArchiveSpec,
    base: &Path,
) -> Result<String> {
    let stage = base.join(RUNTIME_STAGE_DIR);
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage)?;
    let result = async {
        let etag = download_and_extract(window, spec, &stage).await?;
        let staged = stage.join("ShardX-Widevine-Win").join("WidevineCdm");
        validate_widevine_tree(&staged)?;
        replace_directory_transactionally(
            &staged,
            &base.join(engine_root_dir()).join("WidevineCdm"),
            &base.join(WIDEVINE_ROLLBACK_DIR),
        )?;
        Ok::<_, anyhow::Error>(etag)
    }
    .await;
    let _ = fs::remove_dir_all(&stage);
    result
}

fn replace_directory_transactionally(staged: &Path, live: &Path, rollback: &Path) -> Result<()> {
    if !staged.is_dir() {
        anyhow::bail!("staged Runtime directory is missing: {}", staged.display());
    }
    if rollback.exists() {
        fs::remove_dir_all(rollback).with_context(|| {
            format!("remove stale Runtime rollback directory {}", rollback.display())
        })?;
    }
    let had_live = live.exists();
    if had_live {
        fs::rename(live, rollback)
            .with_context(|| format!("move current Runtime {} to rollback", live.display()))?;
    }
    if let Err(error) = fs::rename(staged, live) {
        if had_live {
            if let Err(rollback_error) = fs::rename(rollback, live) {
                return Err(anyhow::anyhow!(
                    "failed to publish staged Runtime {}: {error}; restoring the previous Runtime also failed: {rollback_error}",
                    live.display()
                ));
            }
        }
        return Err(error)
            .with_context(|| format!("publish staged Runtime {}", live.display()));
    }
    if rollback.exists() {
        if let Err(error) = fs::remove_dir_all(rollback) {
            eprintln!(
                "[runtime] installed successfully but could not remove rollback {}: {error}",
                rollback.display()
            );
        }
    }
    Ok(())
}

fn recover_interrupted_runtime_swap(base: &Path) -> Result<()> {
    let live_engine = base.join(engine_root_dir());
    let engine_rollback = base.join(ENGINE_ROLLBACK_DIR);
    if engine_rollback.exists() {
        if live_engine.exists() {
            if let Err(error) = fs::remove_dir_all(&engine_rollback) {
                eprintln!(
                    "[runtime] current engine is valid but old rollback cleanup is still pending: {error}"
                );
            }
        } else {
            fs::rename(&engine_rollback, &live_engine)?;
        }
    }
    let live_widevine = live_engine.join("WidevineCdm");
    let widevine_rollback = base.join(WIDEVINE_ROLLBACK_DIR);
    if widevine_rollback.exists() {
        if live_widevine.exists() {
            if let Err(error) = fs::remove_dir_all(&widevine_rollback) {
                eprintln!(
                    "[runtime] current Widevine is valid but old rollback cleanup is still pending: {error}"
                );
            }
        } else {
            fs::rename(&widevine_rollback, &live_widevine)?;
        }
    }
    Ok(())
}

fn validate_runtime_tree(root: &Path) -> Result<()> {
    let engine = root.join(engine_root_dir());
    for required in ["chrome.exe", "chrome.dll", "resources.pak"] {
        let path = engine.join(required);
        let metadata = fs::metadata(&path)
            .with_context(|| format!("staged Runtime is missing {required}"))?;
        if !metadata.is_file() || metadata.len() == 0 {
            anyhow::bail!("staged Runtime file is empty or invalid: {}", path.display());
        }
    }
    validate_widevine_tree(&engine.join("WidevineCdm"))
}

fn validate_widevine_tree(root: &Path) -> Result<()> {
    let manifest = root.join("manifest.json");
    let metadata = fs::metadata(&manifest).context("staged Widevine manifest is missing")?;
    if !metadata.is_file() || metadata.len() == 0 {
        anyhow::bail!("staged Widevine manifest is empty");
    }
    Ok(())
}

/// Download + seed fingerprint library. Bundled templates are always
/// overwritten (so version bumps propagate); user-added files are left in place.
async fn install_fingerprints(
    window: &Window,
    force: bool,
    local_etag: Option<&str>,
    remote_etag: Option<&str>,
) -> Result<Option<String>> {
    if !force {
        if let (Some(local), Some(remote)) = (local_etag, remote_etag) {
            if local == remote {
                return Ok(None);
            }
        }
    }

    let dir = crate::store::fingerprints_dir()?;
    let spec = ArchiveSpec {
        key: FINGERPRINTS_ARCHIVE_KEY.into(),
        label: "Fingerprint library".into(),
    };
    // Stage outside fingerprints_dir to keep the zip wrapper dir out of the library.
    let staging = dir.join(".staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let etag = download_and_extract(window, &spec, &staging).await?;

    let src = staging.join(FINGERPRINTS_TOP_DIR);
    let walk = if src.exists() { src } else { staging.clone() };
    let mut added = 0;
    let mut overwritten = 0;
    for ent in fs::read_dir(&walk)? {
        let ent = ent?;
        let p = ent.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        // Always overwrite bundled templates so engine-version bumps reach
        // existing libraries. User-added fingerprints (names not in the bundle)
        // are never iterated here, so they stay untouched.
        let dst = dir.join(p.file_name().unwrap());
        let existed = dst.exists();
        fs::copy(&p, &dst)?;
        if existed { overwritten += 1; } else { added += 1; }
    }
    let _ = fs::remove_dir_all(&staging);
    eprintln!("[runtime] fingerprints sync: added={added} overwritten={overwritten}");
    Ok(Some(etag))
}

/// Stream archive → temp file → extract; emits `runtime:progress` events.
async fn download_and_extract(window: &Window, spec: &ArchiveSpec, base: &Path) -> Result<String> {
    let url = format!("{PUB_BASE}/{}", spec.key);
    let _ = window.emit(
        "runtime:progress",
        serde_json::json!({
            "label": spec.label,
            "phase": "download",
            "received": 0,
            "total": 0,
            "percent": 0,
        }),
    );
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?;
    let mut resp = client.get(&url).send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    if total > MAX_RUNTIME_ARCHIVE_BYTES {
        anyhow::bail!(
            "{} exceeds the {} GiB portable Runtime download limit",
            spec.key,
            MAX_RUNTIME_ARCHIVE_BYTES / 1024 / 1024 / 1024
        );
    }
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default();

    let tmp = base.join(format!("{}.tmp", spec.key));
    let operation = async {
        let mut out = tokio::fs::File::create(&tmp).await?;
        let mut received: u64 = 0;
        let mut last_pct: u64 = u64::MAX;
        while let Some(chunk) = resp.chunk().await? {
            out.write_all(&chunk).await?;
            received += chunk.len() as u64;
            if received > MAX_RUNTIME_ARCHIVE_BYTES {
                anyhow::bail!(
                    "{} exceeded the portable Runtime download limit",
                    spec.key
                );
            }
            // Emit once per integer percent.
            let pct = if total > 0 { received * 100 / total } else { 0 };
            if pct != last_pct {
                last_pct = pct;
                let _ = window.emit(
                    "runtime:progress",
                    serde_json::json!({
                        "label": spec.label,
                        "phase": "download",
                        "received": received,
                        "total": total,
                        "percent": pct,
                    }),
                );
            }
        }
        out.flush().await?;
        drop(out);

        let _ = window.emit(
            "runtime:progress",
            serde_json::json!({
                "label": spec.label,
                "phase": "extract",
                "received": total,
                "total": total,
                "percent": 100,
            }),
        );

        let zip_path = tmp.clone();
        let dest = base.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let f = fs::File::open(&zip_path)?;
            let mut archive = zip::ZipArchive::new(f)?;
            if archive.len() > MAX_RUNTIME_ARCHIVE_ENTRIES {
                anyhow::bail!("Runtime archive contains too many entries");
            }
            let mut extracted_bytes = 0u64;
            for index in 0..archive.len() {
                let entry = archive.by_index(index)?;
                if entry.enclosed_name().is_none() {
                    anyhow::bail!("Runtime archive contains an unsafe path: {}", entry.name());
                }
                if entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
                {
                    anyhow::bail!("Runtime archive contains an unsupported symbolic link");
                }
                extracted_bytes = extracted_bytes
                    .checked_add(entry.size())
                    .context("Runtime archive size overflow")?;
                if extracted_bytes > MAX_RUNTIME_EXTRACTED_BYTES {
                    anyhow::bail!("Runtime archive expands beyond the 32 GiB safety limit");
                }
            }
            let available = crate::store::available_space(&dest)?;
            if available < extracted_bytes.saturating_add(512 * 1024 * 1024) {
                anyhow::bail!(
                    "Not enough free disk space to extract the portable Runtime (need at least {} bytes plus reserve)",
                    extracted_bytes
                );
            }
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index)?;
                let relative = entry
                    .enclosed_name()
                    .context("Runtime archive contains an unsafe path")?;
                let output_path = dest.join(relative);
                if entry.is_dir() {
                    fs::create_dir_all(&output_path)?;
                    continue;
                }
                if let Some(parent) = output_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut output = fs::File::create(&output_path)?;
                std::io::copy(&mut entry, &mut output)?;
                output.sync_all()?;
            }
            Ok(())
        })
        .await??;

        Ok::<_, anyhow::Error>(etag)
    }
    .await;

    let _ = fs::remove_file(&tmp);

    operation
}

/// Windows flat layout: WidevineCdm/ sits beside chrome.exe.
fn place_widevine(base: &Path) -> Result<()> {
    let src = base.join("ShardX-Widevine-Win").join("WidevineCdm");
    if !src.exists() {
        return Ok(());
    }
    let dst = base.join("ShardX-Windows").join("WidevineCdm");
    if dst.exists() {
        let _ = fs::remove_dir_all(&dst);
    }
    fs::rename(&src, &dst)?;
    let _ = fs::remove_dir(base.join("ShardX-Widevine-Win"));
    Ok(())
}

// Launcher self-update checks are intentionally disabled.
