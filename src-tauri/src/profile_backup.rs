//! Complete, versioned ShardX profile backups.
//!
//! A v2 `.shardx-backup` is a binary ZIP container holding the exact launcher
//! profile, its bound proxy, the complete Chromium user-data tree, and the raw
//! OSCrypt key. The key is re-wrapped with the destination Windows user's
//! DPAPI credentials during restore, so Chromium keeps the same encrypted
//! database key even though the launcher assigns a fresh profile UUID.

use crate::{cookies, display_order, fingerprints, process, profile, proxy, store};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use zip::write::SimpleFileOptions;

const BACKUP_FORMAT: &str = "shardx-profile-backup";
const BACKUP_VERSION: u32 = 2;
const MANIFEST_ENTRY: &str = "manifest.json";
const PROFILE_ENTRY: &str = "profile.json";
const PROXY_ENTRY: &str = "proxy.json";
const OS_CRYPT_KEY_ENTRY: &str = "crypto/os-crypt-key.bin";
const USER_DATA_PREFIX: &str = "user-data/";

const MAX_BACKUP_BATCH: usize = 100;
const MAX_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_BATCH_UNCOMPRESSED_BYTES: u64 = 128 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 250_000;
const MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_PROFILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROXY_BYTES: usize = 1024 * 1024;
const MAX_USER_DATA_DEPTH: usize = 128;
const RESTORE_DISK_RESERVE_BYTES: u64 = 1024 * 1024 * 1024;
static BACKUP_OPERATION_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BackupEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupEntry {
    path: String,
    kind: BackupEntryKind,
    size: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BackupManifest {
    format: String,
    version: u32,
    launcher_version: String,
    backup_id: String,
    name: String,
    exported_at: String,
    source_profile_id: String,
    has_proxy: bool,
    user_data_file_count: usize,
    user_data_bytes: u64,
    entries: Vec<BackupEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileBackupSummary {
    profile_count: usize,
    file_count: usize,
    data_bytes: u64,
}

struct SourceEntry {
    source: PathBuf,
    relative: PathBuf,
    kind: BackupEntryKind,
}

struct PendingExport {
    temporary_path: PathBuf,
    final_path: PathBuf,
    file_count: usize,
    data_bytes: u64,
}

struct PreparedImport {
    stage_root: PathBuf,
    staged_user_data: PathBuf,
    new_profile_id: String,
    backup_id: String,
    profile: profile::StoredProfile,
    bound_proxy: Option<proxy::ProxyEntry>,
    file_count: usize,
    data_bytes: u64,
    restore_bytes: u64,
}

#[derive(Clone)]
struct ImportArtifacts {
    profile_id: String,
    created_fingerprint_id: Option<String>,
}

fn zip_options() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true)
        .unix_permissions(0o600)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_profile_id(profile_id: &str) -> Result<()> {
    if profile_id.is_empty()
        || !profile_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid profile id");
    }
    Ok(())
}

fn backup_filename_component(name: &str) -> String {
    let component: String = name
        .chars()
        .take(64)
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if component.is_empty() {
        "profile".to_string()
    } else {
        component
    }
}

fn unix_timestamp() -> Result<(u128, String)> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    Ok((millis, format!("@{}", millis / 1000)))
}

fn should_skip_user_data(relative: &Path) -> bool {
    let mut components = relative.components().filter_map(|component| match component {
        Component::Normal(value) => value.to_str(),
        _ => None,
    });
    let values: Vec<&str> = components.by_ref().collect();
    if values.iter().any(|value| value.eq_ignore_ascii_case("Crashpad")) {
        return true;
    }
    let Some(name) = values.last() else {
        return false;
    };
    name.eq_ignore_ascii_case("DevToolsActivePort")
        || name.starts_with("Singleton")
        || name.starts_with("BrowserMetrics")
}

fn collect_user_data_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<SourceEntry>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_USER_DATA_DEPTH {
        bail!("profile user-data directory nesting exceeds the safety limit");
    }
    let mut children: Vec<_> = fs::read_dir(directory)
        .with_context(|| format!("read user-data directory {}", directory.display()))?
        .collect::<std::result::Result<_, _>>()?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        let source = child.path();
        let relative = source
            .strip_prefix(root)
            .context("user-data path escaped its root")?
            .to_path_buf();
        if relative.components().count() > MAX_USER_DATA_DEPTH {
            bail!("profile user-data directory nesting exceeds the safety limit");
        }
        if should_skip_user_data(&relative) {
            continue;
        }
        let metadata = fs::symlink_metadata(&source)?;
        if is_link_or_reparse_point(&metadata) {
            bail!("user-data contains an unsupported link or reparse point: {}", relative.display());
        }
        if metadata.is_dir() {
            entries.push(SourceEntry {
                source: source.clone(),
                relative: relative.clone(),
                kind: BackupEntryKind::Directory,
            });
            collect_user_data_entries(root, &source, entries, depth + 1)?;
        } else if metadata.is_file() {
            entries.push(SourceEntry {
                source,
                relative,
                kind: BackupEntryKind::File,
            });
        }
        if entries.len() > MAX_ARCHIVE_ENTRIES {
            bail!("profile user-data contains too many files");
        }
    }
    Ok(())
}

fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

fn archive_relative_path(relative: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            bail!("invalid user-data path component");
        };
        let value = value
            .to_str()
            .ok_or_else(|| anyhow!("user-data contains a non-Unicode filename"))?;
        if value.is_empty() || value == "." || value == ".." {
            bail!("invalid user-data filename");
        }
        parts.push(value);
    }
    Ok(parts.join("/"))
}

fn write_bytes_entry(
    archive: &mut zip::ZipWriter<File>,
    path: &str,
    bytes: &[u8],
    records: &mut Vec<BackupEntry>,
) -> Result<()> {
    archive.start_file(path, zip_options())?;
    archive.write_all(bytes)?;
    records.push(BackupEntry {
        path: path.to_string(),
        kind: BackupEntryKind::File,
        size: bytes.len() as u64,
        sha256: sha256_hex(bytes),
    });
    Ok(())
}

fn write_user_data_entries(
    archive: &mut zip::ZipWriter<File>,
    source_root: &Path,
    records: &mut Vec<BackupEntry>,
) -> Result<(usize, u64)> {
    archive.add_directory(USER_DATA_PREFIX, zip_options())?;
    records.push(BackupEntry {
        path: USER_DATA_PREFIX.to_string(),
        kind: BackupEntryKind::Directory,
        size: 0,
        sha256: String::new(),
    });

    let mut sources = Vec::new();
    collect_user_data_entries(source_root, source_root, &mut sources, 0)?;
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];

    for source in sources {
        let relative = archive_relative_path(&source.relative)?;
        let mut archive_path = format!("{USER_DATA_PREFIX}{relative}");
        match source.kind {
            BackupEntryKind::Directory => {
                archive_path.push('/');
                archive.add_directory(&archive_path, zip_options())?;
                records.push(BackupEntry {
                    path: archive_path,
                    kind: BackupEntryKind::Directory,
                    size: 0,
                    sha256: String::new(),
                });
            }
            BackupEntryKind::File => {
                let expected_size = fs::metadata(&source.source)?.len();
                total_bytes = total_bytes
                    .checked_add(expected_size)
                    .ok_or_else(|| anyhow!("profile user-data size overflow"))?;
                if total_bytes > MAX_UNCOMPRESSED_BYTES {
                    bail!("profile user-data exceeds the 64 GiB backup limit");
                }
                archive.start_file(&archive_path, zip_options())?;
                let mut input = File::open(&source.source)
                    .with_context(|| format!("open {}", source.source.display()))?;
                let mut hasher = Sha256::new();
                let mut written = 0u64;
                loop {
                    let count = input.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    archive.write_all(&buffer[..count])?;
                    hasher.update(&buffer[..count]);
                    written += count as u64;
                }
                if written != expected_size {
                    bail!(
                        "user-data file changed while it was being backed up: {}",
                        source.relative.display()
                    );
                }
                records.push(BackupEntry {
                    path: archive_path,
                    kind: BackupEntryKind::File,
                    size: written,
                    sha256: format!("{:x}", hasher.finalize()),
                });
                file_count += 1;
            }
        }
    }
    Ok((file_count, total_bytes))
}

fn bound_proxy_for_profile(stored: &profile::StoredProfile) -> Result<Option<proxy::ProxyEntry>> {
    if let Some(proxy_id) = stored.meta.proxy_id.as_deref() {
        return proxy::get(proxy_id)?
            .ok_or_else(|| anyhow!("profile references a proxy that no longer exists"))
            .map(Some);
    }
    Ok(stored.meta.inline_proxy.clone())
}

fn build_backup_archive(profile_id: &str, stamp: u128, exported_at: &str) -> Result<PendingExport> {
    let stored = profile::load_raw(profile_id)?;
    let name = stored
        .config
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    profile::validate_profile_name(&name)?;
    if stored.meta.id != profile_id {
        bail!("profile id does not match its on-disk filename");
    }

    let user_data = profile::user_data_dir(profile_id)?;
    let os_crypt_key = cookies::export_os_crypt_key(&user_data)?;
    let bound_proxy = bound_proxy_for_profile(&stored)?;
    let profile_json = serde_json::to_vec_pretty(&stored)?;
    if profile_json.len() > MAX_PROFILE_BYTES {
        bail!("profile metadata exceeds the 16 MiB backup limit");
    }
    let proxy_json = bound_proxy
        .as_ref()
        .map(serde_json::to_vec_pretty)
        .transpose()?;
    if proxy_json
        .as_ref()
        .is_some_and(|bytes| bytes.len() > MAX_PROXY_BYTES)
    {
        bail!("proxy metadata exceeds the 1 MiB backup limit");
    }

    let backup_id = uuid::Uuid::new_v4().to_string();
    let short_backup_id = &backup_id[..8];
    let file_name = format!(
        "{}__{}__{}.shardx-backup",
        backup_filename_component(&name),
        short_backup_id,
        stamp
    );
    let directory = store::exports_dir()?;
    let final_path = directory.join(file_name);
    let temporary_path = final_path.with_extension("shardx-backup.tmp");
    if temporary_path.exists() {
        fs::remove_file(&temporary_path)?;
    }

    let result = (|| -> Result<PendingExport> {
        let output = File::create(&temporary_path)?;
        let mut archive = zip::ZipWriter::new(output);
        let mut records = Vec::new();
        write_bytes_entry(&mut archive, PROFILE_ENTRY, &profile_json, &mut records)?;
        if let Some(bytes) = proxy_json.as_deref() {
            write_bytes_entry(&mut archive, PROXY_ENTRY, bytes, &mut records)?;
        }
        write_bytes_entry(
            &mut archive,
            OS_CRYPT_KEY_ENTRY,
            &os_crypt_key,
            &mut records,
        )?;
        let (file_count, data_bytes) =
            write_user_data_entries(&mut archive, &user_data, &mut records)?;
        let payload_bytes = records.iter().try_fold(0u64, |total, record| {
            total
                .checked_add(record.size)
                .ok_or_else(|| anyhow!("backup size overflow"))
        })?;
        if payload_bytes > MAX_UNCOMPRESSED_BYTES {
            bail!("backup exceeds the 64 GiB uncompressed limit");
        }

        let manifest = BackupManifest {
            format: BACKUP_FORMAT.to_string(),
            version: BACKUP_VERSION,
            launcher_version: env!("CARGO_PKG_VERSION").to_string(),
            backup_id,
            name,
            exported_at: exported_at.to_string(),
            source_profile_id: profile_id.to_string(),
            has_proxy: bound_proxy.is_some(),
            user_data_file_count: file_count,
            user_data_bytes: data_bytes,
            entries: records,
        };
        let manifest_json = serde_json::to_vec_pretty(&manifest)?;
        if manifest_json.len() > MAX_MANIFEST_BYTES {
            bail!("backup manifest exceeds the 32 MiB limit");
        }
        archive.start_file(MANIFEST_ENTRY, zip_options())?;
        archive.write_all(&manifest_json)?;
        let output = archive.finish()?;
        output.sync_all()?;

        if fs::metadata(&temporary_path)?.len() > MAX_ARCHIVE_BYTES {
            bail!("profile backup exceeds the 32 GiB archive limit");
        }
        Ok(PendingExport {
            temporary_path: temporary_path.clone(),
            final_path,
            file_count,
            data_bytes,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

pub(crate) fn export(profile_ids: Vec<String>) -> std::result::Result<ProfileBackupSummary, String> {
    let _backup_guard = BACKUP_OPERATION_LOCK
        .lock()
        .map_err(|_| "profile backup operation lock is poisoned".to_string())?;
    if profile_ids.is_empty() {
        return Err("select at least one profile to export".into());
    }
    if profile_ids.len() > MAX_BACKUP_BATCH {
        return Err("at most 100 profiles can be exported at once".into());
    }
    let mut unique_ids = HashSet::new();
    for id in &profile_ids {
        validate_profile_id(id).map_err(|error| error.to_string())?;
        if !unique_ids.insert(id.clone()) {
            return Err("duplicate profile id in export request".into());
        }
    }

    let _resource_guard = process::lock_profile_resources().map_err(|error| error.to_string())?;
    for id in &profile_ids {
        profile::ensure_stopped(id).map_err(|error| error.to_string())?;
    }
    let (stamp, exported_at) = unix_timestamp().map_err(|error| error.to_string())?;
    let mut pending = Vec::with_capacity(profile_ids.len());
    for id in &profile_ids {
        match build_backup_archive(id, stamp, &exported_at) {
            Ok(item) => pending.push(item),
            Err(error) => {
                for item in &pending {
                    let _ = fs::remove_file(&item.temporary_path);
                }
                return Err(error.to_string());
            }
        }
    }

    let mut published = Vec::new();
    for item in &pending {
        if let Err(error) = fs::rename(&item.temporary_path, &item.final_path) {
            for cleanup in &pending {
                let _ = fs::remove_file(&cleanup.temporary_path);
            }
            for cleanup in published {
                let _ = fs::remove_file(cleanup);
            }
            return Err(error.to_string());
        }
        published.push(item.final_path.clone());
    }

    Ok(ProfileBackupSummary {
        profile_count: pending.len(),
        file_count: pending.iter().map(|item| item.file_count).sum(),
        data_bytes: pending.iter().map(|item| item.data_bytes).sum(),
    })
}

fn validate_archive_entry_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains(':')
    {
        bail!("backup contains an unsafe archive path");
    }
    for component in name.trim_end_matches('/').split('/') {
        if component.is_empty() || component == "." || component == ".." {
            bail!("backup contains an unsafe archive path");
        }
    }
    Ok(())
}

fn validate_record_path(record: &BackupEntry) -> Result<()> {
    validate_archive_entry_name(&record.path)?;
    if let Some(relative) = record.path.strip_prefix(USER_DATA_PREFIX) {
        let depth = relative.trim_end_matches('/').split('/').filter(|part| !part.is_empty()).count();
        if depth > MAX_USER_DATA_DEPTH {
            bail!("backup user-data directory nesting exceeds the safety limit");
        }
    }
    match record.kind {
        BackupEntryKind::Directory => {
            if !record.path.ends_with('/') || record.size != 0 || !record.sha256.is_empty() {
                bail!("backup contains an invalid directory record");
            }
        }
        BackupEntryKind::File => {
            if record.path.ends_with('/') || record.sha256.len() != 64 {
                bail!("backup contains an invalid file record");
            }
        }
    }
    let allowed = record.path == PROFILE_ENTRY
        || record.path == PROXY_ENTRY
        || record.path == OS_CRYPT_KEY_ENTRY
        || record.path.starts_with(USER_DATA_PREFIX);
    if !allowed {
        bail!("backup contains an unsupported entry: {}", record.path);
    }
    Ok(())
}

fn read_limited<R: Read>(mut reader: R, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("backup metadata entry exceeds its size limit");
    }
    Ok(bytes)
}

fn extract_record(
    archive: &mut zip::ZipArchive<File>,
    record: &BackupEntry,
    staged_user_data: &Path,
) -> Result<Option<Vec<u8>>> {
    let mut entry = archive
        .by_name(&record.path)
        .with_context(|| format!("backup is missing {}", record.path))?;
    if entry.size() != record.size {
        bail!("backup entry size does not match manifest: {}", record.path);
    }
    if record.kind == BackupEntryKind::Directory {
        if !entry.is_dir() {
            bail!("backup directory record is not a directory: {}", record.path);
        }
        if record.path.starts_with(USER_DATA_PREFIX) {
            let relative = record.path[USER_DATA_PREFIX.len()..].trim_end_matches('/');
            let destination = safe_user_data_destination(staged_user_data, relative)?;
            fs::create_dir_all(destination)?;
        }
        return Ok(None);
    }
    if entry.is_dir() {
        bail!("backup file record is a directory: {}", record.path);
    }

    let mut hasher = Sha256::new();
    let mut total = 0u64;
    if record.path.starts_with(USER_DATA_PREFIX) {
        let relative = &record.path[USER_DATA_PREFIX.len()..];
        let destination = safe_user_data_destination(staged_user_data, relative)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&destination)?;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let count = entry.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            hasher.update(&buffer[..count]);
            total += count as u64;
            if total > record.size {
                bail!("backup entry exceeds its declared size: {}", record.path);
            }
        }
        output.sync_all()?;
        if total != record.size {
            bail!("backup entry ended early: {}", record.path);
        }
        let digest = format!("{:x}", hasher.finalize());
        if digest != record.sha256 {
            bail!("backup entry failed integrity verification: {}", record.path);
        }
        return Ok(None);
    }

    let limit = match record.path.as_str() {
        PROFILE_ENTRY => MAX_PROFILE_BYTES,
        PROXY_ENTRY => MAX_PROXY_BYTES,
        OS_CRYPT_KEY_ENTRY => 32,
        _ => bail!("unsupported backup metadata entry"),
    };
    let bytes = read_limited(&mut entry, limit)?;
    total = bytes.len() as u64;
    hasher.update(&bytes);
    if total != record.size || format!("{:x}", hasher.finalize()) != record.sha256 {
        bail!("backup metadata failed integrity verification: {}", record.path);
    }
    Ok(Some(bytes))
}

fn safe_user_data_destination(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty() {
        return Ok(root.to_path_buf());
    }
    validate_archive_entry_name(relative)?;
    let mut destination = root.to_path_buf();
    for component in relative.split('/') {
        destination.push(component);
    }
    Ok(destination)
}

fn prepare_import_inner(
    path: &Path,
    new_profile_id: String,
    stage_root: PathBuf,
    remaining_batch_bytes: u64,
) -> Result<PreparedImport> {
    let metadata = fs::metadata(path).with_context(|| format!("open {}", path.display()))?;
    if metadata.len() > MAX_ARCHIVE_BYTES {
        bail!("{} exceeds the 32 GiB backup limit", path.display());
    }
    let file = File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).context("backup is not a valid v2 archive")?;
    // MAX_ARCHIVE_ENTRIES limits entries copied from user-data. A valid
    // archive additionally contains the user-data root, profile, OSCrypt key,
    // optional proxy, and manifest entries.
    if archive.len() > MAX_ARCHIVE_ENTRIES + 5 {
        bail!("backup contains too many entries");
    }

    let mut actual_names = HashSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        validate_archive_entry_name(&name)?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("backup contains an unsupported symbolic link");
        }
        if !actual_names.insert(name.clone()) {
            bail!("backup contains duplicate archive entry: {name}");
        }
    }

    let manifest_bytes = {
        let entry = archive
            .by_name(MANIFEST_ENTRY)
            .context("backup is missing manifest.json")?;
        read_limited(entry, MAX_MANIFEST_BYTES)?
    };
    let manifest: BackupManifest =
        serde_json::from_slice(&manifest_bytes).context("backup manifest is invalid")?;
    if manifest.format != BACKUP_FORMAT || manifest.version != BACKUP_VERSION {
        bail!("backup is not a supported ShardX v2 profile backup");
    }
    if manifest.entries.len() > MAX_ARCHIVE_ENTRIES + 4 {
        bail!("backup manifest contains too many entries");
    }
    if manifest.launcher_version.trim().is_empty() || manifest.exported_at.trim().is_empty() {
        bail!("backup manifest is missing version or timestamp metadata");
    }
    uuid::Uuid::parse_str(&manifest.backup_id).context("backup id is invalid")?;
    validate_profile_id(&manifest.source_profile_id)?;
    profile::validate_profile_name(&manifest.name)?;

    let mut record_names = HashSet::new();
    let mut total_uncompressed = 0u64;
    let mut user_data_file_count = 0usize;
    let mut user_data_bytes = 0u64;
    for record in &manifest.entries {
        validate_record_path(record)?;
        if !record_names.insert(record.path.clone()) {
            bail!("backup manifest contains duplicate entry: {}", record.path);
        }
        total_uncompressed = total_uncompressed
            .checked_add(record.size)
            .ok_or_else(|| anyhow!("backup size overflow"))?;
        if total_uncompressed > MAX_UNCOMPRESSED_BYTES {
            bail!("backup exceeds the 64 GiB uncompressed limit");
        }
        if record.kind == BackupEntryKind::File && record.path.starts_with(USER_DATA_PREFIX) {
            user_data_file_count += 1;
            user_data_bytes = user_data_bytes
                .checked_add(record.size)
                .ok_or_else(|| anyhow!("backup user-data size overflow"))?;
        }
    }
    if total_uncompressed > remaining_batch_bytes {
        bail!("selected backups exceed the 128 GiB batch restore limit");
    }
    if manifest.user_data_file_count != user_data_file_count
        || manifest.user_data_bytes != user_data_bytes
    {
        bail!("backup user-data totals do not match its manifest");
    }
    if !record_names.contains(PROFILE_ENTRY)
        || !record_names.contains(OS_CRYPT_KEY_ENTRY)
        || record_names.contains(PROXY_ENTRY) != manifest.has_proxy
    {
        bail!("backup manifest is missing required profile, key, or proxy entries");
    }
    let expected_names: HashSet<String> = record_names
        .iter()
        .cloned()
        .chain(std::iter::once(MANIFEST_ENTRY.to_string()))
        .collect();
    if actual_names != expected_names {
        bail!("backup archive contents do not exactly match its manifest");
    }

    let available = store::available_space(
        stage_root
            .parent()
            .context("restore staging directory has no parent")?,
    )?;
    if available < total_uncompressed.saturating_add(RESTORE_DISK_RESERVE_BYTES) {
        bail!(
            "not enough free disk space to restore {} (need {} bytes plus 1 GiB reserve)",
            path.display(),
            total_uncompressed
        );
    }

    let staged_user_data = stage_root.clone();
    fs::create_dir_all(&staged_user_data)?;
    let mut profile_bytes = None;
    let mut proxy_bytes = None;
    let mut os_crypt_key = None;
    for record in &manifest.entries {
        if let Some(bytes) = extract_record(&mut archive, record, &staged_user_data)? {
            match record.path.as_str() {
                PROFILE_ENTRY => profile_bytes = Some(bytes),
                PROXY_ENTRY => proxy_bytes = Some(bytes),
                OS_CRYPT_KEY_ENTRY => os_crypt_key = Some(bytes),
                _ => bail!("unexpected backup metadata entry"),
            }
        }
    }

    let profile_bytes = profile_bytes.ok_or_else(|| anyhow!("backup profile payload is missing"))?;
    let mut stored: profile::StoredProfile =
        serde_json::from_slice(&profile_bytes).context("backup profile payload is invalid")?;
    if stored.meta.id != manifest.source_profile_id {
        bail!("backup profile id does not match its manifest");
    }
    let stored_name = stored
        .config
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if stored_name != manifest.name {
        bail!("backup profile name does not match its manifest");
    }
    let bound_proxy = proxy_bytes
        .map(|bytes| serde_json::from_slice::<proxy::ProxyEntry>(&bytes))
        .transpose()
        .context("backup proxy payload is invalid")?;
    let key = os_crypt_key.ok_or_else(|| anyhow!("backup OSCrypt key is missing"))?;
    cookies::install_os_crypt_key(&staged_user_data, &key)?;

    stored.meta.id = new_profile_id.clone();
    stored.meta.last_launched_at = None;
    stored.meta.created_at = None;
    stored.meta.pinned = false;
    stored.meta.total_runtime_ms = 0;
    stored.meta.temporary = false;
    stored.meta.proxy_id = None;
    stored.meta.inline_proxy = None;

    Ok(PreparedImport {
        stage_root,
        staged_user_data,
        new_profile_id,
        backup_id: manifest.backup_id,
        profile: stored,
        bound_proxy,
        file_count: user_data_file_count,
        data_bytes: user_data_bytes,
        restore_bytes: total_uncompressed,
    })
}

fn prepare_import(path: &Path, remaining_batch_bytes: u64) -> Result<PreparedImport> {
    if path.extension().and_then(|value| value.to_str()).map(str::to_ascii_lowercase)
        != Some("shardx-backup".to_string())
    {
        bail!("{} is not a .shardx-backup file", path.display());
    }
    let new_profile_id = uuid::Uuid::new_v4().to_string();
    // Stage beside the final directory to keep paths short and guarantee that
    // the final rename is atomic on the same volume.
    let stage_root = store::user_data_root()?.join(format!(".restore-{new_profile_id}"));
    if stage_root.exists() {
        fs::remove_dir_all(&stage_root)?;
    }
    fs::create_dir_all(&stage_root)?;
    let result = prepare_import_inner(
        path,
        new_profile_id,
        stage_root.clone(),
        remaining_batch_bytes,
    );
    if result.is_err() {
        let _ = fs::remove_dir_all(stage_root);
    }
    result
}

fn remove_import_artifacts(artifacts: &ImportArtifacts) -> Result<()> {
    let profile_path = store::profiles_dir()?.join(format!("{}.json", artifacts.profile_id));
    if profile_path.exists() {
        fs::remove_file(profile_path)?;
    }
    let user_data = store::user_data_root()?.join(&artifacts.profile_id);
    if user_data.exists() {
        fs::remove_dir_all(user_data)?;
    }
    if let Some(fingerprint_id) = artifacts.created_fingerprint_id.as_deref() {
        fingerprints::delete(fingerprint_id)?;
    }
    Ok(())
}

fn cleanup_staging(prepared: &[PreparedImport]) {
    for item in prepared {
        if item.stage_root.exists() {
            let _ = fs::remove_dir_all(&item.stage_root);
        }
    }
}

fn import_failure(
    error: anyhow::Error,
    prepared: &[PreparedImport],
    imported: &[ImportArtifacts],
) -> String {
    let mut rollback_errors = Vec::new();
    for artifacts in imported.iter().rev() {
        if let Err(rollback) = remove_import_artifacts(artifacts) {
            rollback_errors.push(rollback.to_string());
        }
    }
    for item in prepared {
        if item.stage_root.exists() {
            if let Err(rollback) = fs::remove_dir_all(&item.stage_root) {
                rollback_errors.push(rollback.to_string());
            }
        }
    }
    if rollback_errors.is_empty() {
        error.to_string()
    } else {
        format!(
            "{error}; complete backup import rollback was incomplete: {}",
            rollback_errors.join(", ")
        )
    }
}

pub(crate) fn import(paths: Vec<String>) -> std::result::Result<ProfileBackupSummary, String> {
    let _backup_guard = BACKUP_OPERATION_LOCK
        .lock()
        .map_err(|_| "profile backup operation lock is poisoned".to_string())?;
    if paths.is_empty() {
        return Err("select at least one .shardx-backup file".into());
    }
    if paths.len() > MAX_BACKUP_BATCH {
        return Err("at most 100 profiles can be imported at once".into());
    }
    let mut unique_paths = HashSet::new();
    let mut prepared: Vec<PreparedImport> = Vec::with_capacity(paths.len());
    let mut backup_ids = HashSet::new();
    let mut batch_bytes = 0u64;
    for raw_path in paths {
        let path = PathBuf::from(raw_path);
        let canonical = fs::canonicalize(&path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        if !unique_paths.insert(canonical.clone()) {
            for item in &prepared {
                let _ = fs::remove_dir_all(&item.stage_root);
            }
            return Err("the same backup file was selected more than once".into());
        }
        match prepare_import(
            &canonical,
            MAX_BATCH_UNCOMPRESSED_BYTES.saturating_sub(batch_bytes),
        ) {
            Ok(item) => {
                if !backup_ids.insert(item.backup_id.clone()) {
                    let _ = fs::remove_dir_all(&item.stage_root);
                    for prepared_item in &prepared {
                        let _ = fs::remove_dir_all(&prepared_item.stage_root);
                    }
                    return Err(format!(
                        "backup {} was selected more than once",
                        item.backup_id
                    ));
                }
                batch_bytes = batch_bytes
                    .checked_add(item.restore_bytes)
                    .ok_or_else(|| "selected backup size overflow".to_string())?;
                prepared.push(item);
            }
            Err(error) => {
                for item in &prepared {
                    let _ = fs::remove_dir_all(&item.stage_root);
                }
                return Err(error.to_string());
            }
        }
    }

    let _resource_guard = match process::lock_profile_resources() {
        Ok(guard) => guard,
        Err(error) => {
            cleanup_staging(&prepared);
            return Err(error.to_string());
        }
    };

    let mut imported = Vec::with_capacity(prepared.len());
    let bound_proxies = prepared
        .iter()
        .map(|item| item.bound_proxy.clone())
        .collect();
    let commit = proxy::with_restored_proxy_bindings(
        bound_proxies,
        |restored_proxy_ids| -> Result<()> {
            let mut imported_ids = Vec::with_capacity(prepared.len());
            for (item, restored_proxy_id) in prepared.iter_mut().zip(restored_proxy_ids) {
                let created_fingerprint_id;
                let fingerprint_id =
                    if let Some(existing_id) = crate::infer_gpu_preset_id(&item.profile.config) {
                        created_fingerprint_id = None;
                        Some(existing_id)
                    } else {
                        let payload = serde_json::to_string(&item.profile.config)?;
                        let id_hint = format!("restored-{}", &item.backup_id[..8]);
                        let entry = fingerprints::import(&payload, Some(id_hint))?;
                        created_fingerprint_id = Some(entry.id.clone());
                        Some(entry.id)
                    };
                item.profile.meta.gpu_preset_id = fingerprint_id;
                item.profile.meta.proxy_id = restored_proxy_id;

                let artifacts = ImportArtifacts {
                    profile_id: item.new_profile_id.clone(),
                    created_fingerprint_id,
                };
                imported.push(artifacts);

                let final_user_data = store::user_data_root()?.join(&item.new_profile_id);
                if final_user_data.exists() {
                    bail!("restored profile id already exists");
                }
                fs::rename(&item.staged_user_data, &final_user_data)?;
                profile::save_restored(&mut item.profile)?;
                imported_ids.push(item.new_profile_id.clone());
            }
            // Re-read after every restored profile has been persisted. The
            // order validator rejects ids absent from this list.
            let current_profile_ids = profile::list_all()?
                .into_iter()
                .map(|profile| profile.id)
                .collect::<Vec<_>>();
            display_order::append_profiles(&current_profile_ids, &imported_ids)?;
            Ok(())
        },
    );

    if let Err(error) = commit {
        return Err(import_failure(error, &prepared, &imported));
    }

    for item in &prepared {
        let _ = fs::remove_dir_all(&item.stage_root);
    }
    crate::notify_store_changed("profiles");
    Ok(ProfileBackupSummary {
        profile_count: prepared.len(),
        file_count: prepared.iter().map(|item| item.file_count).sum(),
        data_bytes: prepared.iter().map(|item| item.data_bytes).sum(),
    })
}

/// Remove only launcher-owned staging artifacts left by an interrupted export
/// or restore. Reparse points are skipped so cleanup can never traverse outside
/// the portable data tree.
pub(crate) fn cleanup_stale_artifacts() -> Result<usize> {
    let mut removed = 0usize;
    for entry in fs::read_dir(store::user_data_root()?)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(".restore-") {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !is_link_or_reparse_point(&metadata) {
            fs::remove_dir_all(entry.path())?;
            removed += 1;
        }
    }
    for entry in fs::read_dir(store::exports_dir()?)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.ends_with(".shardx-backup.tmp") && entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_archive_path_traversal() {
        assert!(validate_archive_entry_name("user-data/Default/Cookies").is_ok());
        assert!(validate_archive_entry_name("../Cookies").is_err());
        assert!(validate_archive_entry_name("user-data\\Cookies").is_err());
        assert!(validate_archive_entry_name("C:/Cookies").is_err());
    }

    #[test]
    fn excludes_only_transient_browser_files() {
        assert!(should_skip_user_data(Path::new("DevToolsActivePort")));
        assert!(should_skip_user_data(Path::new("SingletonLock")));
        assert!(should_skip_user_data(Path::new("Crashpad/reports/report.dmp")));
        assert!(!should_skip_user_data(Path::new("Default/Network/Cookies")));
        assert!(!should_skip_user_data(Path::new("Default/Service Worker/CacheStorage/data")));
        assert!(!should_skip_user_data(Path::new("Default/IndexedDB/site.leveldb")));
    }
}
