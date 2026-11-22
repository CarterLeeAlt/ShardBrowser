//! Install and repair the runtime snapshot paired with this launcher.
//! Emits `runtime:progress` and `runtime:done` events to the Tauri frontend.

use anyhow::{Context, Result};
#[cfg(test)]
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{Emitter, Window};
use tokio::io::AsyncWriteExt;

#[cfg(test)]
const MANIFEST_REPO: &str = "ProxyShard/ShardBrowser";
#[cfg(test)]
const MANIFEST_BRANCH: &str = "main";
#[cfg(test)]
const MANIFEST_FILE: &str = "runtime.json";

/// Manifest mirrors in priority order. raw.githubusercontent.com is
/// DNS-unreachable on some networks; the GitHub contents API and the jsDelivr
/// GitHub mirror serve the identical file and stay reachable there (jsDelivr
/// caches branch refs for a few hours, so it is the last resort).
#[cfg(test)]
fn manifest_urls() -> [String; 3] {
    [
        format!("https://raw.githubusercontent.com/{MANIFEST_REPO}/{MANIFEST_BRANCH}/{MANIFEST_FILE}"),
        format!("https://api.github.com/repos/{MANIFEST_REPO}/contents/{MANIFEST_FILE}?ref={MANIFEST_BRANCH}"),
        format!("https://cdn.jsdelivr.net/gh/{MANIFEST_REPO}@{MANIFEST_BRANCH}/{MANIFEST_FILE}"),
    ]
}
const BUNDLED_MANIFEST_JSON: &str = include_str!("../../runtime.json");
const MAX_RUNTIME_ARCHIVE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_RUNTIME_EXTRACTED_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_RUNTIME_ARCHIVE_ENTRIES: usize = 250_000;
const RUNTIME_STAGE_DIR: &str = ".runtime-stage";
const ENGINE_ROLLBACK_DIR: &str = ".ShardX-Windows.rollback";
const WIDEVINE_ROLLBACK_DIR: &str = ".WidevineCdm.rollback";
static RUNTIME_STATE_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
static RUNTIME_UPDATE_CHECK_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
    std::sync::OnceLock::new();

fn runtime_state_lock() -> &'static tokio::sync::Mutex<()> {
    RUNTIME_STATE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn runtime_update_check_lock() -> &'static tokio::sync::Mutex<()> {
    RUNTIME_UPDATE_CHECK_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn default_check_available() -> bool {
    true
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
    Ok(runtime_dir()?.join(engine_root_dir()).join("chrome.exe"))
}

pub(crate) fn widevine_dir() -> Result<PathBuf> {
    Ok(runtime_dir()?.join(engine_root_dir()).join("WidevineCdm"))
}

fn manifest_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("manifest.json"))
}

fn update_check_snapshot_path() -> Result<PathBuf> {
    Ok(crate::store::config_root()?.join("runtime-update-check.json"))
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
    /// Written only after a first complete install or a successful Chromium
    /// component update; Widevine/fingerprint work and repair preserve it.
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
    #[serde(default)]
    installed_metadata: Option<RemoteManifest>,
}

/// Chromium version of the engine actually on disk, read from the Windows
/// `<version>.manifest` sidecar beside chrome.exe.
fn installed_engine_version() -> Option<String> {
    engine_version_in(&runtime_dir().ok()?.join(engine_root_dir())).ok()
}

pub(crate) fn is_engine_installed() -> bool {
    installed_engine_version().is_some()
}

/// Version reported to extension update checks. Falls back to the pinned
/// bundle so a portable first launch can fetch extensions before the engine
/// is on disk.
pub(crate) fn engine_version_for_downloads() -> String {
    installed_engine_version()
        .or_else(|| bundled_manifest().chromium_version)
        .unwrap_or_else(|| "158.0.0.0".into())
}

fn engine_version_in(dir: &Path) -> Result<String> {
    let mut versions = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) == Some("manifest") {
            if let Some(version) = path.file_stem().and_then(|value| value.to_str()) {
                if version.split('.').count() == 4
                    && version.split('.').all(|part| part.parse::<u32>().is_ok())
                {
                    versions.push(version.to_owned());
                }
            }
        }
    }
    anyhow::ensure!(
        versions.len() == 1,
        "Expected exactly one Chromium version manifest in {}",
        dir.display()
    );
    let sidecar = versions.remove(0);
    let executable = executable_product_version(&dir.join("chrome.exe"))?;
    anyhow::ensure!(
        sidecar == executable,
        "Runtime executable version {executable} does not match its {sidecar} sidecar"
    );
    Ok(executable)
}

fn executable_product_version(exe: &Path) -> Result<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let resource_size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut()) };
    anyhow::ensure!(
        resource_size > 0,
        "Cannot read runtime version resource: {}",
        exe.display()
    );
    let mut resource = vec![0u8; resource_size as usize];
    let mut value = std::ptr::null_mut();
    let mut length = 0;
    let root = [b'\\' as u16, 0];
    unsafe {
        anyhow::ensure!(
            GetFileVersionInfoW(
                path.as_ptr(),
                0,
                resource_size,
                resource.as_mut_ptr().cast()
            ) != 0,
            "Cannot load runtime version resource"
        );
        anyhow::ensure!(
            VerQueryValueW(
                resource.as_ptr().cast(),
                root.as_ptr(),
                &mut value,
                &mut length
            ) != 0
                && !value.is_null()
                && length as usize >= std::mem::size_of::<VS_FIXEDFILEINFO>(),
            "Invalid runtime version resource"
        );
        let version = value.cast::<VS_FIXEDFILEINFO>().read_unaligned();
        anyhow::ensure!(
            version.dwSignature == 0xFEEF04BD,
            "Invalid runtime version signature"
        );
        Ok(format!(
            "{}.{}.{}.{}",
            version.dwProductVersionMS >> 16,
            version.dwProductVersionMS & 0xffff,
            version.dwProductVersionLS >> 16,
            version.dwProductVersionLS & 0xffff
        ))
    }
}

/// Prefer the executable version. The recorded version also identifies the
/// snapshot when its engine is missing and needs repair.
fn effective_installed_version(local: &Manifest) -> Option<String> {
    installed_engine_version().or_else(|| local.installed_chromium_version.clone())
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChromiumState {
    browser_etag: Option<String>,
    applied_chromium_version: Option<String>,
    applied_signature: Option<String>,
    installed_chromium_version: Option<String>,
    installed_metadata: Option<RemoteManifest>,
}

#[cfg(test)]
fn chromium_state(local: &Manifest) -> ChromiumState {
    ChromiumState {
        browser_etag: local.browser_etag.clone(),
        applied_chromium_version: local.applied_chromium_version.clone(),
        applied_signature: local.applied_signature.clone(),
        installed_chromium_version: local.installed_chromium_version.clone(),
        installed_metadata: local.installed_metadata.clone(),
    }
}

#[cfg(test)]
fn updated_chromium_state(
    migrated_version: String,
    migrated_signature: String,
    installed_version: Option<String>,
    browser_etag: String,
    installed_metadata: Option<RemoteManifest>,
) -> ChromiumState {
    ChromiumState {
        browser_etag: Some(browser_etag),
        applied_chromium_version: Some(migrated_version),
        applied_signature: Some(migrated_signature),
        installed_chromium_version: installed_version,
        installed_metadata,
    }
}

#[cfg(test)]
fn manifest_with_chromium_state(
    chromium: ChromiumState,
    widevine_etag: Option<String>,
    fingerprints_etag: Option<String>,
) -> Manifest {
    Manifest {
        browser_etag: chromium.browser_etag,
        widevine_etag,
        fingerprints_etag,
        applied_chromium_version: chromium.applied_chromium_version,
        applied_signature: chromium.applied_signature,
        installed_chromium_version: chromium.installed_chromium_version,
        installed_metadata: chromium.installed_metadata,
    }
}

fn is_chromium_replacement_needed(force: bool, browser_was_missing: bool) -> bool {
    force || browser_was_missing
}

pub fn ensure_migration_complete() -> Result<()> {
    let local = load_manifest();
    anyhow::ensure!(
        local.installed_metadata.is_none() || local.applied_signature.is_some(),
        crate::errors::conflict(
            "Runtime profile migration is incomplete. Use Repair before launching browsers."
        )
    );
    Ok(())
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
    let Ok(p) = manifest_path() else {
        return Manifest::default();
    };
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

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct RuntimeUpdateCheckSnapshot {
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub result: Option<RuntimeUpdateStatus>,
    pub error: Option<String>,
}

fn update_check_timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("@{seconds}")
}

fn load_update_check_snapshot() -> RuntimeUpdateCheckSnapshot {
    let Ok(path) = update_check_snapshot_path() else {
        return RuntimeUpdateCheckSnapshot::default();
    };
    if !path.exists() {
        return RuntimeUpdateCheckSnapshot::default();
    }
    match crate::store::load_json_with_backup(&path) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("[runtime] update check snapshot recovery failed: {error}");
            RuntimeUpdateCheckSnapshot::default()
        }
    }
}

fn save_update_check_snapshot(snapshot: &RuntimeUpdateCheckSnapshot) -> Result<()> {
    let path = update_check_snapshot_path()?;
    crate::store::atomic_write(&path, serde_json::to_string_pretty(snapshot)?.as_bytes())
}

#[derive(Serialize, Clone, Debug)]
pub struct RuntimeStatus {
    pub installed: bool,
    pub binary_path: Option<PathBuf>,
    pub initialized: bool,
    pub spec: PlatformSpec,
    pub fingerprints_installed: bool,
    pub widevine_installed: bool,
    pub migration_pending: bool,
    pub repair_requires_upgrade: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RuntimeUpdateStatus {
    pub chromium_installed: bool,
    pub chromium_installed_version: Option<String>,
    pub chromium_latest_version: Option<String>,
    pub chromium_update_available: bool,
    pub chromium_download_url: Option<String>,
    pub fingerprints_installed: bool,
    #[serde(default = "default_check_available")]
    pub fingerprints_check_available: bool,
    pub fingerprints_update_available: bool,
    pub fingerprints_download_url: Option<String>,
    pub widevine_installed: bool,
    #[serde(default = "default_check_available")]
    pub widevine_check_available: bool,
    pub widevine_update_available: bool,
    pub widevine_download_url: Option<String>,
}

fn fingerprints_are_installed(local: &Manifest) -> bool {
    local.fingerprints_etag.is_some()
        && crate::store::fingerprints_dir()
            .map(|d| {
                fs::read_dir(&d)
                    .map(|it| {
                        it.flatten()
                            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false)
}

fn widevine_is_installed(local: &Manifest) -> bool {
    local.widevine_etag.is_some()
        && widevine_dir()
            .map(|directory| validate_widevine_tree(&directory).is_ok())
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
        migration_pending: local.installed_metadata.is_some() && local.applied_signature.is_none(),
        repair_requires_upgrade: repair_manifest(local).is_err()
            && !effective_installed_version(local).is_some_and(|installed| {
                version_is_newer(&installed, bundled_manifest().chromium_version.as_deref())
            }),
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct RemoteManifest {
    archives: std::collections::HashMap<String, String>,
    chromium_version: Option<String>,
    /// GREASE brand/version the engine emits in `sec-ch-ua`. Not derivable from
    /// the version number (it rotates per major release), so it travels in the
    /// manifest as data — migration writes it into every profile/fingerprint.
    grease_brand: Option<String>,
    grease_version: Option<String>,
    android_chromium_version: Option<String>,
    tls: Option<serde_json::Map<String, serde_json::Value>>,
    engine_build: Option<String>,
    windows_snapshot: Option<WindowsSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WindowsSnapshot {
    id: String,
    asset_base: String,
    sizes: std::collections::HashMap<String, u64>,
}

fn bundled_manifest() -> RemoteManifest {
    serde_json::from_str(BUNDLED_MANIFEST_JSON).unwrap_or_default()
}

/// Upstream metadata probe for diagnostics; launcher installation only uses pinned assets.
#[cfg(test)]
async fn fetch_remote_manifest() -> Option<RemoteManifest> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(4))
        .timeout(std::time::Duration::from_secs(8))
        // GitHub's API rejects header-less clients; the other sources ignore it.
        .user_agent("ShardBrowser-Launcher")
        .build()
        .ok()?;
    fetch_manifest_from(&client, manifest_urls()).await
}

#[cfg(test)]
async fn fetch_manifest_from(
    client: &reqwest::Client,
    urls: impl IntoIterator<Item = String>,
) -> Option<RemoteManifest> {
    for url in urls {
        let Some(text) = fetch_manifest_text(client, &url).await else {
            continue;
        };
        if let Some(manifest) = parse_remote_manifest(&text) {
            return Some(manifest);
        }
    }
    None
}

/// GET a manifest source and unwrap the response to the raw runtime.json
/// text. The GitHub contents API wraps the file in a JSON envelope with the
/// body base64-encoded; raw and jsDelivr return the manifest JSON directly.
#[cfg(test)]
async fn fetch_manifest_text(client: &reqwest::Client, url: &str) -> Option<String> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    match body.get("encoding").and_then(|e| e.as_str()) {
        Some("base64") => decode_base64_envelope(&body),
        _ => Some(body.to_string()),
    }
}

/// Decode the GitHub contents API envelope's base64 `content` field (which
/// wraps lines in newlines) into file text.
#[cfg(test)]
fn decode_base64_envelope(body: &serde_json::Value) -> Option<String> {
    let content = body.get("content")?.as_str()?;
    let cleaned: Vec<u8> = content
        .as_bytes()
        .iter()
        .copied()
        .filter(|&b| b != b'\n')
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// Parse runtime.json text into a `RemoteManifest`. Requires
/// `chromium_version` so an unrelated JSON body (API error, HTML error page)
/// is rejected and the next source gets tried instead.
#[cfg(test)]
fn parse_remote_manifest(text: &str) -> Option<RemoteManifest> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let str_field = |k: &str| v.get(k).and_then(|s| s.as_str()).map(String::from);
    let chromium_version = str_field("chromium_version")?;
    let archives = v
        .get("archives")
        .and_then(|a| a.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(RemoteManifest {
        archives,
        chromium_version: Some(chromium_version),
        grease_brand: str_field("grease_brand"),
        grease_version: str_field("grease_version"),
        android_chromium_version: str_field("android_chromium_version"),
        tls: v.get("tls").and_then(|v| v.as_object()).cloned(),
        engine_build: str_field("engine_build"),
        windows_snapshot: None,
    })
}

/// Repair keeps the installed snapshot. An installation without a matching
/// snapshot must use the explicit Chromium update instead.
fn repair_manifest(local: &Manifest) -> Result<RemoteManifest, String> {
    let bundled = bundled_manifest();
    if let Some(metadata) = &local.installed_metadata {
        if metadata.windows_snapshot.is_some() {
            return Ok(metadata.clone());
        }
    }
    if let Some(etag) = local.browser_etag.as_deref() {
        if bundled
            .archives
            .get(&host_spec().browser.key)
            .map(String::as_str)
            != Some(etag)
        {
            return Err("The installed runtime has no pinned repair snapshot. Install the runtime paired with this launcher explicitly instead.".into());
        }
    }
    Ok(bundled)
}

/// New identities use metadata for the installed binary, never a newer remote
/// offer. Bundled metadata can fill the same-version snapshot on offline installs.
pub(crate) fn prepare_new_identity(config: &mut serde_json::Map<String, serde_json::Value>) {
    let local = load_manifest();
    prepare_identity_for_engine(config, &local);
}

fn prepare_identity_for_engine(
    config: &mut serde_json::Map<String, serde_json::Value>,
    local: &Manifest,
) {
    let bundled = bundled_manifest();
    let Some(version) = effective_installed_version(local) else {
        return;
    };
    let metadata = local
        .installed_metadata
        .as_ref()
        .filter(|m| m.chromium_version.as_deref() == Some(version.as_str()))
        .or_else(|| {
            (bundled.chromium_version.as_deref() == Some(version.as_str())).then_some(&bundled)
        });
    let mobile = config
        .get("navigator")
        .and_then(|n| n.get("platform"))
        .and_then(|v| v.as_str())
        == Some("Android");
    let chrome = metadata
        .and_then(|m| {
            mobile
                .then_some(m.android_chromium_version.as_deref())
                .flatten()
        })
        .unwrap_or(&version);
    migrate_descriptor_to(
        config,
        chrome,
        metadata.and_then(|m| m.grease_brand.as_deref()),
        metadata.and_then(|m| m.grease_version.as_deref()),
    );
    if let (Some(tls), Some(target)) = (
        metadata.and_then(|m| m.tls.as_ref()),
        config.get_mut("tls").and_then(|v| v.as_object_mut()),
    ) {
        for (key, value) in tls {
            target.insert(key.clone(), value.clone());
        }
    }
    if mobile {
        if let Some(hints) = config
            .get_mut("client_hints")
            .and_then(|v| v.as_object_mut())
        {
            if hints
                .get("brand")
                .and_then(|v| v.as_str())
                .is_none_or(str::is_empty)
            {
                hints.insert("brand".into(), "Google Chrome".into());
            }
        }
    }
    if let Some(major) = chrome.split('.').next().and_then(|v| v.parse().ok()) {
        crate::fingerprint_identity::add_apple_features(config, major);
    }
}

/// Migrate a fingerprint descriptor to a new engine version. Leaves
/// platform_version, architecture, webgl, and every unrelated field intact.
/// Returns whether any Chrome descriptor field changed.
fn migrate_descriptor_to(
    config: &mut serde_json::Map<String, serde_json::Value>,
    chromium_version: &str,
    grease_brand: Option<&str>,
    grease_version: Option<&str>,
) -> bool {
    let parts: Vec<&str> = chromium_version.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let major = parts[0];
    let build: i64 = parts[2].parse().unwrap_or(0);
    let patch: i64 = parts[3].parse().unwrap_or(0);
    let mut changed = false;

    // navigator.user_agent: replace the Chrome/<ver> token with major.0.0.0.
    if let Some(ua) = config
        .get_mut("navigator")
        .and_then(|value| value.as_object_mut())
        .and_then(|navigator| navigator.get("user_agent"))
        .and_then(|value| value.as_str())
        .map(String::from)
    {
        if let Some(idx) = ua.find("Chrome/") {
            let rest = &ua[idx + 7..];
            let end = rest.find(' ').unwrap_or(rest.len());
            let new_ua = format!("{}Chrome/{}.0.0.0{}", &ua[..idx], major, &rest[end..]);
            if new_ua != ua {
                if let Some(slot) = config
                    .get_mut("navigator")
                    .and_then(|value| value.as_object_mut())
                    .and_then(|navigator| navigator.get_mut("user_agent"))
                {
                    *slot = serde_json::Value::String(new_ua);
                    changed = true;
                }
            }
        }
    }

    if let Some(ch) = config
        .get_mut("client_hints")
        .and_then(|value| value.as_object_mut())
    {
        let mut wants: Vec<(&str, serde_json::Value)> = vec![
            ("brand_version", serde_json::json!(major)),
            ("brand_full_version", serde_json::json!(chromium_version)),
            ("chrome_build", serde_json::json!(build)),
            ("chrome_patch", serde_json::json!(patch)),
        ];
        // GREASE only changes when the manifest carries a new value.
        if let Some(gb) = grease_brand {
            wants.push(("grease_brand", serde_json::json!(gb)));
        }
        if let Some(gv) = grease_version {
            wants.push(("grease_version", serde_json::json!(gv)));
            wants.push((
                "grease_full_version",
                serde_json::json!(format!("{gv}.0.0.0")),
            ));
        }
        for (key, want) in wants {
            if ch.get(key) != Some(&want) {
                ch.insert(key.to_string(), want);
                changed = true;
            }
        }
    }

    changed
}

/// Migrate every user-added fingerprint JSON file to a new engine descriptor.
/// Fingerprint entries contain no launcher metadata, so the ordinary atomic
/// writer remains appropriate here.
fn migrate_fingerprint_dir_to(
    dir: &Path,
    chromium_version: &str,
    grease_brand: Option<&str>,
    grease_version: Option<&str>,
) -> Result<usize> {
    let mut migrated = 0usize;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read fingerprint {}", path.display()))?;
        let mut config: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text)
            .with_context(|| format!("parse fingerprint {}", path.display()))?;
        let changed = if config.contains_key("payload") {
            let payload = config
                .get_mut("payload")
                .and_then(serde_json::Value::as_object_mut)
                .with_context(|| format!("invalid fingerprint payload: {}", path.display()))?;
            let changed =
                migrate_descriptor_to(payload, chromium_version, grease_brand, grease_version);
            let chrome = payload
                .get("client_hints")
                .and_then(|hints| hints.get("brand_version"))
                .cloned();
            let label_changed = chrome
                .as_ref()
                .is_some_and(|chrome| config.get("chrome") != Some(chrome));
            if let Some(chrome) = chrome {
                config.insert("chrome".into(), chrome);
            }
            changed || label_changed
        } else {
            migrate_descriptor_to(&mut config, chromium_version, grease_brand, grease_version)
        };
        if changed {
            crate::store::atomic_write(&path, serde_json::to_string_pretty(&config)?.as_bytes())?;
            migrated += 1;
        }
    }
    Ok(migrated)
}

/// Migrate saved profiles through their protected serialization path. This
/// upgrades legacy inline proxy credentials and prevents either the primary
/// profile file or its recovery copy from retaining plaintext credentials.
fn migrate_profile_dir_to(
    dir: &Path,
    chromium_version: &str,
    grease_brand: Option<&str>,
    grease_version: Option<&str>,
) -> Result<usize> {
    let mut migrated = 0usize;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let mut profile: crate::profile::StoredProfile =
            crate::store::load_json_with_backup(&path)?;
        let normalized = crate::profile::normalize_browser_settings(&mut profile)?;
        if migrate_descriptor_to(
            &mut profile.config,
            chromium_version,
            grease_brand,
            grease_version,
        ) || normalized
        {
            crate::store::atomic_write_sensitive(&path, &serde_json::to_vec_pretty(&profile)?)?;
            migrated += 1;
        }
    }
    Ok(migrated)
}

fn copy_runtime_descriptors(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if matches!(
            entry.path().extension().and_then(|ext| ext.to_str()),
            Some("json" | "bak")
        ) {
            anyhow::ensure!(
                entry.file_type()?.is_file(),
                "Invalid runtime descriptor {}",
                entry.path().display()
            );
            fs::copy(entry.path(), destination.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn collect_descriptor_replacements(
    staged: &Path,
    live: &Path,
    replacements: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    for entry in fs::read_dir(staged)? {
        let entry = entry?;
        let destination = live.join(entry.file_name());
        if destination.try_exists()? {
            anyhow::ensure!(
                fs::symlink_metadata(&destination)?.file_type().is_file(),
                "Invalid runtime descriptor {}",
                destination.display()
            );
            if fs::read(entry.path())? == fs::read(&destination)? {
                continue;
            }
        }
        replacements.push((entry.path(), destination));
    }
    Ok(())
}

/// Fast startup integrity check. It only reads local files and never contacts
/// GitHub or compares versions.
#[tauri::command]
pub fn runtime_local_status() -> Result<RuntimeStatus, String> {
    let base = runtime_dir().map_err(|error| error.to_string())?;
    fs::create_dir_all(&base).map_err(|error| error.to_string())?;
    if let Ok(_guard) = runtime_state_lock().try_lock() {
        if base.join(ENGINE_ROLLBACK_DIR).exists() || base.join(WIDEVINE_ROLLBACK_DIR).exists() {
            let _reservation = crate::operations::reserve_runtime().map_err(|e| e.to_string())?;
            recover_interrupted_runtime_swap(&base).map_err(|error| error.to_string())?;
        }
    }
    let local = load_manifest();
    Ok(local_status(&local))
}

struct RemoteUpdateMetadata {
    manifest: RemoteManifest,
    fingerprints_latest: Option<String>,
    widevine_latest: Option<String>,
}

/// Fetch remote version metadata without reading or modifying local runtime
/// state, so an install is never blocked on network timeouts.
fn pinned_update_metadata() -> Result<RemoteUpdateMetadata, String> {
    let manifest = bundled_manifest();
    let spec = host_spec();
    if manifest.windows_snapshot.is_none() {
        return Err("Missing pinned Windows runtime".into());
    }
    let fingerprints_latest = manifest.archives.get(FINGERPRINTS_ARCHIVE_KEY).cloned();
    let widevine_latest = manifest.archives.get(&spec.widevine.key).cloned();

    Ok(RemoteUpdateMetadata {
        manifest,
        fingerprints_latest,
        widevine_latest,
    })
}

/// Combine remote version metadata with a stable snapshot of the installed
/// runtime. The caller holds `runtime_state_lock` while this reads local files.
fn runtime_update_status_from_metadata(remote: RemoteUpdateMetadata) -> RuntimeUpdateStatus {
    let spec = host_spec();
    let local = load_manifest();
    let status = local_status(&local);
    let installed_version = effective_installed_version(&local);

    let pinned_version = remote.manifest.chromium_version.as_deref();
    let version_matches = installed_version.as_deref() == pinned_version;
    let replace_build = version_matches
        && local.browser_etag.as_ref() != remote.manifest.archives.get(&spec.browser.key);
    let chromium_update_available = status.installed
        && (replace_build
            || pinned_version.is_some_and(|candidate| {
                version_is_newer(candidate, installed_version.as_deref())
            }));
    let fingerprints_check_available = version_matches && remote.fingerprints_latest.is_some();
    let widevine_check_available = version_matches && remote.widevine_latest.is_some();
    let fingerprints_update_available = fingerprints_check_available
        && status.fingerprints_installed
        && remote
            .fingerprints_latest
            .is_some_and(|latest| local.fingerprints_etag.as_deref() != Some(latest.as_str()));
    let widevine_update_available = widevine_check_available
        && status.widevine_installed
        && remote
            .widevine_latest
            .is_some_and(|latest| local.widevine_etag.as_deref() != Some(latest.as_str()));

    let url = |key: &str| {
        remote
            .manifest
            .windows_snapshot
            .as_ref()
            .map(|snapshot| format!("{}/{}-{key}", snapshot.asset_base, snapshot.id))
    };
    let chromium_download_url = url(&spec.browser.key);
    let fingerprints_download_url = url(FINGERPRINTS_ARCHIVE_KEY);
    let widevine_download_url = url(&spec.widevine.key);
    RuntimeUpdateStatus {
        chromium_installed: status.installed,
        chromium_installed_version: installed_version,
        chromium_latest_version: remote.manifest.chromium_version,
        chromium_update_available,
        chromium_download_url,
        fingerprints_installed: status.fingerprints_installed,
        fingerprints_check_available,
        fingerprints_update_available,
        fingerprints_download_url,
        widevine_installed: status.widevine_installed,
        widevine_check_available,
        widevine_update_available,
        widevine_download_url,
    }
}

fn record_update_check_success(
    mut snapshot: RuntimeUpdateCheckSnapshot,
    timestamp: String,
    result: RuntimeUpdateStatus,
) -> RuntimeUpdateCheckSnapshot {
    snapshot.last_attempt_at = Some(timestamp.clone());
    snapshot.last_success_at = Some(timestamp);
    snapshot.result = Some(result);
    snapshot.error = None;
    snapshot
}

fn record_update_check_failure(
    mut snapshot: RuntimeUpdateCheckSnapshot,
    timestamp: String,
    error: String,
) -> RuntimeUpdateCheckSnapshot {
    snapshot.last_attempt_at = Some(timestamp);
    snapshot.error = Some(error);
    snapshot
}

fn emit_update_check_complete(snapshot: &RuntimeUpdateCheckSnapshot) {
    if let Some(window) = crate::main_window() {
        let _ = window.emit("runtime:update-check-complete", snapshot);
    }
}

async fn check_updates_and_persist() -> Result<RuntimeUpdateCheckSnapshot, String> {
    let _check_guard = runtime_update_check_lock().lock().await;
    let timestamp = update_check_timestamp();
    let remote = pinned_update_metadata();
    let _state_guard = runtime_state_lock().lock().await;
    let snapshot = match remote {
        Ok(remote) => record_update_check_success(
            load_update_check_snapshot(),
            timestamp,
            runtime_update_status_from_metadata(remote),
        ),
        Err(error) => record_update_check_failure(load_update_check_snapshot(), timestamp, error),
    };
    save_update_check_snapshot(&snapshot).map_err(|error| error.to_string())?;
    emit_update_check_complete(&snapshot);
    Ok(snapshot)
}

/// Read the persisted result of the most recent automatic or manual check.
#[tauri::command]
pub fn runtime_update_check_status() -> RuntimeUpdateCheckSnapshot {
    load_update_check_snapshot()
}

/// Check for runtime updates, persist the outcome, and notify an open settings
/// page. This only compares remote metadata; it never downloads or installs.
#[tauri::command]
pub async fn runtime_check_updates() -> Result<RuntimeUpdateCheckSnapshot, String> {
    check_updates_and_persist().await
}

/// Background entry point used by the hourly launcher task.
pub async fn run_scheduled_update_check() {
    match check_updates_and_persist().await {
        Ok(snapshot) => {
            if let Some(error) = snapshot.error {
                eprintln!("[runtime] scheduled update check failed: {error}");
            }
        }
        Err(error) => {
            eprintln!("[runtime] scheduled update check could not persist its result: {error}")
        }
    }
}

fn schedule_update_check() {
    tauri::async_runtime::spawn(async { run_scheduled_update_check().await });
}

#[tauri::command]
pub async fn runtime_install(window: Window, force: bool) -> Result<RuntimeStatus, String> {
    let _state_guard = runtime_state_lock()
        .try_lock()
        .map_err(|_| "Runtime operation is in progress")?;
    let _reservation = crate::operations::reserve_runtime().map_err(|e| e.to_string())?;
    let base = runtime_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    recover_interrupted_runtime_swap(&base).map_err(|error| error.to_string())?;

    let local = load_manifest();
    let installed_now = local_status(&local).installed;

    // Setup installs a missing engine; repair explicitly forces a coherent
    // reinstall. A newer remote version alone never triggers installation.
    let browser_was_missing = !installed_now;
    let need_browser = is_chromium_replacement_needed(force, browser_was_missing);

    // Repair Widevine when its required manifest is missing, regardless of the
    // persisted ETag. This is integrity repair, not an update check.
    let repair_widevine = !widevine_is_installed(&local);
    if (need_browser || repair_widevine)
        && !crate::process::Tracker::shared()
            .active_profile_ids()
            .is_empty()
    {
        return Err(
            "Stop all running or starting browsers before repairing the portable Runtime".into(),
        );
    }
    let repair_fingerprints = !fingerprints_are_installed(&local);
    if !(need_browser || repair_widevine || repair_fingerprints) {
        crate::spawn_extension_sync(false);
        return Ok(local_status(&local));
    }
    let manifest = repair_manifest(&local)?;
    install_snapshot(
        &window,
        &base,
        &manifest,
        local,
        [
            need_browser,
            need_browser || repair_widevine,
            need_browser || repair_fingerprints,
        ],
    )
    .await
    .map_err(|error| format!("{error:#}"))?;

    let _ = window.emit("runtime:done", ());
    schedule_update_check();
    crate::spawn_extension_sync(false);
    runtime_local_status()
}

/// User-triggered per-component runtime update from the update-check card.
/// Chromium replacement installs its whole snapshot. Component-only updates
/// use the same snapshot and transaction as setup and repair.
#[tauri::command]
pub async fn runtime_apply_updates(
    window: Window,
    component: String,
) -> Result<RuntimeStatus, String> {
    let _state_guard = runtime_state_lock()
        .try_lock()
        .map_err(|_| "Runtime operation is in progress")?;
    crate::process::ensure_lease_recovery_complete().map_err(|error| error.to_string())?;
    let _reservation = crate::operations::reserve_runtime().map_err(|e| e.to_string())?;
    let base = runtime_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    recover_interrupted_runtime_swap(&base).map_err(|error| error.to_string())?;
    if !crate::process::Tracker::shared()
        .active_profile_ids()
        .is_empty()
    {
        return Err("Stop all running or starting browsers before updating the runtime".into());
    }

    let manifest = bundled_manifest();
    let local = load_manifest();
    let selection = match component.as_str() {
        "chromium" => [true, true, true],
        "widevine" => [false, true, false],
        "fingerprints" => [false, false, true],
        _ => return Err(format!("Unknown runtime component: {component}")),
    };

    if let (Some(pinned), Some(installed)) = (
        manifest.chromium_version.as_deref(),
        effective_installed_version(&local),
    ) {
        if version_is_newer(&installed, Some(pinned)) {
            return Err(
                "The installed runtime is newer than this launcher's pinned snapshot".into(),
            );
        }
        if !selection[0] && installed != pinned {
            return Err(
                "Update Chromium to the pinned runtime before updating individual components"
                    .into(),
            );
        }
    }
    install_snapshot(&window, &base, &manifest, local, selection)
        .await
        .map_err(|error| format!("{error:#}"))?;

    let _ = window.emit("runtime:done", ());
    schedule_update_check();
    crate::spawn_extension_sync(false);
    runtime_local_status()
}

async fn install_snapshot(
    window: &Window,
    base: &Path,
    metadata: &RemoteManifest,
    mut next: Manifest,
    [browser, widevine, fingerprints]: [bool; 3],
) -> Result<()> {
    if !(browser || widevine || fingerprints) {
        return Ok(());
    }
    crate::profile::recover_interrupted_operations()?;
    let stage = base.join(RUNTIME_STAGE_DIR);
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    fs::create_dir_all(&stage)?;
    let mut transaction_started = false;
    let prepared = async {
        let spec = host_spec();
        let engine = stage.join(engine_root_dir());
        let mut replacements = Vec::new();
        let version = metadata
            .chromium_version
            .as_deref()
            .context("Pinned runtime has no Chromium version")?;
        if browser {
            next.browser_etag =
                Some(download_and_extract(window, &spec.browser, &stage, metadata).await?);
            let actual = engine_version_in(&engine)?;
            anyhow::ensure!(
                actual == version,
                "Runtime version {actual} does not match pinned version {version}"
            );
            next.installed_chromium_version = Some(actual);
            next.applied_chromium_version = Some(version.to_owned());
            next.applied_signature = Some(format!(
                "{version}|{}|{}",
                metadata.grease_brand.as_deref().unwrap_or_default(),
                metadata.grease_version.as_deref().unwrap_or_default()
            ));
            next.installed_metadata = Some(metadata.clone());
            replacements.push((engine.clone(), base.join(engine_root_dir())));
        }
        if widevine {
            next.widevine_etag =
                Some(download_and_extract(window, &spec.widevine, &stage, metadata).await?);
            if browser {
                place_widevine(&stage)?;
                validate_complete_engine_tree(&engine)?;
            } else {
                let cdm = stage.join("ShardX-Widevine-Win/WidevineCdm");
                validate_widevine_tree(&cdm)?;
                replacements.push((cdm, base.join(engine_root_dir()).join("WidevineCdm")));
            }
        }
        let incoming = stage.join("Incoming_Fingerprints");
        if fingerprints {
            fs::create_dir(&incoming)?;
            let archive = ArchiveSpec {
                key: FINGERPRINTS_ARCHIVE_KEY.into(),
                label: "Fingerprint library".into(),
            };
            next.fingerprints_etag =
                Some(download_and_extract(window, &archive, &incoming, metadata).await?);
        }
        let _guard = crate::process::lock_profile_resources()?;
        let staged_fp = stage.join("Fingerprints");
        if fingerprints || browser {
            let live = crate::store::fingerprints_dir()?;
            copy_runtime_descriptors(&live, &staged_fp)?;
            if fingerprints {
                let wrapped = incoming.join(FINGERPRINTS_TOP_DIR);
                let source = if wrapped.is_dir() { wrapped } else { incoming };
                let mut count = 0;
                for entry in fs::read_dir(source)? {
                    let entry = entry?;
                    if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
                        let bytes = fs::read(entry.path())?;
                        let _: serde_json::Map<String, serde_json::Value> =
                            serde_json::from_slice(&bytes)?;
                        fs::write(staged_fp.join(entry.file_name()), bytes)?;
                        count += 1;
                    }
                }
                anyhow::ensure!(count > 0, "Pinned fingerprint archive has no templates");
            }
            if browser {
                migrate_fingerprint_dir_to(
                    &staged_fp,
                    version,
                    metadata.grease_brand.as_deref(),
                    metadata.grease_version.as_deref(),
                )?;
            }
            collect_descriptor_replacements(&staged_fp, &live, &mut replacements)?;
        }
        if browser {
            let live = crate::store::profiles_dir()?;
            let staged = stage.join("Profiles");
            copy_runtime_descriptors(&live, &staged)?;
            migrate_profile_dir_to(
                &staged,
                version,
                metadata.grease_brand.as_deref(),
                metadata.grease_version.as_deref(),
            )?;
            collect_descriptor_replacements(&staged, &live, &mut replacements)?;
        }
        let state = stage.join("manifest.json");
        let bytes = serde_json::to_vec_pretty(&next)?;
        crate::store::publish_bytes(&state, &bytes)?;
        let state_backup = stage.join("manifest.json.bak");
        crate::store::publish_bytes(&state_backup, &bytes)?;
        replacements.push((state, manifest_path()?));
        replacements.push((state_backup, crate::store::backup_path(&manifest_path()?)?));
        let mut moves = Vec::new();
        for (index, (from, to)) in replacements.into_iter().enumerate() {
            if to.try_exists()? {
                moves.push(crate::profile::FileMove {
                    from: to.clone(),
                    to: stage.join("Rollback").join(index.to_string()),
                });
            }
            moves.push(crate::profile::FileMove { from, to });
        }
        transaction_started = true;
        crate::profile::transfer_files(moves, Vec::new(), vec![stage.clone()], vec![stage.clone()])
    }
    .await;
    if !transaction_started {
        fs::remove_dir_all(&stage)?;
    }
    prepared
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollbackRecoveryAction {
    RemoveRollback,
    RestoreRollback,
}

fn recover_interrupted_runtime_swap(base: &Path) -> Result<()> {
    let live_engine = base.join(engine_root_dir());
    let engine_rollback = base.join(ENGINE_ROLLBACK_DIR);
    recover_interrupted_swap(
        &live_engine,
        &engine_rollback,
        validate_complete_engine_tree,
        "engine",
    )?;

    let live_widevine = live_engine.join("WidevineCdm");
    let widevine_rollback = base.join(WIDEVINE_ROLLBACK_DIR);
    recover_interrupted_swap(
        &live_widevine,
        &widevine_rollback,
        validate_widevine_tree,
        "Widevine",
    )
}

fn recover_interrupted_swap<F>(live: &Path, rollback: &Path, validate: F, label: &str) -> Result<()>
where
    F: Fn(&Path) -> Result<()>,
{
    if !rollback.exists() {
        return Ok(());
    }
    match rollback_recovery_action(validate(live).is_ok(), validate(rollback).is_ok(), label)? {
        RollbackRecoveryAction::RemoveRollback => {
            if let Err(error) = fs::remove_dir_all(rollback) {
                eprintln!(
                    "[runtime] current {label} is valid but old rollback cleanup is still pending: {error}"
                );
            }
        }
        RollbackRecoveryAction::RestoreRollback => {
            if live.exists() {
                fs::remove_dir_all(live).with_context(|| {
                    format!("remove incomplete live {label} before restoring rollback")
                })?;
            }
            fs::rename(rollback, live)
                .with_context(|| format!("restore previous {label} from rollback"))?;
        }
    }
    Ok(())
}

fn rollback_recovery_action(
    live_is_valid: bool,
    rollback_is_valid: bool,
    label: &str,
) -> Result<RollbackRecoveryAction> {
    if live_is_valid {
        return Ok(RollbackRecoveryAction::RemoveRollback);
    }
    if rollback_is_valid {
        return Ok(RollbackRecoveryAction::RestoreRollback);
    }
    anyhow::bail!(
        "interrupted Runtime {label} swap left both live and rollback incomplete; preserving both for manual recovery"
    )
}

fn validate_engine_tree(root: &Path) -> Result<()> {
    for required in ["chrome.exe", "chrome.dll", "resources.pak"] {
        let path = root.join(required);
        let metadata =
            fs::metadata(&path).with_context(|| format!("Runtime is missing {required}"))?;
        if !metadata.is_file() || metadata.len() == 0 {
            anyhow::bail!("Runtime file is empty or invalid: {}", path.display());
        }
    }
    Ok(())
}

fn validate_complete_engine_tree(root: &Path) -> Result<()> {
    validate_engine_tree(root)?;
    validate_widevine_tree(&root.join("WidevineCdm"))
}

fn validate_widevine_tree(root: &Path) -> Result<()> {
    let manifest = root.join("manifest.json");
    let document: serde_json::Value = serde_json::from_slice(&fs::read(&manifest)?)
        .with_context(|| format!("Read Widevine manifest {}", manifest.display()))?;
    anyhow::ensure!(
        document
            .get("version")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|version| !version.is_empty()),
        "Widevine manifest has no version: {}",
        manifest.display()
    );
    let cdm = root.join("_platform_specific/win_x64/widevinecdm.dll");
    anyhow::ensure!(
        fs::metadata(&cdm).is_ok_and(|file| file.is_file() && file.len() > 0),
        "Widevine DLL is missing or empty: {}",
        cdm.display()
    );
    Ok(())
}

/// Stream archive → temp file → extract; emits `runtime:progress` events.
async fn download_and_extract(
    window: &Window,
    spec: &ArchiveSpec,
    base: &Path,
    manifest: &RemoteManifest,
) -> Result<String> {
    let snapshot = manifest
        .windows_snapshot
        .as_ref()
        .context("Runtime snapshot is not pinned")?;
    let url = format!("{}/{}-{}", snapshot.asset_base, snapshot.id, spec.key);
    let expected_size = *snapshot
        .sizes
        .get(&spec.key)
        .context("Missing pinned runtime asset size")?;
    let etag = manifest
        .archives
        .get(&spec.key)
        .context("Missing pinned runtime archive ETag")?
        .clone();
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
    let settings = crate::settings::load()?;
    let client = crate::download::client(
        &settings,
        std::time::Duration::from_secs(10),
        std::time::Duration::from_secs(60),
    )?;
    let mut resp = client.get(&url).send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    if total > MAX_RUNTIME_ARCHIVE_BYTES {
        anyhow::bail!(
            "{} exceeds the {} GiB portable Runtime download limit",
            spec.key,
            MAX_RUNTIME_ARCHIVE_BYTES / 1024 / 1024 / 1024
        );
    }

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
            let pct = received
                .checked_mul(100)
                .and_then(|value| value.checked_div(total))
                .unwrap_or(0);
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
        anyhow::ensure!(received == expected_size, "Runtime asset {} has {received} bytes; expected {expected_size}", spec.key);

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
    validate_widevine_tree(&src)?;
    let dst = base.join("ShardX-Windows").join("WidevineCdm");
    if dst.exists() {
        let _ = fs::remove_dir_all(&dst);
    }
    fs::rename(&src, &dst)?;
    let _ = fs::remove_dir(base.join("ShardX-Widevine-Win"));
    Ok(())
}

// Launcher self-update checks are intentionally disabled.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_templates_migrate_payload_and_label_and_surface_corruption() {
        let test = crate::store::TestStore::new();
        let dir = test.root.join("templates");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("template.json");
        fs::write(&path, serde_json::to_vec(&serde_json::json!({"chrome": "151", "label": "Keep", "payload": {
            "navigator": {"user_agent": "Mozilla/5.0 Chrome/151.0.0.0 Safari/537.36"}, "client_hints": {"brand_version": "151"}, "noise": {"canvas": {"seed": 12}}
        }})).unwrap()).unwrap();
        assert_eq!(
            migrate_fingerprint_dir_to(&dir, "157.0.8103.50", Some("Not)A;Brand"), Some("99"))
                .unwrap(),
            1
        );
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["chrome"], "157");
        assert_eq!(value["payload"]["client_hints"]["brand_version"], "157");
        assert_eq!(value["payload"]["noise"]["canvas"]["seed"], 12);
        assert_eq!(value["label"], "Keep");
        fs::write(&path, b"invalid JSON").unwrap();
        assert!(migrate_fingerprint_dir_to(&dir, "157.0.8103.50", None, None).is_err());
    }

    #[test]
    fn descriptor_migration_serializes_inline_proxy_without_plaintext_credentials() {
        let mut profile = crate::profile::StoredProfile {
            meta: crate::profile::StoredMeta {
                inline_proxy: Some(crate::proxy::ProxyEntry {
                    id: String::new(),
                    name: "inline".into(),
                    kind: crate::proxy::ProxyKind::Socks5,
                    host: "proxy.example".into(),
                    port: 1080,
                    username: "legacy-user".into(),
                    password: "legacy-password".into(),
                    credentials_unavailable: false,
                    protected_credentials: None,
                    country: String::new(),
                    notes: String::new(),
                }),
                ..Default::default()
            },
            config: serde_json::json!({
                "navigator": { "user_agent": "Mozilla/5.0 Chrome/151.0.0.0 Safari/537.36" },
                "client_hints": { "brand_version": "151" }
            })
            .as_object()
            .unwrap()
            .clone(),
        };

        assert!(migrate_descriptor_to(
            &mut profile.config,
            "152.0.7977.65",
            Some("Not)A;Brand"),
            Some("24"),
        ));
        let encoded = serde_json::to_string(&profile).unwrap();
        assert!(!encoded.contains("legacy-user"));
        assert!(!encoded.contains("legacy-password"));
        assert!(encoded.contains("credentials_protected"));
        assert_eq!(
            profile.config["navigator"]["user_agent"],
            "Mozilla/5.0 Chrome/152.0.0.0 Safari/537.36"
        );

        let mut new_identity = profile.config.clone();
        new_identity.insert(
            "tls".into(),
            serde_json::json!({ "cipher_suites": [1], "shuffle_extensions": false }),
        );
        let old_identity = new_identity.clone();
        let previous_metadata = RemoteManifest {
            chromium_version: Some("157.0.8103.50".into()),
            grease_version: Some("99".into()),
            tls: Some(serde_json::Map::from_iter([(
                "cipher_suites".into(),
                serde_json::json!([2]),
            )])),
            ..Default::default()
        };
        prepare_identity_for_engine(
            &mut new_identity,
            &Manifest {
                installed_chromium_version: Some("157.0.8103.50".into()),
                installed_metadata: Some(previous_metadata),
                ..Default::default()
            },
        );
        assert_eq!(new_identity["client_hints"]["brand_version"], "157");
        assert_eq!(new_identity["client_hints"]["grease_version"], "99");
        assert_eq!(new_identity["tls"]["cipher_suites"], serde_json::json!([2]));
        assert_eq!(new_identity["tls"]["shuffle_extensions"], false);
        assert_eq!(old_identity["tls"]["cipher_suites"], serde_json::json!([1]));
        let mut older_engine = old_identity.clone();
        prepare_identity_for_engine(
            &mut older_engine,
            &Manifest {
                installed_chromium_version: Some("152.0.7977.65".into()),
                ..Default::default()
            },
        );
        assert_eq!(older_engine["tls"], old_identity["tls"]);
        assert_eq!(older_engine["client_hints"]["brand_version"], "152");
    }

    #[test]
    fn local_component_updates_preserve_all_chromium_state() {
        let local = Manifest {
            browser_etag: Some("browser-etag".into()),
            widevine_etag: Some("widevine-old".into()),
            fingerprints_etag: Some("fingerprints-old".into()),
            applied_chromium_version: Some("151.0.1.2".into()),
            applied_signature: Some("151.0.1.2|OldBrand|99".into()),
            installed_chromium_version: Some("151.0.1.2".into()),
            installed_metadata: None,
        };

        let manifest = manifest_with_chromium_state(
            chromium_state(&local),
            Some("widevine-new".into()),
            Some("fingerprints-new".into()),
        );

        assert_eq!(manifest.browser_etag, local.browser_etag);
        assert_eq!(
            manifest.applied_chromium_version,
            local.applied_chromium_version
        );
        assert_eq!(manifest.applied_signature, local.applied_signature);
        assert_eq!(
            manifest.installed_chromium_version,
            local.installed_chromium_version
        );
        assert_eq!(manifest.widevine_etag.as_deref(), Some("widevine-new"));
        assert_eq!(
            manifest.fingerprints_etag.as_deref(),
            Some("fingerprints-new")
        );
    }

    #[test]
    fn chromium_install_state_updates_only_after_successful_publish() {
        let local = Manifest {
            browser_etag: Some("old-browser".into()),
            widevine_etag: Some("widevine".into()),
            fingerprints_etag: Some("fingerprints".into()),
            applied_chromium_version: Some("151.0.1.2".into()),
            applied_signature: Some("151.0.1.2|OldBrand|99".into()),
            installed_chromium_version: Some("151.0.1.2".into()),
            installed_metadata: None,
        };
        let manifest = manifest_with_chromium_state(
            updated_chromium_state(
                "152.0.2.3".into(),
                "152.0.2.3|NewBrand|24".into(),
                Some("152.0.2.3".into()),
                "new-browser".into(),
                None,
            ),
            local.widevine_etag.clone(),
            local.fingerprints_etag.clone(),
        );

        assert_eq!(manifest.browser_etag.as_deref(), Some("new-browser"));
        assert_eq!(
            manifest.applied_chromium_version.as_deref(),
            Some("152.0.2.3")
        );
        assert_eq!(
            manifest.applied_signature.as_deref(),
            Some("152.0.2.3|NewBrand|24")
        );
        assert_eq!(
            manifest.installed_chromium_version.as_deref(),
            Some("152.0.2.3")
        );
    }

    #[test]
    fn chromium_replacement_covers_missing_and_forced_repairs() {
        assert!(is_chromium_replacement_needed(false, true));
        assert!(is_chromium_replacement_needed(true, false));
        assert!(!is_chromium_replacement_needed(false, false));
    }

    #[test]
    fn repair_requires_explicit_upgrade_only_for_older_unpinned_installations() {
        let _store = crate::store::TestStore::new();
        let pinned = bundled_manifest();
        let mut installed = Manifest {
            browser_etag: Some("legacy-build".into()),
            installed_chromium_version: Some("157.0.8103.50".into()),
            ..Manifest::default()
        };
        assert!(repair_manifest(&installed).is_err());
        assert!(local_status(&installed).repair_requires_upgrade);

        installed.installed_chromium_version = Some("159.0.0.0".into());
        assert!(repair_manifest(&installed).is_err());
        assert!(!local_status(&installed).repair_requires_upgrade);

        installed.installed_metadata = Some(pinned.clone());
        assert_eq!(repair_manifest(&installed).unwrap(), pinned);
        assert!(!local_status(&installed).repair_requires_upgrade);
        assert_eq!(repair_manifest(&Manifest::default()).unwrap(), pinned);
    }

    #[test]
    fn rollback_recovery_keeps_a_valid_rollback_until_live_is_validated() {
        assert_eq!(
            rollback_recovery_action(true, true, "engine").unwrap(),
            RollbackRecoveryAction::RemoveRollback
        );
        assert_eq!(
            rollback_recovery_action(false, true, "engine").unwrap(),
            RollbackRecoveryAction::RestoreRollback
        );
        assert!(rollback_recovery_action(false, false, "engine").is_err());
    }

    #[test]
    fn update_check_snapshot_defaults_when_fields_are_absent() {
        let snapshot: RuntimeUpdateCheckSnapshot = serde_json::from_str("{}").unwrap();
        assert!(snapshot.last_attempt_at.is_none());
        assert!(snapshot.last_success_at.is_none());
        assert!(snapshot.result.is_none());
        assert!(snapshot.error.is_none());
    }

    #[test]
    fn failed_check_preserves_last_successful_result() {
        let result = RuntimeUpdateStatus {
            chromium_installed: true,
            chromium_installed_version: Some("152.0.0.0".into()),
            chromium_latest_version: Some("152.0.0.0".into()),
            chromium_update_available: false,
            chromium_download_url: Some("https://example.test/chromium.zip".into()),
            fingerprints_installed: true,
            fingerprints_check_available: true,
            fingerprints_update_available: false,
            fingerprints_download_url: Some("https://example.test/fingerprints.zip".into()),
            widevine_installed: true,
            widevine_check_available: true,
            widevine_update_available: false,
            widevine_download_url: Some("https://example.test/widevine.zip".into()),
        };
        let snapshot = record_update_check_success(
            RuntimeUpdateCheckSnapshot::default(),
            "@100".into(),
            result,
        );
        let failed = record_update_check_failure(snapshot, "@200".into(), "offline".into());

        assert_eq!(failed.last_attempt_at.as_deref(), Some("@200"));
        assert_eq!(failed.last_success_at.as_deref(), Some("@100"));
        assert_eq!(failed.error.as_deref(), Some("offline"));
        assert!(failed.result.is_some());
    }

    #[test]
    fn successful_check_replaces_error_and_records_success_time() {
        let snapshot = RuntimeUpdateCheckSnapshot {
            last_attempt_at: Some("@100".into()),
            last_success_at: Some("@90".into()),
            result: None,
            error: Some("offline".into()),
        };
        let result = RuntimeUpdateStatus {
            chromium_installed: false,
            chromium_installed_version: None,
            chromium_latest_version: Some("152.0.0.0".into()),
            chromium_update_available: false,
            chromium_download_url: Some("https://example.test/chromium.zip".into()),
            fingerprints_installed: false,
            fingerprints_check_available: false,
            fingerprints_update_available: false,
            fingerprints_download_url: Some("https://example.test/fingerprints.zip".into()),
            widevine_installed: false,
            widevine_check_available: false,
            widevine_update_available: false,
            widevine_download_url: Some("https://example.test/widevine.zip".into()),
        };
        let successful = record_update_check_success(snapshot, "@200".into(), result);

        assert_eq!(successful.last_attempt_at.as_deref(), Some("@200"));
        assert_eq!(successful.last_success_at.as_deref(), Some("@200"));
        assert!(successful.error.is_none());
        assert!(successful.result.is_some());
    }

    #[test]
    fn update_status_deserializes_precheck_availability_snapshots() {
        let status: RuntimeUpdateStatus = serde_json::from_str(
            r#"{
            "chromium_installed": true,
            "chromium_installed_version": "152.0.0.0",
            "chromium_latest_version": "152.0.0.0",
            "chromium_update_available": false,
            "chromium_download_url": null,
            "fingerprints_installed": true,
            "fingerprints_update_available": false,
            "fingerprints_download_url": null,
            "widevine_installed": true,
            "widevine_update_available": false,
            "widevine_download_url": null
        }"#,
        )
        .unwrap();

        assert!(status.fingerprints_check_available);
        assert!(status.widevine_check_available);
    }

    #[test]
    fn parses_full_manifest() {
        let text = r#"{
            "archives": {"ShardX-Windows.zip": "abc-4"},
            "chromium_version": "149.0.7827.103",
            "grease_brand": "Not)A;Brand",
            "grease_version": "24",
            "revision": 3
        }"#;
        let m = parse_remote_manifest(text).expect("manifest should parse");
        assert_eq!(m.chromium_version.as_deref(), Some("149.0.7827.103"));
        assert_eq!(
            m.archives.get("ShardX-Windows.zip").map(String::as_str),
            Some("abc-4")
        );
        assert_eq!(m.grease_brand.as_deref(), Some("Not)A;Brand"));
        assert_eq!(m.grease_version.as_deref(), Some("24"));
    }

    #[test]
    fn rejects_json_without_chromium_version() {
        // API error bodies are valid JSON but not manifests; the fallback loop
        // must skip them instead of treating an empty manifest as success.
        assert!(parse_remote_manifest(r#"{"message": "Not Found"}"#).is_none());
        assert!(parse_remote_manifest("not json at all").is_none());
    }

    #[test]
    fn decodes_github_contents_envelope() {
        let text = r#"{"chromium_version":"150.0.0.0","archives":{}}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        // GitHub wraps encoded content in newlines every 60 chars.
        let wrapped = encoded
            .as_bytes()
            .chunks(60)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let envelope: serde_json::Value = serde_json::json!({
            "encoding": "base64",
            "content": format!("{wrapped}\n"),
        });
        assert_eq!(decode_base64_envelope(&envelope).as_deref(), Some(text));
    }

    #[tokio::test]
    async fn skips_unusable_manifest_sources_in_order() {
        use axum::{http::StatusCode, routing::get, Json, Router};

        let app = Router::new()
            .route("/unavailable", get(|| async { StatusCode::SERVICE_UNAVAILABLE }))
            .route(
                "/invalid",
                get(|| async { Json(serde_json::json!({"message": "Not a manifest"})) }),
            )
            .route(
                "/mirror",
                get(|| async {
                    let content = base64::engine::general_purpose::STANDARD.encode(
                        r#"{"chromium_version":"150.0.0.0","archives":{"ShardX-Windows.zip":"abc"}}"#,
                    );
                    Json(serde_json::json!({"encoding": "base64", "content": content}))
                }),
            )
            .route(
                "/later",
                get(|| async {
                    Json(serde_json::json!({"chromium_version": "151.0.0.0"}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let invalid = ["unavailable", "invalid"].map(|source| format!("{origin}/{source}"));
        assert!(fetch_manifest_from(&client, invalid.clone())
            .await
            .is_none());
        let valid = ["mirror", "later"].map(|source| format!("{origin}/{source}"));
        let manifest = fetch_manifest_from(&client, invalid.into_iter().chain(valid))
            .await
            .unwrap();
        server.abort();
        assert_eq!(manifest.chromium_version.as_deref(), Some("150.0.0.0"));
        assert_eq!(manifest.archives["ShardX-Windows.zip"], "abc");
    }

    /// Live smoke test for the configured mirrors. The local test above
    /// exercises fallback even when the first live mirror is healthy.
    #[tokio::test]
    #[ignore = "requires network"]
    async fn fetches_manifest_via_fallback_chain() {
        let m = fetch_remote_manifest()
            .await
            .expect("at least one manifest source should work");
        assert!(m.chromium_version.is_some());
        assert!(!m.archives.is_empty());
    }
}
