use crate::store;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// Launcher-side view of a profile (wraps raw FingerprintConfig JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileMeta {
    pub id: String,
    pub name: String,
    pub notes: String,
    pub proxy_id: Option<String>,
    pub last_launched_at: Option<String>,
    pub created_at: Option<String>,
    pub pinned: bool,
    pub folder: String,
    /// Accumulated runtime across every launch; UI shows this plus the
    /// current-session uptime when the profile is running.
    #[serde(default)]
    pub total_runtime_ms: u64,
}

/// On-disk `<profiles_dir>/<id>.json`: FingerprintConfig + `_meta` envelope.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredProfile {
    #[serde(rename = "_meta", default)]
    pub meta: StoredMeta,
    /// Verbatim FingerprintConfig payload (round-trip, not parsed).
    #[serde(flatten)]
    pub config: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredMeta {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub proxy_id: Option<String>,
    #[serde(default)]
    pub last_launched_at: Option<String>,
    /// "@<unix_secs>" creation marker.
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    /// Empty = unfiled (All tab).
    #[serde(default)]
    pub folder: String,
    /// Cumulative engine uptime in milliseconds; bumped by the Tracker
    /// when the child exits.  Persists across launcher restarts.
    #[serde(default)]
    pub total_runtime_ms: u64,
    /// Source library fingerprint id; MUST round-trip — drives the editor GPU select.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_preset_id: Option<String>,
    /// Inline proxy from temporary profile API; not in proxy store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_proxy: Option<crate::proxy::ProxyEntry>,
    /// Hidden from listings; auto-deleted on close.
    #[serde(default, skip_serializing_if = "is_false")]
    pub temporary: bool,
    /// Last successfully launched network identity for this browser profile.
    /// The public IP is stored only as a SHA-256 digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_network_identity: Option<SessionNetworkIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionNetworkIdentity {
    pub proxy_id: String,
    pub exit_ip_sha256: String,
    pub country_code: String,
    pub timezone: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

pub fn validate_profile_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        anyhow::bail!("invalid profile id");
    }
    Ok(())
}

/// User-facing profile names are also passed to browser/taskbar integration,
/// so keep the accepted alphabet explicit and consistent across every entry point.
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("Profile name is required");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        anyhow::bail!(
            "Profile name may only contain letters, numbers, underscores (_), and hyphens (-)"
        );
    }
    Ok(())
}

/// Produce a valid name only for names generated internally (templates/clones).
/// Explicit user input is validated and rejected instead of being rewritten.
pub fn generated_profile_name(value: &str, fallback: &str) -> String {
    fn sanitize(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for ch in value.chars() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                out.push(ch);
            } else if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
        }
        out.trim_matches('-').to_string()
    }

    let generated = sanitize(value);
    if !generated.is_empty() {
        return generated;
    }
    let fallback = sanitize(fallback);
    if fallback.is_empty() {
        "profile".to_string()
    } else {
        fallback
    }
}

fn path_for(id: &str) -> Result<PathBuf> {
    validate_profile_id(id)?;
    Ok(store::profiles_dir()?.join(format!("{id}.json")))
}

pub fn list_all() -> Result<Vec<ProfileMeta>> {
    let dir = store::profiles_dir()?;
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let path = entry.path();
        let mut stored: StoredProfile = match store::load_json_with_backup(&path) {
            Ok(stored) => stored,
            Err(error) => {
                eprintln!("[launcher] skipping unreadable profile {}: {error}", path.display());
                continue;
            }
        };
        // Hide ephemeral profiles.
        if stored.meta.temporary {
            continue;
        }
        // Backfill legacy profiles' created_at from file mtime, then persist.
        if stored.meta.created_at.is_none() {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| format!("@{}", d.as_secs()));
            if let Some(ts) = mtime {
                stored.meta.created_at = Some(ts);
                // Listing profiles must remain read-only for an active browser.
                // Persist the legacy backfill after it stops instead.
                if !crate::is_profile_active(&stored.meta.id) {
                    if let Ok(body) = serde_json::to_string_pretty(&stored) {
                        let _ = store::atomic_write(&path, body.as_bytes());
                    }
                }
            }
        }
        let name = stored
            .config
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("(unnamed)")
            .to_string();
        let notes = stored
            .config
            .get("notes")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(ProfileMeta {
            id: stored.meta.id,
            name,
            notes,
            proxy_id: stored.meta.proxy_id,
            last_launched_at: stored.meta.last_launched_at,
            created_at: stored.meta.created_at,
            pinned: stored.meta.pinned,
            folder: stored.meta.folder,
            total_runtime_ms: stored.meta.total_runtime_ms,
        });
    }
    // Newest-first by created_at; name fallback for same-second ties. Manual
    // display order is applied by the UI-facing Tauri command afterwards.
    out.sort_by(|a, b| {
        match (&b.created_at, &a.created_at) {
            (Some(bv), Some(av)) => bv.cmp(av),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.cmp(&b.name),
        }
    });
    Ok(out)
}

/// Delete leftover temporary profiles after a crash; returns count.
pub fn purge_temporary() -> Result<usize> {
    let dir = store::profiles_dir()?;
    let mut n = 0;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = fs::read_to_string(entry.path()) else { continue; };
        let Ok(stored): std::result::Result<StoredProfile, _> = serde_json::from_str(&body) else {
            continue;
        };
        if stored.meta.temporary && !stored.meta.id.is_empty() {
            let _ = delete(&stored.meta.id);
            n += 1;
        }
    }
    Ok(n)
}

pub fn load_raw(id: &str) -> Result<StoredProfile> {
    let path = path_for(id)?;
    store::load_json_with_backup(&path)
        .with_context(|| format!("load profile {}", path.display()))
}

/// Deterministic non-zero 32-bit seed from the profile id + noise slot (FNV-1a).
/// Same id + slot always yields the same seed (stable fingerprint across
/// launches/edits); different ids yield different seeds (unique per profile).
fn derive_noise_seed(id: &str, slot: &str) -> u32 {
    let s = format!("{id}::{slot}");
    let mut h: u32 = 2166136261;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    // 0 is the "derive automatically" sentinel — never hand it back as a value.
    if h == 0 {
        1
    } else {
        h
    }
}

/// Replace every auto-sentinel noise seed (`seed == 0` or absent) with a
/// stable per-profile value derived from the final profile id.  The UI can't
/// know the id at create time, so it sends `seed: 0` for every vector; without
/// this every freshly-created profile would otherwise share one placeholder
/// seed and produce an identical canvas/audio/WebGL fingerprint.
fn fill_noise_seeds(config: &mut serde_json::Map<String, serde_json::Value>, id: &str) {
    let Some(noise) = config.get_mut("noise").and_then(|n| n.as_object_mut()) else {
        return;
    };
    for (slot, block) in noise.iter_mut() {
        let Some(obj) = block.as_object_mut() else {
            continue;
        };
        let needs = obj
            .get("seed")
            .and_then(|v| v.as_u64())
            .map(|n| n == 0)
            .unwrap_or(true);
        if needs {
            obj.insert("seed".into(), serde_json::Value::from(derive_noise_seed(id, slot)));
        }
    }
}

/// Reset every noise seed back to the auto sentinel so the next `save_raw`
/// re-derives them from a fresh id.  Used when cloning so the copy doesn't
/// inherit the source's canvas/audio/WebGL fingerprint.
fn clear_noise_seeds(config: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(noise) = config.get_mut("noise").and_then(|n| n.as_object_mut()) else {
        return;
    };
    for (_, block) in noise.iter_mut() {
        if let Some(obj) = block.as_object_mut() {
            obj.insert("seed".into(), serde_json::Value::from(0u32));
        }
    }
}

/// Reject user-initiated profile mutations while its browser owns the live
/// user-data directory. Runtime bookkeeping continues to use `save_raw`
/// directly, and temporary cleanup calls `delete` only after the tracker entry
/// is removed, so lifecycle persistence keeps working normally.
pub fn ensure_stopped(id: &str) -> Result<()> {
    if crate::is_profile_active(id) {
        anyhow::bail!("Stop the running or starting browser before modifying this profile");
    }
    Ok(())
}

/// Reject proxy endpoint mutations while any active browser depends on it.
/// Reading the active ids from the tracker also covers temporary API profiles,
/// which are intentionally hidden from the normal profile list.
fn active_proxy_bindings() -> Vec<(String, String)> {
    crate::process::Tracker::shared()
        .active_profile_ids()
        .into_iter()
        .filter_map(|profile_id| {
            load_raw(&profile_id)
                .ok()
                .and_then(|stored| stored.meta.proxy_id)
                .map(|proxy_id| (profile_id, proxy_id))
        })
        .collect()
}

pub fn active_proxy_ids() -> Vec<String> {
    active_proxy_bindings()
        .into_iter()
        .map(|(_, proxy_id)| proxy_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

pub fn ensure_proxy_not_active(proxy_id: &str) -> Result<()> {
    if proxy_id.is_empty() {
        return Ok(());
    }

    let active_count = active_proxy_bindings()
        .into_iter()
        .filter(|(_, active_proxy_id)| active_proxy_id == proxy_id)
        .count();

    if active_count > 0 {
        anyhow::bail!(
            "Stop the {active_count} running or starting browser{} using this proxy before modifying or deleting it",
            if active_count == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn session_location_changed(
    locked: &SessionNetworkIdentity,
    current: &SessionNetworkIdentity,
) -> bool {
    locked.country_code != current.country_code
        || (!locked.timezone.is_empty()
            && !current.timezone.is_empty()
            && locked.timezone != current.timezone)
}

/// Bind a browser profile's persisted sessions to the geographic identity
/// observed on its first protected launch. A same-region IP rotation is logged
/// and accepted, but a country/timezone jump on the same proxy binding is
/// blocked before Chromium can expose the account session to the new location.
pub fn enforce_session_network_identity(
    stored: &mut StoredProfile,
    proxy_id: &str,
    geo: &crate::proxy::GeoInfo,
) -> Result<()> {
    let public_ip = geo.ip.trim();
    let country_code = geo.country_code.trim().to_ascii_uppercase();
    if public_ip.is_empty() || country_code.is_empty() {
        anyhow::bail!(
            "session network identity check returned no public IP or country; browser launch cancelled"
        );
    }

    let current = SessionNetworkIdentity {
        proxy_id: proxy_id.to_string(),
        exit_ip_sha256: format!("{:x}", Sha256::digest(public_ip.as_bytes())),
        country_code,
        timezone: geo.timezone.trim().to_string(),
    };

    match stored.meta.session_network_identity.clone() {
        None => {
            stored.meta.session_network_identity = Some(current);
            save_raw(stored)?;
        }
        Some(locked) if locked.proxy_id != proxy_id => {
            stored.meta.session_network_identity = Some(current);
            save_raw(stored)?;
        }
        Some(locked) if locked.exit_ip_sha256 == current.exit_ip_sha256 => {}
        Some(locked) if session_location_changed(&locked, &current) => {
            anyhow::bail!(
                "bound proxy session identity changed from {}/{} to {}/{}; browser launch blocked to protect Google/ChatGPT sessions. Bind a stable proxy (or intentionally rebind this profile) before launching",
                locked.country_code,
                if locked.timezone.is_empty() { "unknown-timezone" } else { &locked.timezone },
                current.country_code,
                if current.timezone.is_empty() { "unknown-timezone" } else { &current.timezone },
            );
        }
        Some(locked) => {
            let mut updated = current;
            if updated.timezone.is_empty() {
                // A provider can occasionally omit timezone while still
                // returning the same country. Keep the last known timezone so
                // a transient partial response does not weaken future checks.
                updated.timezone = locked.timezone;
            }
            eprintln!(
                "[launcher] profile {} proxy exit IP rotated within {}/{}; accepting the new IP identity",
                stored.meta.id,
                updated.country_code,
                if updated.timezone.is_empty() { "unknown-timezone" } else { &updated.timezone },
            );
            stored.meta.session_network_identity = Some(updated);
            save_raw(stored)?;
        }
    }
    Ok(())
}

pub fn save_raw(stored: &mut StoredProfile) -> Result<()> {
    let is_new = stored.meta.id.is_empty();
    if is_new {
        stored.meta.id = uuid::Uuid::new_v4().to_string();
    }
    // Carry created_at/folder/last_launched_at and the legacy pinned value
    // through edits. Pinning is no longer exposed; preserving the old field
    // keeps existing profile JSON round-trippable.
    if !is_new {
        if let Ok(existing) = load_raw(&stored.meta.id) {
            if stored.meta.proxy_id != existing.meta.proxy_id {
                // An intentional rebind establishes a new trust boundary on
                // the next launch; never carry the old proxy identity across.
                stored.meta.session_network_identity = None;
            } else if stored.meta.session_network_identity.is_none() {
                stored.meta.session_network_identity =
                    existing.meta.session_network_identity.clone();
            }
            if stored.meta.created_at.is_none() {
                stored.meta.created_at = existing.meta.created_at;
            }
            stored.meta.pinned = existing.meta.pinned;
            if stored.meta.folder.is_empty() {
                stored.meta.folder = existing.meta.folder;
            }
            if stored.meta.last_launched_at.is_none() {
                stored.meta.last_launched_at = existing.meta.last_launched_at;
            }
            // total_runtime_ms is owned by the Tracker — every save (edit /
            // proxy bind / folder move) carries the existing counter through.
            if stored.meta.total_runtime_ms == 0 {
                stored.meta.total_runtime_ms = existing.meta.total_runtime_ms;
            }
        }
    }
    if stored.meta.created_at.is_none() {
        stored.meta.created_at = Some(chrono_now_iso());
    }
    // The id is now final (freshly minted for new profiles, carried through for
    // edits) — derive per-profile noise seeds from it so each profile gets a
    // unique-but-stable fingerprint instead of sharing the UI's placeholder.
    fill_noise_seeds(&mut stored.config, &stored.meta.id);
    let path = path_for(&stored.meta.id)?;
    let body = serde_json::to_string_pretty(stored)?;
    store::atomic_write(&path, body.as_bytes())?;
    Ok(())
}

pub fn delete(id: &str) -> Result<()> {
    let _resource_guard = crate::process::lock_profile_resources()?;
    ensure_stopped(id)?;
    let path = path_for(id)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    // Also wipe per-profile user-data-dir.
    let udd = store::user_data_root()?.join(id);
    if udd.exists() {
        let _ = fs::remove_dir_all(udd);
    }
    // Per-profile browser copies exist only to give Windows an independent
    // taskbar identity and NAME badge.  Remove them with the profile; a copy
    // still locked by a closing browser is retried by later launch cleanup.
    crate::taskbar_icon::remove_profile_launchers(id);
    Ok(())
}

/// Persist a fully restored profile under its already-assigned fresh id.
/// Unlike `save_raw`, this deliberately does not derive or fill fingerprint
/// noise seeds: a complete backup must retain the source browser's exact
/// canvas/audio/WebGL identity.
pub fn save_restored(stored: &mut StoredProfile) -> Result<()> {
    if stored.meta.id.is_empty() {
        anyhow::bail!("restored profile id is required");
    }
    let path = path_for(&stored.meta.id)?;
    if path.exists() {
        anyhow::bail!("restored profile id already exists");
    }
    if stored.meta.created_at.is_none() {
        stored.meta.created_at = Some(chrono_now_iso());
    }
    store::atomic_write(&path, serde_json::to_string_pretty(stored)?.as_bytes())?;
    Ok(())
}

/// Add `ms` to the persisted total_runtime_ms counter.  Called by the
/// process Tracker when the engine exits — totals survive launcher restarts.
pub fn add_runtime(id: &str, ms: u64) -> Result<()> {
    let mut p = load_raw(id)?;
    p.meta.total_runtime_ms = p.meta.total_runtime_ms.saturating_add(ms);
    save_raw(&mut p)?;
    Ok(())
}

/// Touch last_launched_at; optionally switch bound proxy.
pub fn touch_launched(id: &str, proxy_id: Option<String>) -> Result<()> {
    let mut p = load_raw(id)?;
    p.meta.last_launched_at = Some(chrono_now_iso());
    if proxy_id.is_some() {
        p.meta.proxy_id = proxy_id;
    }
    save_raw(&mut p)?;
    Ok(())
}

pub fn clone_profile(id: &str) -> Result<ProfileMeta> {
    let _resource_guard = crate::process::lock_profile_resources()?;
    ensure_stopped(id)?;
    let mut src = load_raw(id)?;
    let new_id = uuid::Uuid::new_v4().to_string();
    let old_name = src
        .config
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("profile")
        .to_string();
    let clone_name = format!("{}-C", generated_profile_name(&old_name, "profile"));
    src.meta.id = new_id.clone();
    src.meta.last_launched_at = None;
    src.meta.created_at = None;
    src.meta.pinned = false;
    src.meta.total_runtime_ms = 0;
    src.meta.session_network_identity = None;
    src.config
        .insert("name".into(), serde_json::Value::String(clone_name.clone()));
    // Re-randomize CPU/RAM/platform_version so the copy doesn't collide on those axes.
    crate::randomize_platform_version(&mut src.config);
    crate::randomize_hardware(&mut src.config);
    // Same reasoning for the fingerprint noise: drop the source's seeds so
    // save_raw re-derives fresh ones from new_id, giving the copy its own
    // canvas/audio/WebGL fingerprint instead of a clone of the original's.
    clear_noise_seeds(&mut src.config);
    save_raw(&mut src)?;
    Ok(ProfileMeta {
        id: src.meta.id,
        name: clone_name,
        notes: src
            .config
            .get("notes")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        proxy_id: src.meta.proxy_id,
        last_launched_at: None,
        created_at: src.meta.created_at,
        pinned: false,
        folder: src.meta.folder,
        total_runtime_ms: 0,
    })
}

/// Assign folder tag (empty string clears).
pub fn set_folder(id: &str, folder: &str) -> Result<()> {
    ensure_stopped(id)?;
    let mut p = load_raw(id)?;
    p.meta.folder = folder.trim().to_string();
    let path = path_for(&p.meta.id)?;
    let body = serde_json::to_string_pretty(&p)?;
    store::atomic_write(&path, body.as_bytes())?;
    Ok(())
}

fn profiles_in_folder(name: &str) -> Result<Vec<(PathBuf, StoredProfile)>> {
    let dir = store::profiles_dir()?;
    let mut profiles = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let path = entry.path();
        let body = fs::read_to_string(&path)?;
        let stored: StoredProfile = serde_json::from_str(&body)
            .with_context(|| format!("parse profile {}", path.display()))?;
        if stored.meta.folder == name {
            profiles.push((path, stored));
        }
    }
    Ok(profiles)
}

/// Retag profiles from folder `old` to `new`; returns count.
pub fn rename_folder(old: &str, new: &str) -> Result<usize> {
    let profiles = profiles_in_folder(old)?;
    // Preflight the whole folder so a running profile cannot cause a partial
    // rename after earlier profiles were already written.
    for (_, stored) in &profiles {
        ensure_stopped(&stored.meta.id)?;
    }
    let count = profiles.len();
    let new = new.trim();
    for (path, mut stored) in profiles {
        ensure_stopped(&stored.meta.id)?;
        stored.meta.folder = new.to_string();
        store::atomic_write(&path, serde_json::to_string_pretty(&stored)?.as_bytes())?;
    }
    Ok(count)
}

/// Delete folder; `delete_profiles` true removes, false unfiles. Returns count.
pub fn delete_folder(name: &str, delete_profiles: bool) -> Result<usize> {
    let profiles = profiles_in_folder(name)?;
    // As with rename, reject before changing anything when one member runs.
    for (_, stored) in &profiles {
        ensure_stopped(&stored.meta.id)?;
    }
    let count = profiles.len();
    for (path, mut stored) in profiles {
        if delete_profiles {
            delete(&stored.meta.id)?;
        } else {
            ensure_stopped(&stored.meta.id)?;
            stored.meta.folder = String::new();
            store::atomic_write(&path, serde_json::to_string_pretty(&stored)?.as_bytes())?;
        }
    }
    Ok(count)
}

/// Per-profile user-data-dir; created on first call.
pub fn user_data_dir(id: &str) -> Result<PathBuf> {
    validate_profile_id(id)?;
    let p = store::user_data_root()?.join(id);
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

fn chrono_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{s}")
}

#[cfg(test)]
mod tests {
    use super::{session_location_changed, SessionNetworkIdentity};

    fn identity(country_code: &str, timezone: &str) -> SessionNetworkIdentity {
        SessionNetworkIdentity {
            proxy_id: "proxy-1".into(),
            exit_ip_sha256: "digest".into(),
            country_code: country_code.into(),
            timezone: timezone.into(),
        }
    }

    #[test]
    fn session_location_detects_country_change() {
        let locked = identity("US", "America/Los_Angeles");
        let current = identity("DE", "Europe/Berlin");

        assert!(session_location_changed(&locked, &current));
    }

    #[test]
    fn session_location_detects_timezone_change() {
        let locked = identity("US", "America/Los_Angeles");
        let current = identity("US", "America/New_York");

        assert!(session_location_changed(&locked, &current));
    }

    #[test]
    fn session_location_accepts_same_identity() {
        let locked = identity("DE", "Europe/Berlin");
        let current = identity("DE", "Europe/Berlin");

        assert!(!session_location_changed(&locked, &current));
    }

    #[test]
    fn session_location_ignores_timezone_when_either_side_is_empty() {
        let known = identity("US", "America/Los_Angeles");
        let unknown = identity("US", "");

        assert!(!session_location_changed(&known, &unknown));
        assert!(!session_location_changed(&unknown, &known));
    }
}
