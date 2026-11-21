mod crypto;

use crate::{operations, process, profile, profile_backup as backup, proxy, store};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const EXTENSION: &str = "shardx-workspace";
const METADATA_LIMIT: u64 = 16 * 1024 * 1024;
const ENTRY_LIMIT: usize = 500_000;
const DATA_LIMIT: u64 = 128 * 1024 * 1024 * 1024;
const DIRECTORIES: &[&str] = &["profiles", "user-data", "extensions"];
const TOP_FILES: &[&str] = &["proxies.json", "display-order.json"];

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary {
    pub id: uuid::Uuid,
    profiles: usize,
    proxies: usize,
    folders: Vec<String>,
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let result = if self.0.is_dir() {
            fs::remove_dir_all(&self.0)
        } else {
            fs::remove_file(&self.0)
        };
        if let Err(error) = result {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("[workspace backup] could not clean temporary item: {error}");
            }
        }
    }
}

fn path_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !backup::is_link_or_reparse_point(&metadata),
                "Workspace backup cannot use a link or reparse point: {}",
                path.display()
            );
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn directory_exists(path: &Path) -> Result<bool> {
    let Some(metadata) = path_metadata(path)? else {
        return Ok(false);
    };
    ensure!(
        metadata.is_dir(),
        "Workspace backup expected a directory: {}",
        path.display()
    );
    Ok(true)
}

fn read_metadata(reader: impl Read, size: u64) -> Result<Vec<u8>> {
    ensure!(
        size <= METADATA_LIMIT,
        "Workspace backup metadata is too large"
    );
    let mut bytes = Vec::new();
    reader.take(METADATA_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == size,
        "Workspace backup metadata size mismatch"
    );
    Ok(bytes)
}

fn metadata_file(path: &Path) -> Result<Vec<u8>> {
    let meta = path_metadata(path)?.context("Missing workspace metadata")?;
    ensure!(meta.is_file(), "Invalid workspace metadata file");
    read_metadata(File::open(path)?, meta.len())
}

fn validate_folders(folders: &[String]) -> Result<()> {
    ensure!(
        folders.len() <= 100_000
            && folders
                .iter()
                .all(|folder| !folder.is_empty() && folder.len() <= 4096),
        "Invalid workspace folder list"
    );
    Ok(())
}

fn safe_name(name: &str) -> Result<Vec<&str>> {
    backup::validate_archive_entry_name(name)?;
    let pieces: Vec<_> = name.split('/').collect();
    ensure!(
        pieces.len() <= 131
            && pieces.iter().all(|name| {
                let stem = name
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .trim_end()
                    .to_ascii_uppercase();
                !name.is_empty()
                    && !name.ends_with(['.', ' '])
                    && !name
                        .chars()
                        .any(|ch| ch.is_control() || "<>\"|?*".contains(ch))
                    && !matches!(
                        stem.as_str(),
                        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
                    )
                    && !["COM", "LPT"].iter().any(|prefix| {
                        stem.strip_prefix(prefix).is_some_and(|suffix| {
                            matches!(
                                suffix,
                                "0" | "1"
                                    | "2"
                                    | "3"
                                    | "4"
                                    | "5"
                                    | "6"
                                    | "7"
                                    | "8"
                                    | "9"
                                    | "¹"
                                    | "²"
                                    | "³"
                            )
                        })
                    })
            }),
        "Invalid workspace archive path"
    );
    Ok(pieces)
}

fn validate_path(name: &str, directory: bool) -> Result<Vec<&str>> {
    let parts = safe_name(name)?;
    let valid = match parts.as_slice() {
        [name] => !directory && TOP_FILES.contains(name),
        ["profiles" | "fingerprints", file] => !directory && file.ends_with(".json"),
        ["user-data", id, rest @ ..] => {
            profile::validate_profile_id(id).is_ok() && (directory || !rest.is_empty())
        }
        ["extensions", id, rest @ ..] => {
            crate::extensions::validate_id(id).is_ok() && (directory || !rest.is_empty())
        }
        _ => false,
    };
    ensure!(valid, "Unsupported workspace archive entry");
    Ok(parts)
}

fn append<W: Write>(
    archive: &mut tar::Builder<W>,
    name: &str,
    directory: bool,
    size: u64,
    input: impl Read,
) -> Result<()> {
    if name != "manifest.json" {
        validate_path(name, directory)?;
    }
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(if directory {
        tar::EntryType::Directory
    } else {
        tar::EntryType::Regular
    });
    header.set_mode(if directory { 0o700 } else { 0o600 });
    header.set_size(size);
    archive.append_data(&mut header, name, input)?;
    Ok(())
}

fn append_file<W: Write>(
    archive: &mut tar::Builder<W>,
    source: &Path,
    name: &str,
    size: &mut u64,
) -> Result<()> {
    let meta = path_metadata(source)?.context("Workspace file disappeared during export")?;
    ensure!(meta.is_file(), "Workspace export expected a regular file");
    *size = size
        .checked_add(meta.len())
        .context("Workspace data size overflow")?;
    ensure!(
        *size <= DATA_LIMIT,
        "Workspace backup exceeds the size limit"
    );
    let mut file = File::open(source)?.take(meta.len());
    append(archive, name, false, meta.len(), &mut file)?;
    ensure!(
        file.limit() == 0 && file.into_inner().metadata()?.len() == meta.len(),
        "Workspace file size changed while reading"
    );
    Ok(())
}

fn append_extensions<W: Write>(
    archive: &mut tar::Builder<W>,
    root: &Path,
    directory: &Path,
    size: &mut u64,
    depth: usize,
) -> Result<()> {
    ensure!(
        depth <= 128,
        "Workspace extension nesting exceeds the limit"
    );
    for item in fs::read_dir(directory)? {
        let source = item?.path();
        let meta = path_metadata(&source)?.context("Extension disappeared during export")?;
        if directory == root.join("extensions")
            && source
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| crate::extensions::validate_id(name).is_err())
        {
            continue;
        }
        let name = backup::archive_relative_path(source.strip_prefix(root)?)?;
        if meta.is_dir() {
            append(archive, &name, true, 0, io::empty())?;
            append_extensions(archive, root, &source, size, depth + 1)?;
        } else {
            append_file(archive, &source, &name, size)?;
        }
    }
    Ok(())
}

fn idle() -> Result<()> {
    process::ensure_lease_recovery_complete()?;
    operations::ensure_runtime_available()?;
    ensure!(
        process::Tracker::shared().active_profile_ids().is_empty(),
        "Stop all browsers before backing up or restoring the workspace"
    );
    ensure!(
        operations::busy_profiles().is_empty(),
        "Another profile backup is in progress"
    );
    profile::recover_interrupted_operations_locked()
}

fn checked_file(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case(EXTENSION)),
        "Select a .shardx-workspace file"
    );
    Ok(())
}

pub(crate) fn export(path: PathBuf, folders: Vec<String>) -> Result<Summary> {
    let _activity = operations::activity();
    checked_file(&path)?;
    validate_folders(&folders)?;
    let root = store::config_root()?;
    let parent = path
        .parent()
        .context("Invalid backup destination")?
        .canonicalize()?;
    let path = parent.join(path.file_name().context("Missing backup filename")?);
    ensure!(
        !parent.starts_with(root.canonicalize()?),
        "Select a backup destination outside the workspace data folder"
    );
    ensure!(
        path_metadata(&path)?.is_none(),
        "A file already exists at the backup destination"
    );
    let temporary = parent.join(format!(
        ".shardx-workspace-{}.partial.{EXTENSION}",
        uuid::Uuid::new_v4()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let _scratch = Scratch(temporary.clone());
    let mut archive = tar::Builder::new(crypto::Encrypt::new(BufWriter::new(file))?);
    let mut data_size = 0;
    let (summary, ids, _reservation) = {
        let _guard = process::lock_profile_resources()?;
        idle()?;
        let mut profiles = Vec::new();
        let profiles_dir = root.join("profiles");
        if directory_exists(&profiles_dir)? {
            for entry in fs::read_dir(profiles_dir)? {
                let file = entry?.path();
                if file.extension().is_none_or(|extension| extension != "json") {
                    continue;
                }
                let bytes = metadata_file(&file)?;
                let stored: profile::StoredProfile = serde_json::from_slice(&bytes)?;
                profile::validate_profile_id(&stored.meta.id)?;
                ensure!(
                    file.file_stem().and_then(|name| name.to_str()) == Some(&stored.meta.id),
                    "Workspace profile filename and ID differ"
                );
                if !stored.meta.temporary {
                    profiles.push((stored.meta.id, bytes));
                }
            }
        }
        let ids: Vec<_> = profiles.iter().map(|(id, _)| id.clone()).collect();
        let default_order: Vec<_> = profile::list_all()?
            .into_iter()
            .map(|item| item.id)
            .collect();
        ensure!(
            default_order.len() == ids.len(),
            "Workspace profile list changed during export"
        );
        let proxies = root.join("proxies.json");
        let proxy_bytes = path_metadata(&proxies)?
            .map(|_| metadata_file(&proxies))
            .transpose()?;
        let proxy_ids = proxy_bytes
            .as_deref()
            .map(proxy::validate_workspace_proxy_data)
            .transpose()?
            .unwrap_or_default();
        ensure!(
            !ids.is_empty() || !proxy_ids.is_empty(),
            "There are no browser profiles or proxies to back up"
        );
        let summary = Summary {
            id: uuid::Uuid::new_v4(),
            profiles: ids.len(),
            proxies: proxy_ids.len(),
            folders,
        };
        let bytes = serde_json::to_vec(&summary)?;
        append(
            &mut archive,
            "manifest.json",
            false,
            bytes.len() as u64,
            bytes.as_slice(),
        )?;
        let reservation = operations::reserve_profiles_locked(&ids)?;
        for (id, bytes) in profiles {
            append(
                &mut archive,
                &format!("profiles/{id}.json"),
                false,
                bytes.len() as u64,
                bytes.as_slice(),
            )?;
        }
        if let Some(bytes) = proxy_bytes {
            append(
                &mut archive,
                "proxies.json",
                false,
                bytes.len() as u64,
                bytes.as_slice(),
            )?;
        }
        path_metadata(&root.join("display-order.json"))?;
        let order = crate::display_order::workspace_snapshot(&default_order, &proxy_ids)?;
        append(
            &mut archive,
            "display-order.json",
            false,
            order.len() as u64,
            order.as_slice(),
        )?;
        let fingerprints = root.join("fingerprints");
        if directory_exists(&fingerprints)? {
            for entry in fs::read_dir(fingerprints)? {
                let file = entry?.path();
                if file.extension().is_some_and(|ext| ext == "json") {
                    let name = backup::archive_relative_path(file.strip_prefix(&root)?)?;
                    append_file(&mut archive, &file, &name, &mut data_size)?;
                }
            }
        }
        let extensions = root.join("extensions");
        if directory_exists(&extensions)? {
            append_extensions(&mut archive, &root, &extensions, &mut data_size, 0)?;
        }
        (summary, ids, reservation)
    };
    let users = root.join("user-data");
    if directory_exists(&users)? {
        for id in ids {
            let source = users.join(&id);
            if !directory_exists(&source)? {
                continue;
            }
            let prefix = format!("user-data/{id}");
            append(&mut archive, &prefix, true, 0, io::empty())?;
            for item in backup::user_data_sources(&source)? {
                let name = format!(
                    "{prefix}/{}",
                    backup::archive_relative_path(&item.relative)?
                );
                match item.kind {
                    backup::BackupEntryKind::Directory => {
                        append(&mut archive, &name, true, 0, io::empty())?
                    }
                    backup::BackupEntryKind::File => {
                        append_file(&mut archive, &item.source, &name, &mut data_size)?
                    }
                }
            }
        }
    }
    let writer = archive.into_inner()?.finish()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    drop(_reservation);
    inspect_archive(&temporary, None, Some(summary.id))
        .context("Verify exported workspace backup")?;
    publish(&temporary, &path)?;
    Ok(summary)
}

fn publish(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>()
    };
    // Do not let a file created after the save dialog be overwritten.
    if unsafe {
        MoveFileExW(
            wide(source).as_ptr(),
            wide(destination).as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error())
            .context("Cannot publish the backup without overwriting the destination");
    }
    Ok(())
}

fn inspect_archive(
    path: &Path,
    stage: Option<&Path>,
    expected: Option<uuid::Uuid>,
) -> Result<Summary> {
    checked_file(path)?;
    let meta = path_metadata(path)?.context("Workspace backup file does not exist")?;
    ensure!(
        meta.is_file() && meta.len() <= DATA_LIMIT + 4 * 1024 * 1024 * 1024,
        "Invalid workspace backup size"
    );
    let reader = crypto::Decrypt::new(BufReader::new(File::open(path)?))?;
    let mut archive = tar::Archive::new(reader);
    let mut seen = HashSet::new();
    let mut profiles = HashMap::new();
    let mut users = HashSet::new();
    let mut proxy_ids = HashSet::new();
    let mut bytes = 0u64;
    let mut entries = archive.entries()?;
    let mut first = entries
        .next()
        .context("Missing workspace backup manifest")??;
    ensure!(
        first.path_bytes().as_ref() == b"manifest.json" && first.header().entry_type().is_file(),
        "Invalid workspace backup manifest"
    );
    let length = first.size();
    let summary: Summary = serde_json::from_slice(&read_metadata(&mut first, length)?)?;
    drop(first);
    ensure!(
        expected.is_none_or(|id| id == summary.id),
        "The backup changed after confirmation; select it again"
    );
    validate_folders(&summary.folders)?;
    for entry in entries {
        let mut entry = entry?;
        let name = std::str::from_utf8(&entry.path_bytes())?.to_owned();
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_dir() || kind.is_file(),
            "Unsupported workspace archive entry type"
        );
        let parts = validate_path(&name, kind.is_dir())?;
        ensure!(
            seen.insert(name.to_lowercase()) && seen.len() <= ENTRY_LIMIT,
            "Duplicate or excessive workspace archive entries"
        );
        let length = entry.size();
        bytes = bytes
            .checked_add(length)
            .context("Workspace size overflow")?;
        ensure!(
            bytes <= DATA_LIMIT,
            "Workspace backup exceeds the size limit"
        );
        let target = stage.map(|root| root.join(&name));
        if let ["user-data", id, ..] = parts.as_slice() {
            users.insert((*id).to_owned());
        }
        if kind.is_dir() {
            ensure!(length == 0, "Invalid workspace archive directory");
            if let Some(target) = target {
                fs::create_dir_all(target)?;
            }
            continue;
        }
        let mut output = target
            .as_ref()
            .map(|path| -> Result<File> {
                fs::create_dir_all(path.parent().context("Missing staging parent")?)?;
                Ok(OpenOptions::new().write(true).create_new(true).open(path)?)
            })
            .transpose()?;
        if parts[0] == "profiles"
            || TOP_FILES.contains(&name.as_str())
            || parts[0] == "fingerprints"
        {
            let content = read_metadata(&mut entry, length)?;
            match parts.as_slice() {
                ["profiles", file] => {
                    let stored: profile::StoredProfile = serde_json::from_slice(&content)?;
                    ensure!(
                        !stored.meta.temporary
                            && file.strip_suffix(".json") == Some(&stored.meta.id),
                        "Invalid workspace profile ID"
                    );
                    profile::validate_profile_id(&stored.meta.id)?;
                    profiles.insert(stored.meta.id, stored.meta.proxy_id);
                }
                ["proxies.json"] => {
                    for id in proxy::validate_workspace_proxy_data(&content)? {
                        ensure!(proxy_ids.insert(id), "Duplicate workspace proxy ID");
                    }
                }
                _ => {
                    let _: serde_json::Value = serde_json::from_slice(&content)?;
                }
            }
            if let Some(file) = &mut output {
                file.write_all(&content)?;
            }
        } else {
            let copied = if let Some(file) = &mut output {
                io::copy(&mut entry, file)?
            } else {
                io::copy(&mut entry, &mut io::sink())?
            };
            ensure!(copied == length, "Truncated workspace archive entry");
        }
    }
    // The tar reader stops at its first zero block. Authenticate the rest,
    // including the encrypted terminator, before accepting the archive.
    let mut reader = archive.into_inner();
    let mut tail = [0; 1024];
    let mut total = 0;
    loop {
        let n = reader.read(&mut tail)?;
        if n == 0 {
            break;
        }
        total += n;
        ensure!(
            total <= 1024 && tail[..n].iter().all(|value| *value == 0),
            "Unexpected workspace archive trailer"
        );
    }
    ensure!(
        profiles.len() == summary.profiles
            && proxy_ids.len() == summary.proxies
            && (!profiles.is_empty() || !proxy_ids.is_empty()),
        "Workspace inventory does not match the manifest"
    );
    ensure!(
        users.iter().all(|id| profiles.contains_key(id))
            && profiles.values().flatten().all(|id| proxy_ids.contains(id)),
        "Workspace contains unresolved browser data or proxy bindings"
    );
    Ok(summary)
}

fn empty_workspace(root: &Path) -> Result<()> {
    directory_exists(root)?;
    for name in DIRECTORIES.iter().copied().chain(["trash"]) {
        let path = root.join(name);
        ensure!(!directory_exists(&path)? || fs::read_dir(path)?.next().is_none(), "Restore requires an empty workspace (browser profiles, browser data, extensions and recycle bin)");
    }
    for name in TOP_FILES.iter().copied().chain(["proxies-history.json"]) {
        let path = root.join(name);
        ensure!(
            path_metadata(&path)?.is_none()
                && path_metadata(&store::backup_path(&path)?)?.is_none(),
            "Restore requires an empty workspace (saved proxies or order already exist)"
        );
    }
    Ok(())
}

pub(crate) fn cleanup_interrupted_stage() -> Result<()> {
    let _guard = process::lock_profile_resources()?;
    if store::profile_operation_journal_path()?.exists()
        || store::backup_path(&store::profile_operation_journal_path()?)?.exists()
    {
        return Ok(());
    }
    for entry in fs::read_dir(store::config_root()?)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .and_then(|s| s.strip_prefix("workspace-restore-"))
            .and_then(|s| s.strip_suffix(".tmp"))
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            && directory_exists(&entry.path())?
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn create_private_stage(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let wide = |text: &std::ffi::OsStr| text.encode_wide().chain([0]).collect::<Vec<_>>();
    let sddl = wide("D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)".as_ref());
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error())
            .context("Cannot prepare private restore permissions");
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let path = wide(path.as_os_str());
    let result = if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };
    unsafe { LocalFree(descriptor) };
    result.context("Cannot create private restore staging")
}

pub(crate) fn inspect(path: PathBuf) -> Result<Summary> {
    let _activity = operations::activity();
    {
        let _guard = process::lock_profile_resources()?;
        idle()?;
        empty_workspace(&store::config_root()?)?;
    }
    inspect_archive(&path, None, None)
}

pub(crate) fn restore(path: PathBuf, id: uuid::Uuid) -> Result<Summary> {
    let _activity = operations::activity();
    let root = store::config_root()?;
    {
        let _guard = process::lock_profile_resources()?;
        idle()?;
        empty_workspace(&root)?;
    }
    let stage = root.join(format!("workspace-restore-{}.tmp", uuid::Uuid::new_v4()));
    create_private_stage(&stage)?;
    let scratch = Scratch(stage.clone());
    let summary = inspect_archive(&path, Some(&stage), Some(id))?;
    let _guard = process::lock_profile_resources()?;
    idle()?;
    empty_workspace(&root)?;
    let mut moves = Vec::new();
    let mut add = |from: PathBuf, to: PathBuf| moves.push(profile::FileMove { from, to });
    for name in TOP_FILES {
        let from = stage.join(name);
        if from.exists() {
            add(from, root.join(name));
        }
    }
    // Empty directory placeholders may already exist; transfer their children.
    for name in DIRECTORIES {
        let from = stage.join(name);
        if from.is_dir() {
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                add(entry.path(), root.join(name).join(entry.file_name()));
            }
        }
    }
    let fingerprints = root.join("fingerprints");
    directory_exists(&fingerprints)?;
    if stage.join("fingerprints").is_dir() {
        for entry in fs::read_dir(stage.join("fingerprints"))? {
            let entry = entry?;
            let to = fingerprints.join(entry.file_name());
            if path_metadata(&to)?.is_some() {
                ensure!(
                    metadata_file(&entry.path())? == metadata_file(&to)?,
                    "A different fingerprint with the same ID exists in the destination"
                );
            } else {
                add(entry.path(), to);
            }
        }
    }
    // An interrupted transfer owns the stage via the existing durable journal.
    let result = profile::transfer_files(moves, Vec::new(), vec![stage.clone()], vec![stage]);
    let journal = store::profile_operation_journal_path()?;
    if journal.exists() || store::backup_path(&journal)?.exists() {
        std::mem::forget(scratch);
    }
    result?;
    drop(_guard);
    for kind in ["profiles", "proxies", "extensions", "fingerprints"] {
        crate::notify_store_changed(kind);
    }
    Ok(summary)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (store::TestStore, PathBuf) {
        let test = store::TestStore::new();
        let root = &test.root;
        let bound = proxy::upsert_dedup(
            serde_json::from_value(json!({
                "id":"", "name":"Bound", "kind":"socks5", "host":"bound.invalid", "port":1080,
                "username":"fixture-login", "password":"fixture-password"
            }))
            .unwrap(),
        )
        .unwrap();
        let unbound = proxy::upsert_dedup(
            serde_json::from_value(json!({
                "id":"", "name":"Unbound", "kind":"http", "host":"unbound.invalid", "port":8080
            }))
            .unwrap(),
        )
        .unwrap();
        let mut profile: profile::StoredProfile = serde_json::from_value(json!({
            "_meta":{"id":"workspace-profile","proxy_id":bound.id,"folder":"Work"},"name":"Workspace"
        })).unwrap();
        profile::save_raw(&mut profile).unwrap();
        let data = root.join("user-data/workspace-profile");
        fs::create_dir_all(data.join("Default/Network")).unwrap();
        fs::write(
            data.join("Default/Network/Cookies"),
            b"fixture-persistent-login".repeat(70000),
        )
        .unwrap();
        fs::create_dir_all(data.join("Default/Cache")).unwrap();
        fs::write(data.join("Default/Cache/rebuildable"), b"cache").unwrap();
        let extension = root.join("extensions/abcdefghijklmnopabcdefghijklmnop");
        fs::create_dir_all(&extension).unwrap();
        fs::write(extension.join("worker.js"), b"// extension fixture").unwrap();
        fs::create_dir_all(root.join("fingerprints")).unwrap();
        fs::write(root.join("fingerprints/library-entry.json"), b"{}").unwrap();
        fs::write(
            root.join("display-order.json"),
            serde_json::to_vec(&json!({
                "version":1,"profiles":["workspace-profile"],"proxies":[unbound.id,bound.id]
            }))
            .unwrap(),
        )
        .unwrap();
        let path = root
            .parent()
            .unwrap()
            .join(format!("{}.shardx-workspace", uuid::Uuid::new_v4()));
        (test, path)
    }

    pub(crate) fn move_live_to_fixture(root: &Path) {
        let prior = root.join("prior-workspace");
        fs::create_dir(&prior).unwrap();
        for name in DIRECTORIES
            .iter()
            .copied()
            .chain(TOP_FILES.iter().copied())
            .chain(["proxies.json.bak", "display-order.json.bak"])
        {
            if root.join(name).exists() {
                fs::rename(root.join(name), prior.join(name)).unwrap();
            }
        }
    }

    #[test]
    fn restores_ids_order_credentials_and_login_files_without_caches() {
        let (test, path) = fixture();
        let _file = Scratch(path.clone());
        let source = profile::load_raw("workspace-profile").unwrap();
        let summary = export(path.clone(), vec!["Empty folder".into()]).unwrap();
        assert_eq!((summary.profiles, summary.proxies), (1, 2));
        assert!(export(path.clone(), Vec::new())
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        assert!(inspect(path.clone())
            .unwrap_err()
            .to_string()
            .contains("empty workspace"));
        assert!(restore(path.clone(), summary.id)
            .unwrap_err()
            .to_string()
            .contains("empty workspace"));
        let bytes = fs::read(&path).unwrap();
        assert!(!bytes
            .windows(b"fixture-persistent-login".len())
            .any(|s| s == b"fixture-persistent-login"));
        move_live_to_fixture(&test.root);
        assert_eq!(inspect(path.clone()).unwrap(), summary);
        assert!(!test.root.join("profiles").exists());
        assert_eq!(restore(path, summary.id).unwrap(), summary);
        assert_eq!(
            profile::load_raw("workspace-profile").unwrap().meta.folder,
            "Work"
        );
        let list = proxy::list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            list.iter()
                .find(|p| Some(&p.id) == source.meta.proxy_id.as_ref())
                .unwrap()
                .password,
            "fixture-password"
        );
        let read_order = |name| {
            serde_json::from_slice::<serde_json::Value>(&fs::read(test.root.join(name)).unwrap())
                .unwrap()
        };
        assert_eq!(
            read_order("display-order.json"),
            read_order("prior-workspace/display-order.json")
        );
        let data = test.root.join("user-data/workspace-profile");
        assert_eq!(
            fs::read(data.join("Default/Network/Cookies")).unwrap(),
            b"fixture-persistent-login".repeat(70000)
        );
        assert!(!data.join("Default/Cache").exists());
        assert_eq!(
            fs::read(
                test.root
                    .join("extensions/abcdefghijklmnopabcdefghijklmnop/worker.js")
            )
            .unwrap(),
            b"// extension fixture"
        );
        assert_eq!(
            fs::read(test.root.join("fingerprints/library-entry.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn captures_default_order_and_rechecks_fingerprint_conflicts_before_commit() {
        let (test, path) = fixture();
        let _file = Scratch(path.clone());
        fs::remove_file(test.root.join("display-order.json")).unwrap();
        let mut second: profile::StoredProfile = serde_json::from_value(json!({
            "_meta":{"id":"second","created_at":"@100"},"name":"Second"
        }))
        .unwrap();
        profile::save_raw(&mut second).unwrap();
        let ids = |profiles: Vec<profile::ProfileMeta>| {
            profiles
                .into_iter()
                .map(|profile| profile.id)
                .collect::<Vec<_>>()
        };
        let wanted = ids(profile::list_all().unwrap());
        let summary = export(path.clone(), Vec::new()).unwrap();
        move_live_to_fixture(&test.root);
        assert_eq!(inspect(path.clone()).unwrap(), summary);
        let fingerprint_file = test.root.join("fingerprints/library-entry.json");
        fs::write(&fingerprint_file, b"{\"name\":\"conflicting\"}").unwrap();
        assert!(restore(path.clone(), summary.id)
            .unwrap_err()
            .to_string()
            .contains("same ID"));
        assert!(!test.root.join("profiles").exists());
        assert!(!test.root.join("proxies.json").exists());
        assert_eq!(
            fs::read(&fingerprint_file).unwrap(),
            b"{\"name\":\"conflicting\"}"
        );
        fs::write(&fingerprint_file, b"{}").unwrap();
        restore(path, summary.id).unwrap();
        assert_eq!(
            ids(crate::display_order::sort_profiles(profile::list_all().unwrap()).unwrap()),
            wanted
        );
    }

    #[test]
    fn rejects_changed_or_tampered_backup_before_publishing() {
        let (test, path) = fixture();
        let _file = Scratch(path.clone());
        let summary = export(path.clone(), Vec::new()).unwrap();
        move_live_to_fixture(&test.root);
        assert!(restore(path.clone(), uuid::Uuid::new_v4())
            .unwrap_err()
            .to_string()
            .contains("changed after confirmation"));
        let bytes = fs::read(&path).unwrap();
        let mut flipped = bytes.clone();
        *flipped.last_mut().unwrap() ^= 1;
        let mut trailing = bytes.clone();
        trailing.push(0);
        for tampered in [flipped, bytes[..bytes.len() - 1].to_vec(), trailing] {
            fs::write(&path, tampered).unwrap();
            assert!(inspect(path.clone()).is_err());
            assert!(restore(path.clone(), summary.id).is_err());
            assert!(!test.root.join("profiles").exists());
            assert!(fs::read_dir(&test.root).unwrap().all(|item| !item
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("workspace-restore-")));
        }
    }

    #[test]
    fn rejects_unsafe_windows_paths() {
        for entry in [
            "user-data/id/../x",
            "user-data/id/NUL.txt",
            "user-data/id/name.",
            "user-data/id/stream:ads",
            "user-data/id/COM¹.txt",
            "user-data/id/a\\b",
            "settings.json",
        ] {
            assert!(validate_path(entry, false).is_err(), "{entry}");
        }
    }
}
