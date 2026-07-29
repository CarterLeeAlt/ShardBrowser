use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;

fn backup_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("persistent file has an invalid name")?;
    Ok(path.with_file_name(format!("{name}.bak")))
}

fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("persistent file has no parent")?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("persistent file has an invalid name")?;
    let temporary = parent.join(format!(".{name}.{:016x}.tmp", rand::random::<u64>()));
    let result = (|| -> Result<()> {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        output.write_all(bytes)?;
        output.sync_all()?;
        drop(output);
        replace_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        let backup = backup_path(path)?;
        let current = fs::read(path)?;
        publish(&backup, &current)?;
    }
    publish(path, bytes)
}

pub(crate) fn read_json_with_backup<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let primary = fs::read(path)?;
    match serde_json::from_slice(&primary) {
        Ok(value) => Ok(value),
        Err(primary_error) => {
            let backup = backup_path(path)?;
            let backup_bytes = fs::read(&backup).with_context(|| {
                format!(
                    "parse {:?} failed ({primary_error}) and backup {:?} is unavailable",
                    path, backup
                )
            })?;
            let value = serde_json::from_slice(&backup_bytes)?;
            publish(path, &backup_bytes)?;
            eprintln!("[shardx] restored corrupted JSON {path:?} from {backup:?}");
            Ok(value)
        }
    }
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new_name: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

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
        return Err(std::io::Error::last_os_error()).context("replace persistent file");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}
