// Download the MCP server source from our R2 CDN into the launcher's
// portable data directory. The app does NOT run or manage it — the user installs deps
// + registers it with their MCP client themselves (see
// mcp/README.md).
//
// The bundle ships pre-packed at ~12 KB (just index.js + package.json
// + README.md), so the download is instant and contains no
// node_modules / .gitignore noise.

use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// Public R2.dev URL for the MCP server bundle (matches the launcher's
/// runtime bucket — same CDN as the browser, Widevine and fingerprint
/// library archives).
const MCP_ARCHIVE_URL: &str =
    "https://pub-e57a7c60f6934eb09a6600bf2fc59cdc.r2.dev/ShardX-MCP.tar.gz";

/// Top-level directory inside the tarball that wraps the actual files.
const MCP_TOP_DIR: &str = "ShardX-MCP";
const MCP_STAGE_DIR: &str = ".mcp.stage";
const MCP_ROLLBACK_DIR: &str = ".mcp.rollback";
const MCP_ARCHIVE_FILE: &str = "bundle.tar.gz";
const MAX_MCP_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MCP_ARCHIVE_ENTRIES: usize = 1_000;
const MAX_MCP_ENTRY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MCP_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;
static MCP_INSTALL_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

fn mcp_install_lock() -> &'static tokio::sync::Mutex<()> {
    MCP_INSTALL_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollbackRecoveryAction {
    RemoveRollback,
    RestoreRollback,
}

/// Download the MCP server into the fixed portable `mcp` directory.
pub async fn download_mcp() -> Result<PathBuf> {
    let _install_guard = mcp_install_lock().lock().await;
    let dest = crate::store::config_root()?.join("mcp");
    let parent = dest
        .parent()
        .context("MCP destination has no parent directory")?;
    fs::create_dir_all(parent)?;

    let staging = parent.join(MCP_STAGE_DIR);
    let rollback = parent.join(MCP_ROLLBACK_DIR);
    recover_interrupted_mcp_swap(&dest, &rollback)?;
    remove_directory_if_exists(&staging)?;
    fs::create_dir_all(&staging)?;

    let result = async {
        let archive = staging.join(MCP_ARCHIVE_FILE);
        download_mcp_archive(&archive).await?;
        extract_mcp_archive(&archive, &staging)?;
        fs::remove_file(&archive)?;
        validate_mcp_tree(&staging)?;
        publish_mcp_transactionally(&staging, &dest, &rollback)?;
        Ok::<_, anyhow::Error>(dest.clone())
    }
    .await;

    if result.is_err() {
        let _ = remove_directory_if_exists(&staging);
    }
    result
}

/// Keep the existing HTTP status validation while streaming the body directly
/// into staging, so a hostile response cannot be accumulated in memory.
async fn download_mcp_archive(path: &Path) -> Result<()> {
    let mut response = reqwest::get(MCP_ARCHIVE_URL)
        .await
        .context("download MCP archive")?
        .error_for_status()
        .context("MCP archive request failed")?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_MCP_ARCHIVE_BYTES)
    {
        anyhow::bail!(
            "MCP archive exceeds the {} MiB download limit",
            MAX_MCP_ARCHIVE_BYTES / 1024 / 1024
        );
    }

    let mut output = tokio::fs::File::create(path).await?;
    let mut received = 0u64;
    while let Some(chunk) = response.chunk().await.context("read MCP archive")? {
        received = received
            .checked_add(chunk.len() as u64)
            .context("MCP archive size overflow")?;
        if received > MAX_MCP_ARCHIVE_BYTES {
            anyhow::bail!(
                "MCP archive exceeds the {} MiB download limit",
                MAX_MCP_ARCHIVE_BYTES / 1024 / 1024
            );
        }
        output.write_all(&chunk).await?;
    }
    output.flush().await?;
    Ok(())
}

fn extract_mcp_archive(archive_path: &Path, staging: &Path) -> Result<()> {
    let archive_file = fs::File::open(archive_path)?;
    let gz = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(gz);
    let mut entries = 0usize;
    let mut unpacked_bytes = 0u64;
    let mut extracted_files = 0usize;

    for entry in archive.entries()? {
        entries = entries
            .checked_add(1)
            .context("MCP archive entry count overflow")?;
        if entries > MAX_MCP_ARCHIVE_ENTRIES {
            anyhow::bail!("MCP archive contains too many entries");
        }

        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let relative = mcp_archive_relative_path(&path)?;
        let entry_type = entry.header().entry_type();
        let declared_size = entry
            .header()
            .size()
            .context("read MCP archive entry size")?;

        let Some(relative) = relative else {
            if entry_type.is_dir() {
                continue;
            }
            anyhow::bail!(
                "MCP archive wrapper must be a directory: {}",
                path.display()
            );
        };
        let output_path = staging.join(relative);
        if entry_type.is_dir() {
            fs::create_dir_all(output_path)?;
            continue;
        }
        if !entry_type.is_file() {
            anyhow::bail!("MCP archive contains an unsupported link or special entry");
        }
        if declared_size > MAX_MCP_ENTRY_BYTES {
            anyhow::bail!(
                "MCP archive entry exceeds the {} MiB per-file limit",
                MAX_MCP_ENTRY_BYTES / 1024 / 1024
            );
        }
        unpacked_bytes = checked_unpacked_size(unpacked_bytes, declared_size)?;
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::File::create(&output_path)?;
        let copied = std::io::copy(&mut entry, &mut output)?;
        if copied != declared_size {
            anyhow::bail!(
                "MCP archive entry has an unexpected size: {}",
                path.display()
            );
        }
        output.flush()?;
        output.sync_all()?;
        extracted_files += 1;
    }

    if extracted_files == 0 {
        anyhow::bail!("MCP archive contained no files (CDN delivered an empty bundle?)");
    }
    Ok(())
}

/// Require every archive path to have exactly one known wrapper directory. This
/// rejects both root files and lookalike sibling wrappers before any file is
/// written to staging.
fn mcp_archive_relative_path(path: &Path) -> Result<Option<PathBuf>> {
    let mut components = path.components();
    let Some(Component::Normal(wrapper)) = components.next() else {
        anyhow::bail!("MCP archive contains an unsafe path: {}", path.display());
    };
    if wrapper != OsStr::new(MCP_TOP_DIR) {
        anyhow::bail!(
            "MCP archive entry is outside the required {MCP_TOP_DIR}/ wrapper: {}",
            path.display()
        );
    }

    let mut relative = PathBuf::new();
    for component in components {
        let Component::Normal(part) = component else {
            anyhow::bail!("MCP archive contains an unsafe path: {}", path.display());
        };
        relative.push(part);
    }
    if relative.as_os_str().is_empty() {
        Ok(None)
    } else {
        Ok(Some(relative))
    }
}

fn checked_unpacked_size(current: u64, entry_size: u64) -> Result<u64> {
    let total = current
        .checked_add(entry_size)
        .context("MCP archive unpacked size overflow")?;
    if total > MAX_MCP_UNPACKED_BYTES {
        anyhow::bail!(
            "MCP archive expands beyond the {} MiB safety limit",
            MAX_MCP_UNPACKED_BYTES / 1024 / 1024
        );
    }
    Ok(total)
}

fn validate_mcp_tree(root: &Path) -> Result<()> {
    for required in ["index.js", "package.json"] {
        let path = root.join(required);
        let metadata = fs::metadata(&path)
            .with_context(|| format!("staged MCP bundle is missing {required}"))?;
        if !metadata.is_file() || metadata.len() == 0 {
            anyhow::bail!(
                "staged MCP bundle file is empty or invalid: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn recover_interrupted_mcp_swap(live: &Path, rollback: &Path) -> Result<()> {
    if !rollback.exists() {
        return Ok(());
    }
    match rollback_recovery_action(
        validate_mcp_tree(live).is_ok(),
        validate_mcp_tree(rollback).is_ok(),
    )? {
        RollbackRecoveryAction::RemoveRollback => remove_directory_if_exists(rollback),
        RollbackRecoveryAction::RestoreRollback => {
            remove_directory_if_exists(live)?;
            fs::rename(rollback, live)
                .with_context(|| format!("restore previous MCP bundle from {}", rollback.display()))
        }
    }
}

fn publish_mcp_transactionally(staged: &Path, live: &Path, rollback: &Path) -> Result<()> {
    validate_mcp_tree(staged)?;
    if rollback.exists() {
        anyhow::bail!(
            "MCP rollback directory still exists: {}",
            rollback.display()
        );
    }
    let had_live = live.exists();
    if had_live {
        fs::rename(live, rollback)
            .with_context(|| format!("move current MCP bundle {} to rollback", live.display()))?;
    }
    if let Err(error) = fs::rename(staged, live) {
        if had_live {
            if let Err(restore_error) = fs::rename(rollback, live) {
                return Err(anyhow::anyhow!(
                    "failed to publish staged MCP bundle {}: {error}; restoring the previous MCP bundle also failed: {restore_error}",
                    live.display()
                ));
            }
        }
        return Err(error).with_context(|| format!("publish staged MCP bundle {}", live.display()));
    }
    if rollback.exists() {
        if let Err(error) = remove_directory_if_exists(rollback) {
            eprintln!(
                "[mcp] installed successfully but could not remove rollback {}: {error}",
                rollback.display()
            );
        }
    }
    Ok(())
}

fn rollback_recovery_action(
    live_is_valid: bool,
    rollback_is_valid: bool,
) -> Result<RollbackRecoveryAction> {
    if live_is_valid {
        return Ok(RollbackRecoveryAction::RemoveRollback);
    }
    if rollback_is_valid {
        return Ok(RollbackRecoveryAction::RestoreRollback);
    }
    anyhow::bail!(
        "MCP live bundle and rollback are both incomplete; preserving both for manual recovery"
    )
}

fn remove_directory_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path).with_context(|| format!("remove directory {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_paths_require_the_exact_wrapper_directory() {
        assert_eq!(
            mcp_archive_relative_path(Path::new("ShardX-MCP/index.js"))
                .unwrap()
                .as_deref(),
            Some(Path::new("index.js"))
        );
        assert!(mcp_archive_relative_path(Path::new("index.js")).is_err());
        assert!(mcp_archive_relative_path(Path::new("ShardX-MCP/../index.js")).is_err());
        assert!(mcp_archive_relative_path(Path::new("ShardX-MCP-copy/index.js")).is_err());
    }

    #[test]
    fn archive_paths_allow_only_the_wrapper_directory_at_root() {
        assert_eq!(
            mcp_archive_relative_path(Path::new(MCP_TOP_DIR)).unwrap(),
            None
        );
        assert!(mcp_archive_relative_path(Path::new("/ShardX-MCP/index.js")).is_err());
    }

    #[test]
    fn unpacked_size_enforces_the_bundle_limit() {
        assert_eq!(checked_unpacked_size(10, 20).unwrap(), 30);
        assert!(checked_unpacked_size(MAX_MCP_UNPACKED_BYTES, 1).is_err());
        assert!(checked_unpacked_size(u64::MAX, 1).is_err());
    }

    #[test]
    fn rollback_recovery_never_discards_a_valid_rollback_for_an_invalid_live_tree() {
        assert_eq!(
            rollback_recovery_action(true, true).unwrap(),
            RollbackRecoveryAction::RemoveRollback
        );
        assert_eq!(
            rollback_recovery_action(false, true).unwrap(),
            RollbackRecoveryAction::RestoreRollback
        );
        assert!(rollback_recovery_action(false, false).is_err());
    }
}
