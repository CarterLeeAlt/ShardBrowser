use crate::store;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::proxy::protected_proxy_serde"
    )]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProfileOperation {
    Retag {
        changes: Vec<ProfileRetag>,
        #[serde(default = "default_apply_after")]
        apply_after: bool,
    },
    Delete {
        targets: Vec<ProfileDeleteTarget>,
        #[serde(default = "default_delete_commit")]
        commit: bool,
    },
}

fn default_apply_after() -> bool {
    true
}

fn default_delete_commit() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileRetag {
    profile_id: String,
    path: PathBuf,
    before: StoredProfile,
    after: StoredProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileDeleteTarget {
    profile_id: String,
    profile_path: PathBuf,
    profile_backup_path: PathBuf,
    user_data_path: PathBuf,
    profile_quarantine: PathBuf,
    profile_backup_quarantine: PathBuf,
    user_data_quarantine: PathBuf,
}

fn persist_operation(operation: &ProfileOperation) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(operation)?;
    store::atomic_write_sensitive(&store::profile_operation_journal_path()?, &bytes)
}

fn clear_operation() -> Result<()> {
    let path = store::profile_operation_journal_path()?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("remove profile operation journal {}", path.display()))?;
    }
    let backup = store::backup_path(&path)?;
    if backup.exists() {
        fs::remove_file(&backup)
            .with_context(|| format!("remove profile operation backup {}", backup.display()))?;
    }
    Ok(())
}

fn load_operation() -> Result<Option<ProfileOperation>> {
    let path = store::profile_operation_journal_path()?;
    if path.exists() {
        return store::load_json_with_backup(&path)
            .with_context(|| format!("load profile operation journal {}", path.display()))
            .map(Some);
    }
    let backup = store::backup_path(&path)?;
    if !backup.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&backup)
        .with_context(|| format!("read profile operation backup {}", backup.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse profile operation backup {}", backup.display()))
        .map(Some)
}

fn write_profile_snapshot(path: &Path, stored: &StoredProfile) -> Result<()> {
    store::atomic_write_sensitive(path, &serde_json::to_vec_pretty(stored)?)
}

fn apply_retags(changes: &[ProfileRetag], use_after: bool) -> Result<()> {
    for change in changes {
        ensure_stopped(&change.profile_id)?;
        write_profile_snapshot(
            &change.path,
            if use_after {
                &change.after
            } else {
                &change.before
            },
        )?;
    }
    Ok(())
}

fn move_to_quarantine(live: &Path, quarantine: &Path) -> Result<()> {
    if quarantine.exists() {
        if live.exists() {
            anyhow::bail!(
                "profile deletion quarantine conflict: both {} and {} exist",
                live.display(),
                quarantine.display()
            );
        }
        return Ok(());
    }
    if live.exists() {
        fs::rename(live, quarantine).with_context(|| {
            format!(
                "isolate profile deletion target {} as {}",
                live.display(),
                quarantine.display()
            )
        })?;
    }
    Ok(())
}

fn quarantine_delete_targets(targets: &[ProfileDeleteTarget]) -> Result<()> {
    for target in targets {
        ensure_stopped(&target.profile_id)?;
        move_to_quarantine(&target.profile_path, &target.profile_quarantine)?;
        move_to_quarantine(
            &target.profile_backup_path,
            &target.profile_backup_quarantine,
        )?;
        move_to_quarantine(&target.user_data_path, &target.user_data_quarantine)?;
    }
    Ok(())
}

fn restore_delete_targets(targets: &[ProfileDeleteTarget]) -> Result<()> {
    for target in targets.iter().rev() {
        if target.user_data_quarantine.exists() {
            if target.user_data_path.exists() {
                anyhow::bail!(
                    "cannot restore profile user-data because both {} and {} exist",
                    target.user_data_path.display(),
                    target.user_data_quarantine.display()
                );
            }
            fs::rename(&target.user_data_quarantine, &target.user_data_path)?;
        }
        if target.profile_backup_quarantine.exists() {
            if target.profile_backup_path.exists() {
                anyhow::bail!(
                    "cannot restore profile metadata backup because both {} and {} exist",
                    target.profile_backup_path.display(),
                    target.profile_backup_quarantine.display()
                );
            }
            fs::rename(
                &target.profile_backup_quarantine,
                &target.profile_backup_path,
            )?;
        }
        if target.profile_quarantine.exists() {
            if target.profile_path.exists() {
                anyhow::bail!(
                    "cannot restore profile metadata because both {} and {} exist",
                    target.profile_path.display(),
                    target.profile_quarantine.display()
                );
            }
            fs::rename(&target.profile_quarantine, &target.profile_path)?;
        }
    }
    Ok(())
}

fn commit_quarantined_delete(targets: &[ProfileDeleteTarget]) -> Result<()> {
    for target in targets {
        if target.profile_quarantine.exists() {
            fs::remove_file(&target.profile_quarantine).with_context(|| {
                format!(
                    "remove quarantined profile metadata {}",
                    target.profile_quarantine.display()
                )
            })?;
        }
        if target.profile_backup_quarantine.exists() {
            fs::remove_file(&target.profile_backup_quarantine).with_context(|| {
                format!(
                    "remove quarantined profile metadata backup {}",
                    target.profile_backup_quarantine.display()
                )
            })?;
        }
        if target.user_data_quarantine.exists() {
            fs::remove_dir_all(&target.user_data_quarantine).with_context(|| {
                format!(
                    "remove quarantined profile user-data {}",
                    target.user_data_quarantine.display()
                )
            })?;
        }
    }
    Ok(())
}

fn recover_interrupted_operations_locked() -> Result<()> {
    let Some(operation) = load_operation()? else {
        return Ok(());
    };
    match &operation {
        ProfileOperation::Retag {
            changes,
            apply_after,
        } => apply_retags(changes, *apply_after)?,
        ProfileOperation::Delete { targets, commit } => {
            if *commit {
                quarantine_delete_targets(targets)?;
                commit_quarantined_delete(targets)?;
                for target in targets {
                    crate::taskbar_icon::remove_profile_launchers(&target.profile_id);
                }
            } else {
                restore_delete_targets(targets)?;
            }
        }
    }
    clear_operation()
}

/// Resume a previously interrupted profile folder or deletion transaction. The
/// launcher calls this before temporary cleanup; individual profile operations
/// call the locked variant before starting a new transaction.
pub fn recover_interrupted_operations() -> Result<()> {
    let _resource_guard = crate::process::lock_profile_resources()?;
    recover_interrupted_operations_locked()
}

fn quarantine_path(live: &Path, profile_id: &str, kind: &str) -> Result<PathBuf> {
    let parent = live
        .parent()
        .context("profile deletion target has no parent directory")?;
    Ok(parent.join(format!(
        ".{profile_id}.delete-{kind}-{}",
        uuid::Uuid::new_v4().simple()
    )))
}

fn delete_targets_transactionally(targets: Vec<ProfileDeleteTarget>) -> Result<()> {
    if targets.is_empty() {
        return Ok(());
    }
    persist_operation(&ProfileOperation::Delete {
        targets: targets.clone(),
        commit: true,
    })?;

    if let Err(error) = quarantine_delete_targets(&targets) {
        persist_operation(&ProfileOperation::Delete {
            targets: targets.clone(),
            commit: false,
        })?;
        match restore_delete_targets(&targets) {
            Ok(()) => {
                clear_operation()?;
                return Err(error);
            }
            Err(rollback) => {
                return Err(anyhow::anyhow!(
                    "{error}; profile deletion rollback was incomplete: {rollback}"
                ));
            }
        }
    }
    // The journal remains until startup can finish the deletion. All live
    // paths have already been isolated, so no partially deleted profile is
    // exposed to the normal profile list.
    commit_quarantined_delete(&targets)?;
    clear_operation()?;
    for target in targets {
        crate::taskbar_icon::remove_profile_launchers(&target.profile_id);
    }
    Ok(())
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
                eprintln!(
                    "[launcher] skipping unreadable profile {}: {error}",
                    path.display()
                );
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
                if !crate::is_profile_active(&stored.meta.id)
                    && !crate::process::lease_recovery_is_incomplete()
                {
                    if let Ok(body) = serde_json::to_string_pretty(&stored) {
                        if stored.meta.inline_proxy.is_some() {
                            let _ = store::atomic_write_sensitive(&path, body.as_bytes());
                        } else {
                            let _ = store::atomic_write(&path, body.as_bytes());
                        }
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
    out.sort_by(|a, b| match (&b.created_at, &a.created_at) {
        (Some(bv), Some(av)) => bv.cmp(av),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
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
        let Ok(body) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(stored): std::result::Result<StoredProfile, _> = serde_json::from_str(&body) else {
            continue;
        };
        if stored.meta.temporary && !stored.meta.id.is_empty() {
            delete(&stored.meta.id)?;
            n += 1;
        }
    }
    Ok(n)
}

pub fn load_raw(id: &str) -> Result<StoredProfile> {
    let path = path_for(id)?;
    let stored: StoredProfile = store::load_json_with_backup(&path)
        .with_context(|| format!("load profile {}", path.display()))?;
    let has_legacy_inline_credentials = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.pointer("/_meta/inline_proxy").cloned())
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|inline| inline.contains_key("username") || inline.contains_key("password"));
    if has_legacy_inline_credentials {
        let body = serde_json::to_vec_pretty(&stored)?;
        store::atomic_write_sensitive(&path, &body)?;
    }
    Ok(stored)
}

/// Count how many stored browser identities use each fingerprint-library
/// template. Temporary profiles count too while they exist because they are
/// still active identities that should not bias the next automatic pick.
pub fn gpu_preset_usage_counts() -> Result<HashMap<String, usize>> {
    let dir = store::profiles_dir()?;
    let mut counts = HashMap::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let path = entry.path();
        let stored: StoredProfile = match store::load_json_with_backup(&path) {
            Ok(stored) => stored,
            Err(error) => {
                eprintln!(
                    "[launcher] skipping unreadable profile {} while counting fingerprint templates: {error}",
                    path.display()
                );
                continue;
            }
        };
        let preset_id = stored
            .meta
            .gpu_preset_id
            .clone()
            .or_else(|| crate::infer_gpu_preset_id(&stored.config));
        if let Some(preset_id) = preset_id {
            *counts.entry(preset_id).or_insert(0) += 1;
        }
    }
    Ok(counts)
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
            obj.insert(
                "seed".into(),
                serde_json::Value::from(derive_noise_seed(id, slot)),
            );
        }
    }
}

/// Reset every noise seed back to the auto sentinel so the next identity save
/// re-derives them from a fresh id. Used by clone and collision retry paths.
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

/// An explicit Real -> Auto noise change on a legacy profile may introduce an
/// enabled block that never had a seed. Seed only that newly enabled vector;
/// unchanged existing vectors, including legacy zero values, remain verbatim.
fn fill_newly_enabled_noise_seeds(
    config: &mut serde_json::Map<String, serde_json::Value>,
    existing: &serde_json::Map<String, serde_json::Value>,
    id: &str,
) {
    let existing_noise = existing.get("noise").and_then(|value| value.as_object());
    let Some(noise) = config
        .get_mut("noise")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };

    for (slot, block) in noise.iter_mut() {
        let Some(block) = block.as_object_mut() else {
            continue;
        };
        let enabled = block
            .get("enabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let was_enabled = existing_noise
            .and_then(|noise| noise.get(slot))
            .and_then(|value| value.get("enabled"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let needs_seed = block
            .get("seed")
            .and_then(|value| value.as_u64())
            .map(|seed| seed == 0)
            .unwrap_or(true);
        if enabled && !was_enabled && needs_seed {
            block.insert(
                "seed".into(),
                serde_json::Value::from(derive_noise_seed(id, slot)),
            );
        }
    }
}

/// Browser-effective comparison form. Names and launcher notes are not exposed
/// to page JavaScript, and disabled noise blocks have no effect regardless of
/// the seed or inactive tuning values stored in them.
fn effective_fingerprint(config: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let mut normalized = config.clone();
    normalized.remove("name");
    normalized.remove("notes");

    if let Some(noise) = normalized.get("noise").and_then(|value| value.as_object()) {
        let enabled = noise
            .iter()
            .filter(|(_, block)| {
                block
                    .get("enabled")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
            })
            .map(|(slot, block)| (slot.clone(), block.clone()))
            .collect::<serde_json::Map<_, _>>();
        if enabled.is_empty() {
            normalized.remove("noise");
        } else {
            normalized.insert("noise".into(), serde_json::Value::Object(enabled));
        }
    }

    serde_json::Value::Object(normalized)
}

fn enabled_noise_present(config: &serde_json::Map<String, serde_json::Value>) -> bool {
    config
        .get("noise")
        .and_then(|value| value.as_object())
        .map(|noise| {
            noise.values().any(|block| {
                block
                    .get("enabled")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn colliding_profile_id(
    config: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<String>> {
    let candidate = effective_fingerprint(config);
    let dir = store::profiles_dir()?;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let path = entry.path();
        let stored: StoredProfile = match store::load_json_with_backup(&path) {
            Ok(stored) => stored,
            Err(error) => {
                eprintln!(
                    "[launcher] skipping unreadable profile {} during fingerprint collision check: {error}",
                    path.display()
                );
                continue;
            }
        };
        if effective_fingerprint(&stored.config) == candidate {
            return Ok(Some(stored.meta.id));
        }
    }
    Ok(None)
}

/// Persist a fresh browser identity only after its effective fingerprint is
/// distinct from every stored profile. A collision can be resolved by deriving
/// fresh seeds when at least one noise vector is enabled; an all-real duplicate
/// is rejected because changing the UUID would not change its fingerprint.
pub fn save_new_unique(stored: &mut StoredProfile) -> Result<()> {
    const MAX_ATTEMPTS: usize = 16;

    if stored.meta.id.is_empty() {
        stored.meta.id = uuid::Uuid::new_v4().to_string();
    }

    for attempt in 0..MAX_ATTEMPTS {
        let path = path_for(&stored.meta.id)?;
        if path.exists() {
            if attempt + 1 == MAX_ATTEMPTS {
                anyhow::bail!("could not allocate a unique browser profile id");
            }
            stored.meta.id = uuid::Uuid::new_v4().to_string();
            clear_noise_seeds(&mut stored.config);
            continue;
        }

        fill_noise_seeds(&mut stored.config, &stored.meta.id);
        match colliding_profile_id(&stored.config)? {
            None => return save_raw(stored),
            Some(existing_id) if enabled_noise_present(&stored.config) => {
                if attempt + 1 == MAX_ATTEMPTS {
                    anyhow::bail!(
                        "could not generate a unique browser fingerprint after {MAX_ATTEMPTS} attempts"
                    );
                }
                eprintln!(
                    "[launcher] fresh profile fingerprint collided with {existing_id}; deriving new noise seeds"
                );
                stored.meta.id = uuid::Uuid::new_v4().to_string();
                clear_noise_seeds(&mut stored.config);
            }
            Some(existing_id) => {
                anyhow::bail!(
                    "This browser fingerprint duplicates existing profile {existing_id}. Enable Auto noise for Canvas, WebGL, or ClientRects, or choose a different device template."
                );
            }
        }
    }

    anyhow::bail!("could not generate a unique browser fingerprint")
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

/// How the live exit compares to the identity locked on a previous launch.
enum SessionIdentityShift {
    /// Same exit IP — nothing to report.
    SameExitIp,
    /// New exit IP within the locked country and timezone.
    Rotated,
    /// New exit IP whose country or timezone differs from the locked one.
    Relocated,
}

fn session_identity_shift(
    locked: &SessionNetworkIdentity,
    current: &SessionNetworkIdentity,
) -> SessionIdentityShift {
    if locked.exit_ip_sha256 == current.exit_ip_sha256 {
        SessionIdentityShift::SameExitIp
    } else if session_location_changed(locked, current) {
        SessionIdentityShift::Relocated
    } else {
        SessionIdentityShift::Rotated
    }
}

/// User-facing notice for an exit change. Exit changes never block a launch;
/// they are surfaced so the user knows their sessions may have jumped
/// location. Every shift re-anchors the stored identity, so a notice fires
/// once per actual exit change.
fn session_identity_warning(
    shift: SessionIdentityShift,
    locked: &SessionNetworkIdentity,
    current: &SessionNetworkIdentity,
) -> Option<String> {
    let timezone_label = |timezone: &str| {
        if timezone.is_empty() {
            "unknown-timezone".to_string()
        } else {
            timezone.to_string()
        }
    };
    match shift {
        SessionIdentityShift::SameExitIp => None,
        SessionIdentityShift::Rotated => Some(format!(
            "Proxy exit IP rotated within {}/{}; session identity re-anchored to the new exit IP",
            current.country_code,
            timezone_label(&current.timezone)
        )),
        SessionIdentityShift::Relocated => Some(format!(
            "Proxy exit location changed from {}/{} to {}/{}; launch allowed and session identity re-anchored",
            locked.country_code,
            timezone_label(&locked.timezone),
            current.country_code,
            timezone_label(&current.timezone),
        )),
    }
}

/// Bind a browser profile's persisted sessions to the geographic identity
/// observed on its first protected launch. Every later launch re-checks the
/// exit through the bound proxy and re-anchors the stored identity in place;
/// an exit change (same-region rotation or a country/timezone jump) returns a
/// warning to the caller instead of blocking the launch.
pub fn enforce_session_network_identity(
    stored: &mut StoredProfile,
    proxy_id: &str,
    geo: &crate::proxy::GeoInfo,
) -> Result<Option<String>> {
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
        Some(locked) => {
            let shift = session_identity_shift(&locked, &current);
            let mut updated = current.clone();
            if matches!(shift, SessionIdentityShift::Rotated) && updated.timezone.is_empty() {
                // A provider can occasionally omit timezone while still
                // returning the same country. Keep the last known timezone so
                // a transient partial response does not weaken future checks.
                updated.timezone = locked.timezone.clone();
            }
            stored.meta.session_network_identity = Some(updated);
            save_raw(stored)?;
            if let Some(warning) = session_identity_warning(shift, &locked, &current) {
                eprintln!("[launcher] profile {}: {warning}", stored.meta.id);
                return Ok(Some(warning));
            }
        }
    }
    Ok(None)
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
            fill_newly_enabled_noise_seeds(&mut stored.config, &existing.config, &stored.meta.id);
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
    // Only a genuinely fresh profile receives derived seeds here. Existing
    // identities must round-trip their fingerprint verbatim during ordinary
    // edits, runtime accounting, proxy binding, and metadata backfills.
    if is_new {
        fill_noise_seeds(&mut stored.config, &stored.meta.id);
    }
    let path = path_for(&stored.meta.id)?;
    let body = serde_json::to_string_pretty(stored)?;
    store::atomic_write_sensitive(&path, body.as_bytes())?;
    Ok(())
}

pub fn delete(id: &str) -> Result<()> {
    let _resource_guard = crate::process::lock_profile_resources()?;
    ensure_stopped(id)?;
    recover_interrupted_operations_locked()?;

    let profile_path = path_for(id)?;
    let profile_backup_path = store::backup_path(&profile_path)?;
    let user_data_path = store::user_data_root()?.join(id);
    if !profile_path.exists() && !profile_backup_path.exists() && !user_data_path.exists() {
        return Ok(());
    }
    let target = ProfileDeleteTarget {
        profile_id: id.to_string(),
        profile_quarantine: quarantine_path(&profile_path, id, "profile")?,
        profile_backup_quarantine: quarantine_path(&profile_backup_path, id, "profile-backup")?,
        user_data_quarantine: quarantine_path(&user_data_path, id, "user-data")?,
        profile_path,
        profile_backup_path,
        user_data_path,
    };
    delete_targets_transactionally(vec![target])
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
    store::atomic_write_sensitive(&path, serde_json::to_string_pretty(stored)?.as_bytes())?;
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
    // A clone is a new browser identity. Keep the source untouched, but apply
    // the current default noise policy and derive fresh seeds for the copy.
    crate::apply_default_noise(&mut src.config);
    save_new_unique(&mut src)?;
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
    store::atomic_write_sensitive(&path, body.as_bytes())?;
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
    let _resource_guard = crate::process::lock_profile_resources()?;
    recover_interrupted_operations_locked()?;
    let profiles = profiles_in_folder(old)?;
    let new = new.trim();
    let changes = profiles
        .into_iter()
        .map(|(path, before)| {
            ensure_stopped(&before.meta.id)?;
            let mut after = before.clone();
            after.meta.folder = new.to_string();
            Ok(ProfileRetag {
                profile_id: before.meta.id.clone(),
                path,
                before,
                after,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if changes.is_empty() {
        return Ok(0);
    }

    persist_operation(&ProfileOperation::Retag {
        changes: changes.clone(),
        apply_after: true,
    })?;
    if let Err(error) = apply_retags(&changes, true) {
        persist_operation(&ProfileOperation::Retag {
            changes: changes.clone(),
            apply_after: false,
        })?;
        match apply_retags(&changes, false) {
            Ok(()) => {
                clear_operation()?;
                return Err(error);
            }
            Err(rollback) => {
                return Err(anyhow::anyhow!(
                    "{error}; folder rename rollback was incomplete: {rollback}"
                ));
            }
        }
    }
    clear_operation()?;
    Ok(changes.len())
}

/// Delete folder; `delete_profiles` true removes, false unfiles. Returns count.
pub fn delete_folder(name: &str, delete_profiles: bool) -> Result<usize> {
    let _resource_guard = crate::process::lock_profile_resources()?;
    recover_interrupted_operations_locked()?;
    let profiles = profiles_in_folder(name)?;
    if profiles.is_empty() {
        return Ok(0);
    }
    for (_, stored) in &profiles {
        ensure_stopped(&stored.meta.id)?;
    }

    if !delete_profiles {
        let changes = profiles
            .into_iter()
            .map(|(path, before)| {
                let mut after = before.clone();
                after.meta.folder.clear();
                ProfileRetag {
                    profile_id: before.meta.id.clone(),
                    path,
                    before,
                    after,
                }
            })
            .collect::<Vec<_>>();
        persist_operation(&ProfileOperation::Retag {
            changes: changes.clone(),
            apply_after: true,
        })?;
        if let Err(error) = apply_retags(&changes, true) {
            persist_operation(&ProfileOperation::Retag {
                changes: changes.clone(),
                apply_after: false,
            })?;
            match apply_retags(&changes, false) {
                Ok(()) => {
                    clear_operation()?;
                    return Err(error);
                }
                Err(rollback) => {
                    return Err(anyhow::anyhow!(
                        "{error}; folder unfile rollback was incomplete: {rollback}"
                    ));
                }
            }
        }
        clear_operation()?;
        return Ok(changes.len());
    }

    let targets = profiles
        .into_iter()
        .map(|(profile_path, stored)| {
            let profile_id = stored.meta.id;
            let profile_backup_path = store::backup_path(&profile_path)?;
            let user_data_path = store::user_data_root()?.join(&profile_id);
            Ok(ProfileDeleteTarget {
                profile_quarantine: quarantine_path(&profile_path, &profile_id, "profile")?,
                profile_backup_quarantine: quarantine_path(
                    &profile_backup_path,
                    &profile_id,
                    "profile-backup",
                )?,
                user_data_quarantine: quarantine_path(&user_data_path, &profile_id, "user-data")?,
                profile_id,
                profile_path,
                profile_backup_path,
                user_data_path,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let count = targets.len();
    delete_targets_transactionally(targets)?;
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
    use super::{
        effective_fingerprint, fill_newly_enabled_noise_seeds, session_identity_shift,
        session_identity_warning, session_location_changed, ProfileDeleteTarget, ProfileOperation,
        ProfileRetag, SessionIdentityShift, SessionNetworkIdentity, StoredProfile,
    };
    use serde_json::json;
    use std::path::PathBuf;

    fn identity(country_code: &str, timezone: &str) -> SessionNetworkIdentity {
        SessionNetworkIdentity {
            proxy_id: "proxy-1".into(),
            exit_ip_sha256: "digest".into(),
            country_code: country_code.into(),
            timezone: timezone.into(),
        }
    }

    fn identity_with_digest(
        digest: &str,
        country_code: &str,
        timezone: &str,
    ) -> SessionNetworkIdentity {
        SessionNetworkIdentity {
            exit_ip_sha256: digest.into(),
            ..identity(country_code, timezone)
        }
    }

    #[test]
    fn session_identity_shift_classifies_exit_changes() {
        let locked = identity_with_digest("digest-1", "US", "Pacific/Honolulu");

        // Same exit IP stays quiet regardless of any provider drift.
        let same_ip = identity_with_digest("digest-1", "US", "America/Los_Angeles");
        assert!(matches!(
            session_identity_shift(&locked, &same_ip),
            SessionIdentityShift::SameExitIp
        ));
        assert!(
            session_identity_warning(SessionIdentityShift::SameExitIp, &locked, &same_ip).is_none()
        );

        // New exit IP within the same country and timezone rotates.
        let rotated = identity_with_digest("digest-2", "US", "Pacific/Honolulu");
        assert!(matches!(
            session_identity_shift(&locked, &rotated),
            SessionIdentityShift::Rotated
        ));
        let warning =
            session_identity_warning(SessionIdentityShift::Rotated, &locked, &rotated).unwrap();
        assert!(warning.contains("rotated within US/Pacific/Honolulu"));

        // New exit IP with a timezone jump relocates and warns with both ends.
        let relocated = identity_with_digest("digest-2", "US", "America/Los_Angeles");
        assert!(matches!(
            session_identity_shift(&locked, &relocated),
            SessionIdentityShift::Relocated
        ));
        let warning =
            session_identity_warning(SessionIdentityShift::Relocated, &locked, &relocated).unwrap();
        assert!(warning.contains("from US/Pacific/Honolulu"));
        assert!(warning.contains("to US/America/Los_Angeles"));

        // A provider that omits the timezone reports as unknown-timezone.
        let tzless = identity_with_digest("digest-2", "DE", "");
        let warning =
            session_identity_warning(SessionIdentityShift::Relocated, &locked, &tzless).unwrap();
        assert!(warning.contains("to DE/unknown-timezone"));
    }

    #[test]
    fn profile_operation_journal_records_recovery_intent() {
        let retag = ProfileOperation::Retag {
            changes: vec![ProfileRetag {
                profile_id: "profile-1".into(),
                path: PathBuf::from("profiles/profile-1.json"),
                before: StoredProfile::default(),
                after: StoredProfile::default(),
            }],
            apply_after: false,
        };
        let retag_json = serde_json::to_value(&retag).unwrap();
        assert_eq!(retag_json["kind"], "retag");
        assert_eq!(retag_json["apply_after"], false);

        let delete = ProfileOperation::Delete {
            targets: vec![ProfileDeleteTarget {
                profile_id: "profile-1".into(),
                profile_path: PathBuf::from("profiles/profile-1.json"),
                profile_backup_path: PathBuf::from("profiles/profile-1.json.bak"),
                user_data_path: PathBuf::from("user-data/profile-1"),
                profile_quarantine: PathBuf::from("profiles/.profile-1.delete-profile"),
                profile_backup_quarantine: PathBuf::from(
                    "profiles/.profile-1.delete-profile-backup",
                ),
                user_data_quarantine: PathBuf::from("user-data/.profile-1.delete-user-data"),
            }],
            commit: false,
        };
        let delete_json = serde_json::to_value(&delete).unwrap();
        assert_eq!(delete_json["kind"], "delete");
        assert_eq!(delete_json["commit"], false);
    }

    #[test]
    fn legacy_profile_operation_defaults_to_completion() {
        let retag: ProfileOperation = serde_json::from_value(json!({
            "kind": "retag",
            "changes": []
        }))
        .unwrap();
        assert!(matches!(
            retag,
            ProfileOperation::Retag {
                apply_after: true,
                ..
            }
        ));

        let delete: ProfileOperation = serde_json::from_value(json!({
            "kind": "delete",
            "targets": []
        }))
        .unwrap();
        assert!(matches!(
            delete,
            ProfileOperation::Delete { commit: true, .. }
        ));
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

    #[test]
    fn effective_fingerprint_ignores_launcher_text() {
        let first = json!({
            "name": "browser-a",
            "notes": "first note",
            "navigator": { "user_agent": "test-agent" }
        })
        .as_object()
        .unwrap()
        .clone();
        let second = json!({
            "name": "browser-b",
            "notes": "second note",
            "navigator": { "user_agent": "test-agent" }
        })
        .as_object()
        .unwrap()
        .clone();

        assert_eq!(
            effective_fingerprint(&first),
            effective_fingerprint(&second)
        );
    }

    #[test]
    fn effective_fingerprint_ignores_disabled_noise() {
        let without_noise = json!({ "navigator": { "user_agent": "test-agent" } })
            .as_object()
            .unwrap()
            .clone();
        let disabled_noise = json!({
            "navigator": { "user_agent": "test-agent" },
            "noise": {
                "canvas": { "enabled": false, "seed": 123 },
                "webgl": { "enabled": false, "seed": 456, "intensity": 0.0005 }
            }
        })
        .as_object()
        .unwrap()
        .clone();

        assert_eq!(
            effective_fingerprint(&without_noise),
            effective_fingerprint(&disabled_noise)
        );
    }

    #[test]
    fn effective_fingerprint_preserves_enabled_noise_seed() {
        let first = json!({
            "navigator": { "user_agent": "test-agent" },
            "noise": { "canvas": { "enabled": true, "seed": 123 } }
        })
        .as_object()
        .unwrap()
        .clone();
        let second = json!({
            "navigator": { "user_agent": "test-agent" },
            "noise": { "canvas": { "enabled": true, "seed": 456 } }
        })
        .as_object()
        .unwrap()
        .clone();

        assert_ne!(
            effective_fingerprint(&first),
            effective_fingerprint(&second)
        );
    }

    #[test]
    fn newly_enabled_legacy_noise_gets_a_seed_without_reseeding_active_noise() {
        let existing = json!({
            "noise": {
                "canvas": { "enabled": false, "seed": 0 },
                "webgl": { "enabled": true, "seed": 0 }
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let mut incoming = json!({
            "noise": {
                "canvas": { "enabled": true, "seed": 0 },
                "webgl": { "enabled": true, "seed": 0 }
            }
        })
        .as_object()
        .unwrap()
        .clone();

        fill_newly_enabled_noise_seeds(&mut incoming, &existing, "profile-id");

        let noise = incoming
            .get("noise")
            .and_then(|value| value.as_object())
            .unwrap();
        let canvas_seed = noise
            .get("canvas")
            .and_then(|value| value.get("seed"))
            .and_then(|value| value.as_u64());
        let webgl_seed = noise
            .get("webgl")
            .and_then(|value| value.get("seed"))
            .and_then(|value| value.as_u64());
        assert!(canvas_seed.is_some());
        assert_ne!(canvas_seed, Some(0));
        assert_eq!(webgl_seed, Some(0));
    }
}
