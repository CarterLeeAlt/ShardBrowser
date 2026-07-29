// Persistent Windows storage layout. The base is derived directly from the
// launcher executable directory, so every entry stays portable:
//   <launcher-dir>/shardx-launcher/
//     profiles/                   ← fingerprint profile JSON files
//     proxies.json                ← saved proxy list
//     user-data/<profile-id>/     ← per-profile user-data-dir for ShardX
//     settings.json               ← global app settings

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn config_root() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("launcher executable path unavailable")?;
    let base = executable
        .parent()
        .context("launcher executable directory unavailable")?;
    let root = base.join("shardx-launcher");
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

pub fn profiles_dir() -> Result<PathBuf> {
    let p = config_root()?.join("profiles");
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

pub fn fingerprints_dir() -> Result<PathBuf> {
    let p = config_root()?.join("fingerprints");
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

pub fn exports_dir() -> Result<PathBuf> {
    let p = config_root()?.join("exports");
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

/// Cached Widevine CDM, seeded from a host Chrome install (or
/// downloaded from the project's git LFS bucket for end users).  When
/// present, every freshly-created profile's user-data-dir gets a
/// pre-warmed `WidevineCdm/` copy so the browser doesn't sit waiting
/// on the component updater the first time a DRM page (Netflix /
/// Spotify / etc.) loads.
pub fn widevine_cache_dir() -> Result<PathBuf> {
    Ok(config_root()?.join("widevine-cdm"))
}

pub fn user_data_root() -> Result<PathBuf> {
    let p = config_root()?.join("user-data");
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

pub fn proxies_path() -> Result<PathBuf> {
    Ok(config_root()?.join("proxies.json"))
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(config_root()?.join("settings.json"))
}

pub fn display_order_path() -> Result<PathBuf> {
    Ok(config_root()?.join("display-order.json"))
}

pub fn backup_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("persistent file has an invalid name")?;
    Ok(path.with_file_name(format!("{name}.bak")))
}

/// Persist a critical launcher file without exposing a truncated destination.
/// A last-known-good sibling `.bak` is retained for crash recovery.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        let backup = backup_path(path)?;
        let current = std::fs::read(path)
            .with_context(|| format!("read current persistent file {}", path.display()))?;
        publish_bytes(&backup, &current)
            .with_context(|| format!("update backup {}", backup.display()))?;
    }
    publish_bytes(path, bytes)
}

/// Parse a critical JSON file, recovering from its last-known-good backup when
/// the primary was truncated by a crash or power loss. Recovery also restores
/// the primary without overwriting the good backup with corrupted bytes.
pub fn load_json_with_backup<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let primary = std::fs::read(path)
        .with_context(|| format!("read persistent file {}", path.display()))?;
    match serde_json::from_slice(&primary) {
        Ok(value) => Ok(value),
        Err(primary_error) => {
            let backup = backup_path(path)?;
            let backup_bytes = std::fs::read(&backup).with_context(|| {
                format!(
                    "parse {} failed ({primary_error}) and recovery backup {} is unavailable",
                    path.display(),
                    backup.display()
                )
            })?;
            let value = serde_json::from_slice(&backup_bytes).with_context(|| {
                format!(
                    "parse {} failed ({primary_error}) and recovery backup {} is also invalid",
                    path.display(),
                    backup.display()
                )
            })?;
            publish_bytes(path, &backup_bytes)
                .with_context(|| format!("restore {} from backup", path.display()))?;
            eprintln!(
                "[launcher] restored corrupted persistent file {} from {}",
                path.display(),
                backup.display()
            );
            Ok(value)
        }
    }
}

fn publish_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("persistent file has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("persistent file has an invalid name")?;
    let temporary = parent.join(format!(
        ".{name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        output.write_all(bytes)?;
        output.sync_all()?;
        drop(output);
        replace_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source_wide: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "replace {} with {}",
                destination.display(),
                source.display()
            )
        });
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
pub fn available_space(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            path_wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("query available space for {}", path.display()));
    }
    Ok(available)
}

#[cfg(not(windows))]
pub fn available_space(_path: &Path) -> Result<u64> {
    Ok(u64::MAX)
}
