//! Portable, immutable local extension library. Manifest keys fix Chrome IDs
//! across directory moves; profiles own only their selected library IDs.
use crate::{profile, store};
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const DEFAULT_SELECTION_FILE: &str = "default-extensions.json";

pub(crate) fn default_selection() -> Result<Vec<String>> {
    let path = store::config_root()?.join(DEFAULT_SELECTION_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    serde_json::from_slice(&bytes).with_context(|| format!("Invalid {}", path.display()))
}

#[derive(Debug, Serialize)]
pub struct ExtensionEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub profile_names: Vec<String>,
    pub trash_count: usize,
    pub error: Option<String>,
    pub usage_error: Option<String>,
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32 && id.bytes().all(|c| (b'a'..=b'p').contains(&c)),
        "Invalid extension ID"
    );
    Ok(())
}

pub fn path_for(id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(store::extensions_dir()?.join(id))
}

fn read_manifest(root: &Path) -> Result<Value> {
    let path = root.join("manifest.json");
    ensure!(
        fs::metadata(&path)?.len() <= 1024 * 1024,
        "Extension manifest is too large"
    );
    let text = fs::read_to_string(&path).context("Read extension manifest.json")?;
    let manifest: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .context("Invalid extension manifest.json")?;
    ensure!(
        manifest["manifest_version"].as_u64() == Some(3),
        "This browser requires Manifest V3. Import a Manifest V3 extension."
    );
    for field in ["name", "version"] {
        ensure!(
            manifest[field]
                .as_str()
                .is_some_and(|v| !v.trim().is_empty()),
            "Extension manifest is missing {field}"
        );
    }
    Ok(manifest)
}

fn id_from_key(key: &[u8]) -> String {
    Sha256::digest(key)[..16]
        .iter()
        .flat_map(|b| [char::from(b'a' + (b >> 4)), char::from(b'a' + (b & 15))])
        .collect()
}

fn manifest_key(manifest: &Value) -> Result<Option<Vec<u8>>> {
    manifest
        .get("key")
        .map(|key| {
            let key = key.as_str().context("Extension key must be a string")?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(key.split_whitespace().collect::<String>())
                .context("Invalid extension key")?;
            ensure!(!decoded.is_empty(), "Extension key is empty");
            Ok(decoded)
        })
        .transpose()
}

fn new_public_key() -> Result<Vec<u8>> {
    use ring::signature::{Ed25519KeyPair, KeyPair};
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
        .map_err(|_| anyhow::anyhow!("Generate extension identity"))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| anyhow::anyhow!("Read generated extension identity"))?;
    // RFC 8410 SubjectPublicKeyInfo; only the public key is retained.
    let mut der = vec![0x30, 0x2a, 0x30, 5, 6, 3, 0x2b, 0x65, 0x70, 3, 0x21, 0];
    der.extend_from_slice(pair.public_key().as_ref());
    Ok(der)
}

fn varint(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut result = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*cursor).context("Truncated CRX header")?;
        *cursor += 1;
        ensure!(shift != 63 || byte <= 1, "Invalid CRX varint");
        result |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(result);
        }
    }
    bail!("Invalid CRX varint")
}

fn protobuf_fields(bytes: &[u8]) -> Result<Vec<(u64, &[u8])>> {
    let mut cursor = 0;
    let mut fields = Vec::new();
    while cursor < bytes.len() {
        let tag = varint(bytes, &mut cursor)?;
        match tag & 7 {
            0 => {
                varint(bytes, &mut cursor)?;
            }
            1 => {
                cursor = cursor.checked_add(8).context("CRX overflow")?;
            }
            5 => {
                cursor = cursor.checked_add(4).context("CRX overflow")?;
            }
            2 => {
                let size = usize::try_from(varint(bytes, &mut cursor)?)?;
                let end = cursor.checked_add(size).context("CRX overflow")?;
                fields.push((
                    tag >> 3,
                    bytes.get(cursor..end).context("Truncated CRX field")?,
                ));
                cursor = end;
            }
            _ => bail!("Unsupported CRX wire type"),
        }
        ensure!(cursor <= bytes.len(), "Truncated CRX header");
    }
    Ok(fields)
}

fn archive_payload(bytes: &[u8]) -> Result<(&[u8], Option<Vec<u8>>)> {
    if !bytes.starts_with(b"Cr24") {
        return Ok((bytes, None));
    }
    let word = |at: usize| -> Result<usize> {
        Ok(u32::from_le_bytes(
            bytes
                .get(at..at + 4)
                .context("Truncated CRX header")?
                .try_into()?,
        ) as usize)
    };
    match word(4)? {
        2 => {
            let key_end = 16usize.checked_add(word(8)?).context("CRX overflow")?;
            let offset = key_end.checked_add(word(12)?).context("CRX overflow")?;
            Ok((
                bytes.get(offset..).context("Truncated CRX archive")?,
                Some(
                    bytes
                        .get(16..key_end)
                        .context("Truncated CRX key")?
                        .to_vec(),
                ),
            ))
        }
        3 => {
            let offset = 12usize.checked_add(word(8)?).context("CRX overflow")?;
            let fields = protobuf_fields(bytes.get(12..offset).context("Truncated CRX header")?)?;
            let signed = fields
                .iter()
                .find(|(tag, _)| *tag == 10000)
                .context("CRX identity is missing")?
                .1;
            let signed_fields = protobuf_fields(signed)?;
            let id = signed_fields
                .iter()
                .find(|(tag, _)| *tag == 1)
                .context("CRX ID is missing")?
                .1;
            ensure!(id.len() == 16, "Invalid CRX ID");
            for (_, proof) in fields.iter().filter(|(tag, _)| matches!(tag, 2 | 3)) {
                for (tag, key) in protobuf_fields(proof)? {
                    if tag == 1 && Sha256::digest(key)[..16] == *id {
                        return Ok((
                            bytes.get(offset..).context("Truncated CRX archive")?,
                            Some(key.to_vec()),
                        ));
                    }
                }
            }
            bail!("CRX identity has no matching public key")
        }
        _ => bail!("Unsupported CRX format"),
    }
}

pub(crate) fn tree_files(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(
        root: &Path,
        path: &Path,
        files: &mut Vec<PathBuf>,
        bytes: &mut u64,
        depth: usize,
    ) -> Result<()> {
        ensure!(depth < 64, "Extension directory nesting is too deep");
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            !crate::profile_backup::is_link_or_reparse_point(&metadata),
            "Extension contains a link: {}",
            path.display()
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                walk(root, &entry?.path(), files, bytes, depth + 1)?;
            }
        } else {
            ensure!(metadata.is_file(), "Unsupported extension file");
            *bytes = bytes
                .checked_add(metadata.len())
                .context("Extension size overflow")?;
            ensure!(
                *bytes <= MAX_PACKAGE_BYTES && files.len() < MAX_FILES,
                "Extension exceeds the 256 MiB / 20000 file limit"
            );
            files.push(path.strip_prefix(root)?.to_path_buf());
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, root, &mut files, &mut 0, 0)?;
    files.sort();
    Ok(files)
}

pub(crate) fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let files = tree_files(source)?;
    fs::create_dir_all(destination)?;
    for relative in files {
        let out = destination.join(&relative);
        fs::create_dir_all(out.parent().unwrap())?;
        fs::copy(source.join(relative), out)?;
    }
    Ok(())
}

fn unpack(source: &Path, stage: &Path) -> Result<Option<Vec<u8>>> {
    if source.is_dir() {
        copy_tree(source, stage)?;
        return Ok(None);
    }
    ensure!(
        fs::metadata(source)?.len() <= MAX_PACKAGE_BYTES,
        "Extension package exceeds 256 MiB"
    );
    let bytes = fs::read(source)?;
    let (payload, key) = archive_payload(&bytes)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(payload))
        .context("Not a ZIP or CRX extension")?;
    ensure!(
        archive.len() <= MAX_FILES,
        "Extension contains too many files"
    );
    let mut total = 0u64;
    let mut names = HashSet::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        crate::profile_backup::validate_archive_entry_name(entry.name())?;
        ensure!(
            names.insert(entry.name().to_ascii_lowercase()),
            "Duplicate extension archive path"
        );
        ensure!(
            !entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000),
            "Extension archive contains a link"
        );
        total = total
            .checked_add(entry.size())
            .context("Extension size overflow")?;
        ensure!(
            total <= MAX_PACKAGE_BYTES,
            "Extension expands beyond 256 MiB"
        );
        let out = stage.join(
            entry
                .enclosed_name()
                .context("Invalid extension archive path")?,
        );
        if entry.is_dir() {
            fs::create_dir_all(out)?;
        } else {
            fs::create_dir_all(out.parent().unwrap())?;
            let expected = entry.size();
            let mut file = fs::File::create(out)?;
            ensure!(
                std::io::copy(&mut entry.by_ref().take(expected + 1), &mut file)? == expected,
                "Invalid extension archive size"
            );
            file.flush()?;
        }
    }
    Ok(key)
}

fn manifest_root(stage: &Path) -> Result<PathBuf> {
    if stage.join("manifest.json").is_file() {
        return Ok(stage.to_path_buf());
    }
    let children = fs::read_dir(stage)?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        children.len() == 1 && children[0].path().join("manifest.json").is_file(),
        "No manifest.json at extension root"
    );
    Ok(children[0].path())
}

pub(crate) fn read_entry(id: &str, root: &Path) -> Result<ExtensionEntry> {
    validate_id(id)?;
    let manifest = read_manifest(root)?;
    let key = manifest_key(&manifest)?.context("Extension identity is missing")?;
    ensure!(
        id_from_key(&key) == id,
        "Extension ID does not match its manifest key"
    );
    let messages = if let Some(locale) = manifest["default_locale"].as_str() {
        ensure!(
            locale
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')),
            "Invalid extension locale"
        );
        serde_json::from_str::<Value>(&fs::read_to_string(
            root.join("_locales").join(locale).join("messages.json"),
        )?)?
    } else {
        Value::Null
    };
    let text = |field: &str| {
        let value = manifest[field].as_str().unwrap_or("");
        value
            .strip_prefix("__MSG_")
            .and_then(|v| v.strip_suffix("__"))
            .and_then(|key| messages[key]["message"].as_str())
            .unwrap_or(value)
            .to_string()
    };
    Ok(ExtensionEntry {
        id: id.into(),
        name: text("name"),
        version: text("version"),
        description: text("description"),
        profile_names: Vec::new(),
        trash_count: 0,
        error: None,
        usage_error: None,
    })
}

fn import_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Unpack a package into `stage`, pin its identity into the manifest, and
/// return the validated entry plus the directory that should become the
/// library entry.
fn stage_package(source: &Path, stage: &Path) -> Result<(ExtensionEntry, PathBuf)> {
    let header_key = unpack(source, stage)?;
    let root = manifest_root(stage)?;
    let mut manifest = read_manifest(&root)?;
    let declared = manifest_key(&manifest)?;
    if let (Some(header), Some(declared)) = (&header_key, &declared) {
        ensure!(
            header == declared,
            "CRX public key conflicts with manifest key"
        );
    }
    let key = header_key
        .or(declared)
        .map(Ok)
        .unwrap_or_else(new_public_key)?;
    let id = id_from_key(&key);
    manifest["key"] = Value::String(base64::engine::general_purpose::STANDARD.encode(key));
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let entry = read_entry(&id, &root)?;
    tree_files(&root)?;
    Ok((entry, root))
}

fn prepare_stage() -> Result<PathBuf> {
    let lib = store::extensions_dir()?;
    let stage = lib.join(format!(".import-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&stage)?;
    Ok(stage)
}

pub fn import(source: &Path) -> Result<ExtensionEntry> {
    ensure!(
        source.try_exists()?,
        crate::errors::Failure::NotFound(format!(
            "Extension source does not exist: {}",
            source.display()
        ))
    );
    let _import = import_lock()
        .try_lock()
        .map_err(|_| crate::errors::conflict("Extension import is in progress"))?;
    let _activity = crate::operations::activity();
    {
        let _guard = crate::process::lock_profile_resources()?;
        profile::recover_interrupted_operations_locked()?;
        cleanup_staging_locked()?;
    }
    let stage = prepare_stage()?;
    let result = (|| {
        let (entry, root) = stage_package(source, &stage)?;
        let _guard = crate::process::lock_profile_resources()?;
        let destination = path_for(&entry.id)?;
        ensure!(
            !destination.exists(),
            crate::errors::conflict("Extension already exists in the library; remove its references before replacing it")
        );
        fs::rename(&root, &destination)?;
        Ok(entry)
    })();
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    result
}

#[derive(Debug)]
pub(crate) enum UpsertOutcome {
    Installed(String),
    Unchanged(String),
    Replaced { from: String, to: String },
}

/// Install or update the extension identified by `expected_id` from an official
/// package. The extension ID travels in the manifest key, so a replacement
/// keeps every profile reference valid without rewriting profile files.
pub(crate) fn upsert(expected_id: &str, source: &Path) -> Result<UpsertOutcome> {
    ensure!(
        source.try_exists()?,
        crate::errors::Failure::NotFound(format!(
            "Extension source does not exist: {}",
            source.display()
        ))
    );
    validate_id(expected_id)?;
    let _import = import_lock()
        .try_lock()
        .map_err(|_| crate::errors::conflict("Extension import is in progress"))?;
    let _activity = crate::operations::activity();
    {
        let _guard = crate::process::lock_profile_resources()?;
        profile::recover_interrupted_operations_locked()?;
        cleanup_staging_locked()?;
    }
    let stage = prepare_stage()?;
    let result = (|| {
        let (entry, root) = stage_package(source, &stage)?;
        ensure!(
            entry.id == expected_id,
            "Downloaded extension identity does not match the requested extension"
        );
        let _guard = crate::process::lock_profile_resources()?;
        let destination = path_for(expected_id)?;
        if !destination.exists() {
            fs::rename(&root, &destination)?;
            return Ok(UpsertOutcome::Installed(entry.version));
        }
        let current = read_entry(expected_id, &destination)?;
        if current.version == entry.version {
            return Ok(UpsertOutcome::Unchanged(current.version));
        }
        ensure_no_active_profile_uses(expected_id)?;
        let previous = store::extensions_dir()?.join(format!(".replace-{}", uuid::Uuid::new_v4()));
        fs::rename(&destination, &previous)?;
        if let Err(error) = fs::rename(&root, &destination) {
            let _ = fs::rename(&previous, &destination);
            return Err(error.into());
        }
        if let Err(error) = fs::remove_dir_all(&previous) {
            eprintln!(
                "[extensions] previous version cleanup remains pending at {}: {error}",
                previous.display()
            );
        }
        Ok(UpsertOutcome::Replaced {
            from: current.version,
            to: entry.version,
        })
    })();
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    result
}

fn ensure_no_active_profile_uses(id: &str) -> Result<()> {
    for entry in fs::read_dir(store::profiles_dir()?)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let stored: profile::StoredProfile = store::load_json_with_backup(&path)?;
        if !stored.meta.extensions.iter().any(|selected| selected == id) {
            continue;
        }
        if crate::is_profile_active(&stored.meta.id) {
            let name = stored
                .config
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&stored.meta.id);
            anyhow::bail!(crate::errors::conflict(format!(
                "Close the browser for \"{name}\" before updating this extension"
            )));
        }
    }
    Ok(())
}

pub(crate) fn installed_version(id: &str) -> Result<Option<String>> {
    let path = path_for(id)?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(read_entry(id, &path)?.version))
}

/// Add every ID to the default selection file, preserving entries the user
/// added and the existing order. Returns whether the file changed.
pub(crate) fn ensure_default_selection(ids: &[String]) -> Result<bool> {
    for id in ids {
        validate_id(id)?;
    }
    let path = store::config_root()?.join(DEFAULT_SELECTION_FILE);
    let mut selection: Vec<String> = match fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("Read {}", path.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let original = selection.clone();
    for id in ids {
        if !selection.iter().any(|selected| selected == id) {
            selection.push(id.clone());
        }
    }
    if selection == original && path.exists() {
        return Ok(false);
    }
    let mut bytes = serde_json::to_vec_pretty(&selection)?;
    bytes.push(b'\n');
    store::atomic_write(&path, &bytes)?;
    Ok(true)
}

#[derive(Debug, Default)]
pub(crate) struct Backfill {
    pub updated: usize,
    pub pending: Vec<String>,
}

/// Give existing profiles the default extension IDs once. `only` limits the
/// pass to profiles that a previous run could not edit because their browser
/// was running. Profiles already listing the IDs are never modified, so a
/// later manual selection is preserved.
pub(crate) fn backfill_profiles(ids: &[String], only: Option<&[String]>) -> Result<Backfill> {
    let mut outcome = Backfill::default();
    if ids.is_empty() {
        return Ok(outcome);
    }
    let _activity = crate::operations::activity();
    let _guard = crate::process::lock_profile_resources()?;
    profile::recover_interrupted_operations_locked()?;
    for entry in fs::read_dir(store::profiles_dir()?)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let mut stored: profile::StoredProfile = match store::load_json_with_backup(&path) {
            Ok(stored) => stored,
            Err(error) => {
                eprintln!(
                    "[extensions] skipped unreadable profile {}: {error:#}",
                    path.display()
                );
                continue;
            }
        };
        if stored.meta.temporary {
            continue;
        }
        if let Some(only) = only {
            if !only.iter().any(|id| id == &stored.meta.id) {
                continue;
            }
        }
        let missing: Vec<String> = ids
            .iter()
            .filter(|id| !stored.meta.extensions.contains(id))
            .cloned()
            .collect();
        if missing.is_empty() {
            continue;
        }
        if profile::ensure_stopped(&stored.meta.id).is_err() {
            outcome.pending.push(stored.meta.id.clone());
            continue;
        }
        stored.meta.extensions.extend(missing);
        profile::save_raw(&mut stored)?;
        outcome.updated += 1;
    }
    Ok(outcome)
}

pub(crate) fn references(id: &str) -> Result<Vec<String>> {
    Ok(profile_references()?.remove(id).unwrap_or_default())
}

fn profile_references() -> Result<HashMap<String, Vec<String>>> {
    let mut references = HashMap::<String, Vec<String>>::new();
    for entry in fs::read_dir(store::profiles_dir()?)? {
        let path = entry?.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let stored: profile::StoredProfile = store::load_json_with_backup(&path)?;
        for id in &stored.meta.extensions {
            references.entry(id.clone()).or_default().push(
                stored
                    .config
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&stored.meta.id)
                    .to_string(),
            );
        }
    }
    Ok(references)
}

pub fn list() -> Result<Vec<ExtensionEntry>> {
    let _guard = crate::process::lock_profile_resources()?;
    profile::recover_interrupted_operations_locked()?;
    let (references, mut usage_error) = match profile_references() {
        Ok(references) => (references, None),
        Err(error) => (
            HashMap::new(),
            Some(format!("Unable to check profile references: {error:#}")),
        ),
    };
    let trash = match crate::trash::list_locked() {
        Ok(trash) => trash,
        Err(error) => {
            usage_error = Some(format!("Unable to check recycle-bin references: {error:#}"));
            Vec::new()
        }
    };
    let mut entries = Vec::new();
    for item in fs::read_dir(store::extensions_dir()?)? {
        let item = item?;
        let id = item.file_name().to_string_lossy().to_string();
        if id.starts_with('.') {
            continue;
        }
        let mut entry = read_entry(&id, &item.path()).unwrap_or_else(|error| ExtensionEntry {
            id: id.clone(),
            name: id.clone(),
            version: String::new(),
            description: String::new(),
            profile_names: Vec::new(),
            trash_count: 0,
            error: Some(format!("{error:#}")),
            usage_error: None,
        });
        entry.profile_names = references.get(&id).cloned().unwrap_or_default();
        entry.usage_error = usage_error.clone();
        entry.trash_count = trash
            .iter()
            .filter(|item| item.extensions.contains(&id))
            .count();
        entries.push(entry);
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    Ok(entries)
}

pub fn delete(id: &str) -> Result<()> {
    let _activity = crate::operations::activity();
    let _guard = crate::process::lock_profile_resources()?;
    profile::recover_interrupted_operations_locked()?;
    ensure!(
        !default_selection()?.iter().any(|selected| selected == id),
        crate::errors::conflict(format!(
            "Remove this extension from {DEFAULT_SELECTION_FILE} before deleting it"
        ))
    );
    ensure!(
        references(id)?.is_empty()
            && !crate::trash::list_locked()?
                .iter()
                .any(|e| e.extensions.iter().any(|v| v == id)),
        crate::errors::conflict("Extension is referenced by a profile or the recycle bin; remove those references first")
    );
    let path = path_for(id)?;
    if path.exists() {
        let pending = store::extensions_dir()?.join(format!(".delete-{id}"));
        fs::rename(&path, &pending)?;
        crate::notify_store_changed("extensions");
        fs::remove_dir_all(pending)
            .context("Extension removed from library; file cleanup remains pending")?;
    }
    Ok(())
}

fn cleanup_staging_locked() -> Result<()> {
    for entry in fs::read_dir(store::extensions_dir()?)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        // `.replace-*` backups are only removed after the new version is in
        // place, so a failed restore never loses the last usable copy.
        if [".import-", ".restore-", ".delete-"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            let metadata = fs::symlink_metadata(entry.path())?;
            ensure!(
                metadata.is_dir() && !crate::profile_backup::is_link_or_reparse_point(&metadata),
                "Invalid extension staging directory"
            );
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

pub fn load_paths(ids: &[String]) -> Result<Vec<PathBuf>> {
    let mut unique = HashSet::new();
    ids.iter()
        .map(|id| {
            ensure!(unique.insert(id), "Duplicate extension selection");
            let path = path_for(id)?;
            read_entry(id, &path).with_context(|| {
                format!("Extension {id} is unavailable; restore it or deselect it")
            })?;
            ensure!(
                !path.to_string_lossy().contains(','),
                "Extension library path cannot contain a comma"
            );
            Ok(path)
        })
        .collect()
}

/// A restore either reuses identical files or installs the captured version.
/// A different version under the same ID is a conflict, never an overwrite.
pub(crate) fn restore_files(id: &str, source: &Path) -> Result<bool> {
    read_entry(id, source)?;
    let target = path_for(id)?;
    let files = tree_files(source)?;
    if target.exists() {
        ensure!(
            files == tree_files(&target)?,
            "Extension restore conflicts with installed files: {id}"
        );
        for file in files {
            ensure!(
                fs::read(source.join(&file))? == fs::read(target.join(file))?,
                "Extension restore conflicts with installed version: {id}"
            );
        }
        return Ok(false);
    }
    let stage = store::extensions_dir()?.join(format!(".restore-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        copy_tree(source, &stage)?;
        fs::rename(&stage, &target)?;
        Ok(true)
    })();
    if stage.exists() {
        fs::remove_dir_all(stage)?;
    }
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn fixture(directory: &Path) -> ExtensionEntry {
        fs::create_dir_all(directory).unwrap();
        fs::write(directory.join("manifest.json"), r#"{"manifest_version":3,"name":"StorageFixture","version":"1.0","permissions":["storage"],"background":{"service_worker":"worker.js"}}"#).unwrap();
        fs::write(directory.join("worker.js"), "chrome.runtime.onInstalled.addListener(() => chrome.storage.local.set({installed: true}));").unwrap();
        fs::write(
            directory.join("check.html"),
            "<!doctype html><title>Extension fixture</title><p>Extension fixture</p>",
        )
        .unwrap();
        import(directory).unwrap()
    }

    fn zip_bytes(root: &Path) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for file in tree_files(root).unwrap() {
            zip.start_file(
                file.to_string_lossy().replace('\\', "/"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(&fs::read(root.join(file)).unwrap()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn field(tag: u64, value: &[u8]) -> Vec<u8> {
        fn put(mut n: u64, out: &mut Vec<u8>) {
            while n >= 128 {
                out.push(n as u8 | 128);
                n >>= 7;
            }
            out.push(n as u8);
        }
        let mut out = Vec::new();
        put(tag << 3 | 2, &mut out);
        put(value.len() as u64, &mut out);
        out.extend_from_slice(value);
        out
    }

    #[test]
    fn damaged_extension_does_not_hide_healthy_entries_and_mv2_is_rejected() {
        let test = store::TestStore::new();
        let healthy = fixture(&test.root.join("healthy"));
        let damaged = fixture(&test.root.join("damaged"));
        fs::write(path_for(&damaged.id).unwrap().join("manifest.json"), b"{").unwrap();
        let entries = list().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .find(|e| e.id == healthy.id)
            .unwrap()
            .error
            .is_none());
        assert!(entries
            .iter()
            .find(|e| e.id == damaged.id)
            .unwrap()
            .error
            .is_some());
        let source = test.root.join("mv2");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("manifest.json"),
            br#"{"manifest_version":2,"name":"Legacy","version":"1.0"}"#,
        )
        .unwrap();
        assert!(import(&source)
            .unwrap_err()
            .to_string()
            .contains("Manifest V3"));
    }

    #[test]
    fn upsert_replaces_only_the_same_identity_and_keeps_references_valid() {
        let test = store::TestStore::new();
        let entry = fixture(&test.root.join("v1"));
        let installed = path_for(&entry.id).unwrap();
        let key = read_manifest(&installed).unwrap()["key"]
            .as_str()
            .unwrap()
            .to_string();

        let source = test.root.join("v2");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("manifest.json"),
            serde_json::json!({
                "manifest_version": 3,
                "name": "StorageFixture",
                "version": "2.0",
                "key": key,
                "permissions": ["storage"],
                "background": {"service_worker": "worker.js"}
            })
            .to_string(),
        )
        .unwrap();
        fs::write(source.join("worker.js"), "// v2").unwrap();
        let package = test.root.join("v2.zip");
        fs::write(&package, zip_bytes(&source)).unwrap();

        match upsert(&entry.id, &package).unwrap() {
            UpsertOutcome::Replaced { from, to } => {
                assert_eq!(from, "1.0");
                assert_eq!(to, "2.0");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(
            installed_version(&entry.id).unwrap().as_deref(),
            Some("2.0")
        );
        assert_eq!(read_manifest(&installed).unwrap()["key"], key);
        assert!(matches!(
            upsert(&entry.id, &package).unwrap(),
            UpsertOutcome::Unchanged(version) if version == "2.0"
        ));

        let foreign = test.root.join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(
            foreign.join("manifest.json"),
            serde_json::json!({
                "manifest_version": 3,
                "name": "Foreign",
                "version": "3.0"
            })
            .to_string(),
        )
        .unwrap();
        let foreign_package = test.root.join("foreign.zip");
        fs::write(&foreign_package, zip_bytes(&foreign)).unwrap();
        assert!(upsert(&entry.id, &foreign_package).is_err());
        assert_eq!(
            installed_version(&entry.id).unwrap().as_deref(),
            Some("2.0")
        );
    }

    #[test]
    fn default_selection_merges_without_reordering_or_duplicating() {
        let _test = store::TestStore::new();
        let first = "a".repeat(32);
        let second = "b".repeat(32);
        assert!(ensure_default_selection(std::slice::from_ref(&first)).unwrap());
        assert!(!ensure_default_selection(std::slice::from_ref(&first)).unwrap());
        assert!(ensure_default_selection(std::slice::from_ref(&second)).unwrap());
        assert_eq!(default_selection().unwrap(), vec![first, second]);
        assert!(ensure_default_selection(&["invalid".into()]).is_err());
    }

    #[test]
    fn backfill_adds_defaults_once_and_respects_later_selections() {
        let _test = store::TestStore::new();
        let create = |name: &str| {
            crate::save_profile_core(
                None,
                serde_json::json!({
                    "name": name,
                    "noise": {
                        "canvas": {"enabled": true, "seed": 0},
                        "webgl": {"enabled": true, "seed": 0, "intensity": 0.0005}
                    },
                    "_meta": {"id": "", "extensions": []}
                }),
                false,
            )
            .unwrap()
        };
        let first = create("Legacy one");
        let second = create("Legacy two");
        let first_id = "a".repeat(32);
        let second_id = "b".repeat(32);

        let outcome = backfill_profiles(std::slice::from_ref(&first_id), None).unwrap();
        assert_eq!(outcome.updated, 2);
        assert!(outcome.pending.is_empty());
        for id in [&first.id, &second.id] {
            assert_eq!(
                profile::load_raw(id).unwrap().meta.extensions,
                vec![first_id.clone()]
            );
        }

        // A deliberate removal stays removed on the next pass.
        let mut stored = profile::load_raw(&first.id).unwrap();
        stored.meta.extensions.clear();
        profile::save_raw(&mut stored).unwrap();
        // The sync passes only the pending list after the first workspace pass.
        assert_eq!(
            backfill_profiles(std::slice::from_ref(&first_id), Some(&[]))
                .unwrap()
                .updated,
            0
        );
        assert!(profile::load_raw(&first.id)
            .unwrap()
            .meta
            .extensions
            .is_empty());

        // A pending pass only touches the listed profile.
        let mut second_stored = profile::load_raw(&second.id).unwrap();
        second_stored.meta.extensions.clear();
        profile::save_raw(&mut second_stored).unwrap();
        let pending = [second.id.clone()];
        assert_eq!(
            backfill_profiles(std::slice::from_ref(&second_id), Some(&pending))
                .unwrap()
                .updated,
            1
        );
        assert_eq!(
            profile::load_raw(&second.id).unwrap().meta.extensions,
            [second_id]
        );
    }

    #[test]
    fn directory_zip_and_crx_preserve_identity_and_reject_conflicts() {
        let test = store::TestStore::new();
        let source = test.root.join("source");
        let entry = fixture(&source);
        let original = read_manifest(&source).unwrap();
        assert!(original.get("key").is_none(), "user source is immutable");
        let installed = path_for(&entry.id).unwrap();
        let manifest = read_manifest(&installed).unwrap();
        let key = manifest_key(&manifest).unwrap().unwrap();
        let zip = zip_bytes(&installed);
        let packaged = test.root.join("extension.zip");
        fs::write(&packaged, &zip).unwrap();
        assert!(import(&packaged)
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        let moved = test.root.join("moved");
        copy_tree(&installed, &moved).unwrap();
        assert_eq!(read_entry(&entry.id, &moved).unwrap().id, entry.id);
        let proof = field(1, &key);
        let mut header = field(2, &proof);
        header.extend(field(10000, &field(1, &Sha256::digest(&key)[..16])));
        let mut crx = b"Cr24".to_vec();
        crx.extend(3u32.to_le_bytes());
        crx.extend((header.len() as u32).to_le_bytes());
        crx.extend(header);
        crx.extend(&zip);
        let (payload, parsed_key) = archive_payload(&crx).unwrap();
        assert_eq!(payload, zip);
        assert_eq!(parsed_key.unwrap(), key);
        delete(&entry.id).unwrap();
        fs::write(&packaged, &crx).unwrap();
        assert_eq!(import(&packaged).unwrap().id, entry.id);
        assert_eq!(
            read_manifest(&path_for(&entry.id).unwrap()).unwrap(),
            manifest
        );
        assert!(archive_payload(&crx[..11]).is_err());
        let restore_source = test.root.join("conflict");
        copy_tree(&installed, &restore_source).unwrap();
        fs::write(restore_source.join("worker.js"), "different version").unwrap();
        assert!(restore_files(&entry.id, &restore_source).is_err());
        assert_ne!(
            fs::read(installed.join("worker.js")).unwrap(),
            b"different version"
        );
    }

    #[test]
    fn extension_archive_cannot_escape_import_stage() {
        let test = store::TestStore::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("../escaped.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"escaped").unwrap();
        let path = test.root.join("unsafe.zip");
        fs::write(&path, zip.finish().unwrap().into_inner()).unwrap();
        assert!(import(&path).is_err());
        assert!(!store::extensions_dir()
            .unwrap()
            .join("escaped.txt")
            .exists());
        assert_eq!(
            fs::read_dir(store::extensions_dir().unwrap())
                .unwrap()
                .count(),
            0
        );
    }
}
