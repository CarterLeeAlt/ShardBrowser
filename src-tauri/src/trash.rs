//! Seven-day application recycle bin. User-data is moved intact on the same
//! volume; the shared profile journal makes multi-profile moves recoverable.
use crate::{extensions, process, profile, proxy, store};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Clone, Serialize, Deserialize)]
pub struct TrashEntry {
    pub id: String,
    pub profile_id: String,
    pub name: String,
    pub folder: String,
    pub deleted_at: u64,
    pub expires_at: u64,
    pub size_bytes: u64,
    pub extensions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy_snapshot: Option<serde_json::Value>,
}

fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

fn item_dir(id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("Invalid recycle-bin item ID")?;
    Ok(store::trash_dir()?.join(id))
}

fn tree_size(path: &Path, depth: usize) -> Result<u64> {
    ensure!(depth < 128, "Browser data directory nesting is too deep");
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !crate::profile_backup::is_link_or_reparse_point(&metadata),
        "Browser data contains a link: {}",
        path.display()
    );
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        total = total
            .checked_add(tree_size(&entry?.path(), depth + 1)?)
            .context("Browser data size overflow")?;
    }
    Ok(total)
}

pub(crate) fn list_locked() -> Result<Vec<TrashEntry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(store::trash_dir()?)? {
        let item = item?;
        let id = item.file_name().to_string_lossy().to_string();
        if id.starts_with('.') {
            continue;
        }
        let metadata = item.path().join("entry.json");
        if !metadata.is_file() {
            continue;
        } // unpublished preparation; no live data moved yet
        let entry: TrashEntry = store::load_json_with_backup(&metadata)?;
        ensure!(entry.id == id, "Recycle-bin metadata ID mismatch");
        item_dir(&id)?;
        profile::validate_profile_id(&entry.profile_id)?;
        entries.push(entry);
    }
    entries.sort_by(|a, b| {
        b.deleted_at
            .cmp(&a.deleted_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(entries)
}

pub fn list() -> Result<Vec<TrashEntry>> {
    let _guard = process::lock_profile_resources()?;
    profile::recover_interrupted_operations_locked()?;
    list_locked()
}

pub fn move_profiles(ids: &[String]) -> Result<usize> {
    let _guard = process::lock_profile_resources()?;
    profile::recover_interrupted_operations_locked()?;
    move_profiles_locked(ids)
}

pub(crate) fn move_profiles_locked(ids: &[String]) -> Result<usize> {
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    ensure!(unique.len() == ids.len(), "Duplicate recycle-bin targets");
    let mut moves = Vec::new();
    let mut prepared = Vec::new();
    let mut cleanup_after = Vec::new();
    let deleted_at = now()?;
    let preparation = (|| -> Result<()> {
        for id in ids {
            profile::ensure_stopped(id)?;
            let stored = profile::load_raw(id)?;
            if !stored.meta.temporary {
                extensions::load_paths(&stored.meta.extensions)?;
            }
            let trash_id = uuid::Uuid::new_v4().to_string();
            let dir = if stored.meta.temporary {
                // Folder deletion may include hidden temporary profiles. They
                // join the same transaction, then are cleaned up at commit.
                store::trash_dir()?.join(format!(".temporary-{trash_id}"))
            } else {
                item_dir(&trash_id)?
            };
            fs::create_dir(&dir)?;
            prepared.push(dir.clone());
            if stored.meta.temporary {
                cleanup_after.push(dir.clone());
            }
            let user_data = store::user_data_root()?.join(id);
            let proxy_snapshot = stored
                .meta
                .proxy_id
                .as_deref()
                .map(proxy::get)
                .transpose()?
                .flatten()
                .map(|p| {
                    proxy::serialize_protected_backup_entry(&p)
                        .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
                })
                .transpose()?;
            let entry = TrashEntry {
                id: trash_id,
                profile_id: id.clone(),
                name: stored
                    .config
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(id)
                    .into(),
                folder: stored.meta.folder.clone(),
                deleted_at,
                expires_at: deleted_at + RETENTION_SECS,
                size_bytes: tree_size(&user_data, 0)?,
                extensions: stored.meta.extensions.clone(),
                proxy_snapshot,
            };
            let pending = dir.join("entry.pending.json");
            store::atomic_write_sensitive(&pending, &serde_json::to_vec_pretty(&entry)?)?;
            let profile_path = store::profiles_dir()?.join(format!("{id}.json"));
            for (from, to) in [
                (profile_path.clone(), dir.join("profile.json")),
                (
                    store::backup_path(&profile_path)?,
                    dir.join("profile.json.bak"),
                ),
                (user_data, dir.join("user-data")),
            ] {
                if from.exists() {
                    moves.push(profile::FileMove { from, to });
                }
            }
            moves.push(profile::FileMove {
                from: pending,
                to: dir.join("entry.json"),
            });
        }
        Ok(())
    })();
    if let Err(error) = preparation {
        for dir in prepared {
            fs::remove_dir_all(dir)?;
        }
        return Err(error);
    }
    if ids.is_empty() {
        return Ok(0);
    }
    profile::transfer_files(moves, ids.to_vec(), cleanup_after, prepared)?;
    crate::notify_store_changed("profiles");
    Ok(ids.len())
}

pub fn restore(id: &str) -> Result<String> {
    let _guard = process::lock_profile_resources()?;
    profile::recover_interrupted_operations_locked()?;
    let dir = item_dir(id)?;
    let entry: TrashEntry = store::load_json_with_backup(&dir.join("entry.json"))?;
    ensure!(entry.id == id, "Recycle-bin metadata ID mismatch");
    ensure!(now()? < entry.expires_at, "Recycle-bin item has expired");
    let mut stored: profile::StoredProfile =
        store::load_json_with_backup(&dir.join("profile.json"))?;
    ensure!(
        stored.meta.id == entry.profile_id,
        "Recycle-bin profile ID mismatch"
    );
    profile::validate_profile_id(&stored.meta.id)?;
    profile::ensure_stopped(&stored.meta.id)?;
    extensions::load_paths(&stored.meta.extensions)?;
    let destination = store::profiles_dir()?.join(format!("{}.json", stored.meta.id));
    let user_data = store::user_data_root()?.join(&stored.meta.id);
    ensure!(
        !destination.exists() && !store::backup_path(&destination)?.exists() && !user_data.exists(),
        "Restore conflicts with an existing profile or user-data directory"
    );
    let snapshot = entry
        .proxy_snapshot
        .map(|value| proxy::deserialize_backup_entry(&serde_json::to_vec(&value)?, 3))
        .transpose()?;
    proxy::with_restored_proxy_bindings(vec![snapshot], |bindings| {
        if let Some(proxy_id) = &bindings[0] {
            if stored.meta.proxy_id.as_ref() != Some(proxy_id) {
                stored.meta.session_network_identity = None;
            }
            stored.meta.proxy_id = Some(proxy_id.clone());
        }
        let pending = dir.join("restore-profile.json");
        store::atomic_write_sensitive(&pending, &serde_json::to_vec_pretty(&stored)?)?;
        let mut moves = Vec::new();
        if dir.join("user-data").exists() {
            moves.push(profile::FileMove {
                from: dir.join("user-data"),
                to: user_data,
            });
        }
        moves.push(profile::FileMove {
            from: pending,
            to: destination,
        });
        moves.push(profile::FileMove {
            from: dir.join("entry.json"),
            to: dir.join("entry.restored.json"),
        });
        profile::transfer_files(
            moves,
            vec![stored.meta.id.clone()],
            vec![dir.clone()],
            Vec::new(),
        )
    })?;
    crate::notify_store_changed("profiles");
    crate::notify_store_changed("proxies");
    Ok(stored.meta.id)
}

fn purge_locked(ids: &[String]) -> Result<usize> {
    ensure!(
        ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len(),
        "Duplicate recycle-bin targets"
    );
    let entries = list_locked()?;
    let mut selected = Vec::new();
    for id in ids {
        let entry = entries
            .iter()
            .find(|entry| &entry.id == id)
            .context("Recycle-bin item no longer exists")?;
        profile::ensure_stopped(&entry.profile_id)?;
        selected.push((entry, item_dir(id)?));
    }
    // Publish the deletion decision by renaming each item before removing files.
    // A crash leaves only .purge-* directories, which the cleanup pass finishes.
    for (entry, path) in selected {
        let pending = store::trash_dir()?.join(format!(".purge-{}", entry.id));
        fs::rename(path, &pending)?;
        if !store::profiles_dir()?
            .join(format!("{}.json", entry.profile_id))
            .exists()
        {
            crate::taskbar_icon::remove_profile_launchers(&entry.profile_id);
        }
        fs::remove_dir_all(pending)?;
    }
    crate::notify_store_changed("trash");
    Ok(ids.len())
}

pub fn purge(ids: &[String]) -> Result<usize> {
    let _guard = process::lock_profile_resources()?;
    profile::recover_interrupted_operations_locked()?;
    purge_locked(ids)
}

pub fn empty() -> Result<usize> {
    let _guard = process::lock_profile_resources()?;
    profile::recover_interrupted_operations_locked()?;
    purge_locked(
        &list_locked()?
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
    )
}

pub fn purge_expired() -> Result<usize> {
    let _guard = process::lock_profile_resources()?;
    process::ensure_lease_recovery_complete()?;
    profile::recover_interrupted_operations_locked()?;
    let timestamp = now()?;
    let ids = list_locked()?
        .into_iter()
        .filter(|entry| entry.expires_at <= timestamp)
        .map(|entry| entry.id)
        .collect::<Vec<_>>();
    let count = purge_locked(&ids)?;
    for item in fs::read_dir(store::trash_dir()?)? {
        let item = item?;
        if item.file_name().to_string_lossy().starts_with(".purge-") {
            fs::remove_dir_all(item.path())?;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_keeps_identity_data_and_extension_references_until_expiry() {
        let test = store::TestStore::new();
        let extension = extensions::tests::fixture(&test.root.join("source"));
        let mut stored: profile::StoredProfile = serde_json::from_value(json!({
            "_meta": {"id": "kept-identity", "extensions": [extension.id], "folder": "Original"},
            "name": "TestProfile", "navigator": {"platform_version": "14.7"},
            "noise": {"canvas": {"enabled": true, "seed": 42}}
        }))
        .unwrap();
        profile::save_raw(&mut stored).unwrap();
        let cloned = profile::clone_profile(&stored.meta.id).unwrap();
        assert_eq!(
            profile::load_raw(&cloned.id).unwrap().meta.extensions,
            stored.meta.extensions
        );
        profile::delete(&cloned.id).unwrap();
        let live = store::profiles_dir().unwrap().join("kept-identity.json");
        let profile_bytes = fs::read(&live).unwrap();
        let data = profile::user_data_dir("kept-identity").unwrap();
        fs::create_dir_all(data.join("Default/Local Extension Settings")).unwrap();
        fs::write(
            data.join("Default/Local Extension Settings/state"),
            b"extension-user-state",
        )
        .unwrap();
        fs::write(data.join("session"), b"complete-browser-state").unwrap();
        assert_eq!(move_profiles(&[stored.meta.id.clone()]).unwrap(), 1);
        assert!(!live.exists());
        assert!(!data.exists());
        let entry = list().unwrap().remove(0);
        assert_eq!(entry.expires_at - entry.deleted_at, RETENTION_SECS);
        assert_eq!(extensions::list().unwrap()[0].trash_count, 1);
        assert!(extensions::delete(&extension.id).is_err());
        fs::create_dir_all(&data).unwrap();
        assert!(
            restore(&entry.id).is_err(),
            "existing data must never be overwritten"
        );
        fs::remove_dir(&data).unwrap();
        assert_eq!(restore(&entry.id).unwrap(), "kept-identity");
        assert_eq!(fs::read(&live).unwrap(), profile_bytes);
        assert_eq!(
            fs::read(data.join("session")).unwrap(),
            b"complete-browser-state"
        );
        assert_eq!(
            fs::read(data.join("Default/Local Extension Settings/state")).unwrap(),
            b"extension-user-state"
        );
        assert!(list().unwrap().is_empty());
        assert_eq!(
            extensions::list().unwrap()[0].profile_names,
            ["TestProfile"]
        );
        let reservation = process::Tracker::shared()
            .reserve_launch("kept-identity")
            .unwrap();
        assert!(move_profiles(&[stored.meta.id.clone()]).is_err());
        assert_eq!(fs::read(&live).unwrap(), profile_bytes);
        drop(reservation);
        let mut temporary: profile::StoredProfile = serde_json::from_value(json!({
            "_meta": {"id": "temporary-folder-member", "temporary": true, "folder": "Original"}, "name": "Temporary"
        })).unwrap();
        profile::save_raw(&mut temporary).unwrap();
        assert_eq!(profile::delete_folder("Original", true).unwrap(), 2);
        assert!(profile::load_raw("temporary-folder-member").is_err());
        assert_eq!(list().unwrap().len(), 1);
        let mut entry = list().unwrap().remove(0);
        entry.expires_at = now().unwrap() - 1;
        store::atomic_write_sensitive(
            &item_dir(&entry.id).unwrap().join("entry.json"),
            &serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();
        assert!(restore(&entry.id).is_err());
        assert_eq!(purge_expired().unwrap(), 1);
        assert!(list().unwrap().is_empty());
        extensions::delete(&extension.id).unwrap();
    }
}
