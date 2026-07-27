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
    /// Chromium version the *already-created* profiles were last migrated to.
    /// Lets us bump saved profiles' UA + client_hints when the engine updates,
    /// independent of the fingerprint-library seed.
    #[serde(default)]
    applied_chromium_version: Option<String>,
    /// Signature (`<version>|<grease_brand>|<grease_version>`) of the engine
    /// descriptor the profiles/fingerprints were last migrated against. Migration
    /// re-runs whenever this changes — so adding grease (or any future field) to
    /// the manifest auto-triggers a re-migration even for users already on the
    /// current `applied_chromium_version`. No bump-the-constant ceremony.
    #[serde(default)]
    applied_signature: Option<String>,
    /// Chromium version of the engine binary currently extracted on disk.
    /// The engine update is detected by comparing THIS to the manifest's
    /// `chromium_version` — robust where the etag check failed (e.g. a user who
    /// updated the app but whose stored etag already matched).
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
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_manifest(m: &Manifest) -> Result<()> {
    let p = manifest_path()?;
    fs::create_dir_all(p.parent().unwrap())?;
    fs::write(p, serde_json::to_string_pretty(m)?)?;
    Ok(())
}
#[derive(Serialize, Clone, Debug)]
pub struct RuntimeStatus {
    pub installed: bool,
    pub binary_path: Option<PathBuf>,
    pub installed_browser_etag: Option<String>,
    pub remote_browser_etag: Option<String>,
    pub update_available: bool,
    pub spec: PlatformSpec,
    /// True once the fingerprint library bundle has been extracted.
    pub fingerprints_installed: bool,
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

fn status_from_manifest(
    installed: bool,
    local: Manifest,
    manifest: RemoteManifest,
    update_available: bool,
) -> RuntimeStatus {
    let spec = host_spec();
    let remote = manifest.archives.get(&spec.browser.key).cloned();
    // Stamp present AND dir has at least one .json (catches user-nuked dir).
    let fingerprints_installed = fingerprints_are_installed(&local);

    RuntimeStatus {
        installed,
        binary_path: if installed { binary_path().ok() } else { None },
        installed_browser_etag: local.browser_etag,
        remote_browser_etag: remote,
        update_available,
        spec,
        fingerprints_installed,
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

/// Fetch the version manifest (GitHub raw) — one request yielding every
/// archive's current etag + the chromium version, so install/status never poll
/// R2/S3 per-archive. Empty/None when unreachable.
async fn fetch_manifest() -> RemoteManifest {
    async fn inner() -> Option<RemoteManifest> {
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
    let bundled = bundled_manifest();
    match inner().await {
        Some(manifest) => {
            let bundled_is_newer = bundled.chromium_version.as_deref().is_some_and(|candidate| {
                version_is_newer(candidate, manifest.chromium_version.as_deref())
            });
            if bundled_is_newer {
                eprintln!("[runtime] remote manifest is older than bundled metadata; using bundled");
                bundled
            } else {
                manifest
            }
        }
        None => {
            eprintln!("[runtime] remote manifest unavailable; using bundled metadata");
            bundled
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
            fs::write(&p, serde_json::to_string_pretty(&cfg)?)?;
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

/// Startup hook: migrate saved profiles + the fingerprint library (bundled +
/// user-added) to the manifest's engine descriptor when not already done. One
/// GitHub-manifest GET (never S3). Guarded by a signature of
/// `<version>|<grease_brand>|<grease_version>`, so a change to the grease (or any
/// future manifest field) re-triggers migration even for users already on the
/// current version — no version bump or constant needed. Also covers users
/// whose engine auto-updated via the etag path without an explicit install.
pub async fn ensure_profiles_migrated() {
    let m = fetch_manifest().await;
    let Some(target) = m.chromium_version.clone() else { return };
    let sig = format!(
        "{target}|{}|{}",
        m.grease_brand.as_deref().unwrap_or(""),
        m.grease_version.as_deref().unwrap_or(""),
    );
    let mut local = load_manifest();
    if local.applied_signature.as_deref() == Some(sig.as_str()) {
        return;
    }
    let n = migrate_all_to(&target, m.grease_brand.as_deref(), m.grease_version.as_deref());
    if n > 0 {
        eprintln!("[runtime] migrated {n} profile/fingerprint file(s) to {sig}");
    }
    local.applied_chromium_version = Some(target);
    local.applied_signature = Some(sig);
    let _ = save_manifest(&local);
}

#[tauri::command]
pub async fn runtime_status() -> Result<RuntimeStatus, String> {
    let installed = binary_path().map(|p| p.exists()).unwrap_or(false);
    let local = load_manifest();
    // A clean install does not need a network round-trip merely to discover
    // that the browser is absent. The install command fetches fresh metadata
    // next, with a bounded bundled fallback.
    let manifest = if installed {
        fetch_manifest().await
    } else {
        bundled_manifest()
    };
    // Update is detected by VERSION (a newer manifest version vs the engine on
    // disk), not by etag — robust for users whose stored etag already matched
    // but whose binary never actually updated. Manifest unreachable
    // (chromium_version None) → assume up to date.
    let update_available = installed
        && manifest
            .chromium_version
            .as_deref()
            .is_some_and(|candidate| {
                version_is_newer(candidate, effective_installed_version(&local).as_deref())
            });

    Ok(status_from_manifest(
        installed,
        local,
        manifest,
        update_available,
    ))
}

/// Fast startup check that only reads local files and bundled release metadata.
/// Remote update discovery is intentionally kept out of the launch-critical path.
#[tauri::command]
pub fn runtime_local_status() -> Result<RuntimeStatus, String> {
    let installed = binary_path().map(|p| p.exists()).unwrap_or(false);
    let local = load_manifest();
    let manifest = bundled_manifest();
    let update_available = installed
        && manifest.chromium_version.as_deref().is_some_and(|candidate| {
            version_is_newer(candidate, effective_installed_version(&local).as_deref())
        });

    Ok(status_from_manifest(
        installed,
        local,
        manifest,
        update_available,
    ))
}

#[tauri::command]
pub async fn runtime_install(window: Window, force: bool) -> Result<RuntimeStatus, String> {
    let spec = host_spec();
    let base = runtime_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;

    let installed_now = binary_path().map(|p| p.exists()).unwrap_or(false);
    let local = load_manifest();
    let manifest = fetch_manifest().await;

    // Re-download the engine when the manifest has a newer chromium_version
    // (or when missing / forced). VERSION-based, not etag — so the update
    // actually fires for already-installed users on a bump. Manifest
    // unreachable (None) → don't force a re-download.
    let need_browser = if force || !installed_now {
        true
    } else {
        match &manifest.chromium_version {
            Some(candidate) => version_is_newer(
                candidate,
                effective_installed_version(&local).as_deref(),
            ),
            None => false,
        }
    };
    let browser_etag = if need_browser {
        // Wipe the old engine tree first. The archive extracts *over* the
        // existing dir but never deletes files the new version dropped — most
        // critically the previous `<version>.manifest`, which lingers beside the
        // new one and poisons version detection into an endless re-download;
        // stale DLLs could also be loaded.
        let _ = fs::remove_dir_all(base.join(engine_root_dir()));
        download_and_extract(&window, &spec.browser, &base)
            .await
            .map_err(|e| e.to_string())?
    } else {
        local.browser_etag.clone().unwrap_or_default()
    };

    // Re-download Widevine only when the browser changed or the manifest lacks
    // a stamp. Windows x64 always ships the CDM archive.
    let widevine_etag = if need_browser || local.widevine_etag.is_none() {
        let etag = download_and_extract(&window, &spec.widevine, &base)
            .await
            .map_err(|e| e.to_string())?;
        place_widevine(&base).map_err(|e| e.to_string())?;
        Some(etag)
    } else {
        local.widevine_etag.clone()
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
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default();

    let tmp = base.join(format!("{}.tmp", spec.key));
    {
        let mut out = tokio::fs::File::create(&tmp).await?;
        let mut received: u64 = 0;
        let mut last_pct: u64 = u64::MAX;
        while let Some(chunk) = resp.chunk().await? {
            out.write_all(&chunk).await?;
            received += chunk.len() as u64;
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
    }

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
        archive.extract(&dest)?;
        Ok(())
    })
    .await??;

    let _ = fs::remove_file(&tmp);

    Ok(etag)
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
