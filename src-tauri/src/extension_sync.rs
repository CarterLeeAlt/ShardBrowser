//! Keeps the two default extensions current from their official Chrome Web
//! Store entries. Downloads use the launcher's dedicated download client; a
//! successful import updates `default-extensions.json`, and existing profiles
//! receive the IDs once (running profiles on a later pass).

use crate::{download, extensions, settings, store};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

// Only the extension identities are fixed. Version, package size, checksum and
// download URL always come from the official update response, so a new store
// release is picked up without touching the launcher.
const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "bgnkhhnnamicmpeenaelnjfhikgbkllg",
        label: "AdGuard",
    },
    CatalogEntry {
        id: "dhdgffkkebhmkfjojejmpbldmpobfkfo",
        label: "Tampermonkey",
    },
];
const STATE_FILE: &str = "extension-sync.json";
const CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60;

struct CatalogEntry {
    id: &'static str,
    label: &'static str,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncStatus {
    pub checked_at: Option<u64>,
    pub throttled: bool,
    pub installed: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub failures: Vec<String>,
    pub profiles_updated: usize,
    pub profiles_pending: usize,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    checked_at: Option<u64>,
    backfill_done: bool,
    pending_profiles: Vec<String>,
}

#[derive(Debug)]
struct UpdateInfo {
    version: String,
    size: u64,
    sha256: String,
    url: String,
}

enum EntryOutcome {
    Installed(String),
    Updated { from: String, to: String },
    Unchanged(String),
}

static RUNNING: AtomicBool = AtomicBool::new(false);
static LAST: OnceLock<Mutex<Option<SyncStatus>>> = OnceLock::new();

pub fn last_status() -> Option<SyncStatus> {
    LAST.get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|value| value.clone())
}

pub async fn sync(force: bool) -> SyncStatus {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return SyncStatus {
            failures: vec!["Extension sync is already running".into()],
            ..SyncStatus::default()
        };
    }
    let status = sync_inner(force).await;
    RUNNING.store(false, Ordering::SeqCst);
    if let Ok(mut last) = LAST.get_or_init(|| Mutex::new(None)).lock() {
        *last = Some(status.clone());
    }
    status
}

async fn sync_inner(force: bool) -> SyncStatus {
    let mut status = SyncStatus::default();
    let state_path = match store::config_root() {
        Ok(root) => root.join(STATE_FILE),
        Err(error) => {
            status.failures.push(format!("{error:#}"));
            return status;
        }
    };
    let mut state = match read_state(&state_path) {
        Ok(state) => state,
        Err(error) => {
            status.failures.push(format!("{error:#}"));
            return status;
        }
    };
    let now = epoch();
    if !force
        && state
            .checked_at
            .is_some_and(|at| now.saturating_sub(at) < CHECK_INTERVAL_SECS)
        && state.pending_profiles.is_empty()
    {
        status.throttled = true;
        status.checked_at = state.checked_at;
        return status;
    }
    let settings = match settings::load() {
        Ok(settings) => settings,
        Err(error) => {
            status.failures.push(format!("{error:#}"));
            return status;
        }
    };
    let client = match download::client(&settings, Duration::from_secs(10), Duration::from_secs(60))
    {
        Ok(client) => client,
        Err(error) => {
            status.failures.push(format!("Download proxy: {error:#}"));
            return status;
        }
    };
    let prodversion = crate::runtime::engine_version_for_downloads();
    let downloads = match store::config_root() {
        Ok(root) => root.join("extension-downloads"),
        Err(error) => {
            status.failures.push(format!("{error:#}"));
            return status;
        }
    };
    if let Err(error) = clear_downloads(&downloads).await {
        status.failures.push(format!("{error:#}"));
        return status;
    }

    for entry in CATALOG {
        match sync_entry(&client, entry, &prodversion, &downloads).await {
            Ok(EntryOutcome::Installed(version)) => {
                status.installed.push(format!("{} {version}", entry.label));
            }
            Ok(EntryOutcome::Updated { from, to }) => {
                status
                    .updated
                    .push(format!("{} {from} → {to}", entry.label));
            }
            Ok(EntryOutcome::Unchanged(version)) => {
                status.unchanged.push(format!("{} {version}", entry.label));
            }
            Err(error) => status.failures.push(format!("{}: {error:#}", entry.label)),
        }
    }

    let mut available = Vec::new();
    for entry in CATALOG {
        match extensions::installed_version(entry.id) {
            Ok(Some(_)) => available.push(entry.id.to_string()),
            Ok(None) => {}
            Err(error) => status.failures.push(format!("{}: {error:#}", entry.label)),
        }
    }

    if !available.is_empty() {
        if let Err(error) = extensions::ensure_default_selection(&available) {
            status
                .failures
                .push(format!("Default extension list: {error:#}"));
        }
        let candidates = state.backfill_done.then(|| state.pending_profiles.clone());
        let should_backfill = candidates
            .as_ref()
            .map(|list| !list.is_empty())
            .unwrap_or(true);
        if should_backfill {
            let ids = available.clone();
            let only = candidates.clone();
            match tokio::task::spawn_blocking(move || {
                extensions::backfill_profiles(&ids, only.as_deref())
            })
            .await
            {
                Ok(Ok(outcome)) => {
                    status.profiles_updated = outcome.updated;
                    status.profiles_pending = outcome.pending.len();
                    state.pending_profiles = outcome.pending;
                    state.backfill_done = true;
                }
                Ok(Err(error)) => status
                    .failures
                    .push(format!("Profile extension backfill: {error:#}")),
                Err(error) => status
                    .failures
                    .push(format!("Profile extension backfill: {error}")),
            }
        }
    }

    if !status.installed.is_empty() || !status.updated.is_empty() {
        crate::notify_store_changed("extensions");
    }
    if status.profiles_updated > 0 {
        crate::notify_store_changed("profiles");
    }
    if status.failures.is_empty() {
        state.checked_at = Some(now);
        status.checked_at = Some(now);
        if let Err(error) = write_state(&state_path, &state) {
            status.failures.push(format!("{error:#}"));
        }
    }
    status
}

async fn sync_entry(
    client: &reqwest::Client,
    entry: &CatalogEntry,
    prodversion: &str,
    downloads: &Path,
) -> Result<EntryOutcome> {
    let info = fetch_update_check(client, entry.id, prodversion).await?;
    if let Some(installed) = extensions::installed_version(entry.id)? {
        // Never downgrade a newer locally installed build to the store version.
        if installed == info.version || version_is_newer(&installed, &info.version) {
            return Ok(EntryOutcome::Unchanged(installed));
        }
    }
    tokio::fs::create_dir_all(downloads).await?;
    let package = downloads.join(format!("{}-{}.crx", entry.id, uuid::Uuid::new_v4()));
    if let Err(error) = download_verified(client, &info, &package).await {
        let _ = tokio::fs::remove_file(&package).await;
        return Err(error);
    }
    let id = entry.id.to_string();
    let path = package.clone();
    let outcome = tokio::task::spawn_blocking(move || extensions::upsert(&id, &path)).await;
    let _ = tokio::fs::remove_file(&package).await;
    Ok(match outcome?? {
        extensions::UpsertOutcome::Installed(version) => EntryOutcome::Installed(version),
        extensions::UpsertOutcome::Unchanged(version) => EntryOutcome::Unchanged(version),
        extensions::UpsertOutcome::Replaced { from, to } => EntryOutcome::Updated { from, to },
    })
}

fn update_check_url(id: &str, prodversion: &str) -> String {
    format!(
        "https://clients2.google.com/service/update2/crx?response=updatecheck&os=win&arch=x64&prodversion={prodversion}&acceptformat=crx3&x=id%3D{id}%26installsource%3Dondemand%26uc"
    )
}

async fn fetch_update_check(
    client: &reqwest::Client,
    id: &str,
    prodversion: &str,
) -> Result<UpdateInfo> {
    let response = client
        .get(update_check_url(id, prodversion))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .context("Query the Chrome Web Store")?;
    let response = response
        .error_for_status()
        .context("Chrome Web Store update check")?;
    let bytes = response
        .bytes()
        .await
        .context("Read the Chrome Web Store update check")?;
    ensure!(bytes.len() <= 64 * 1024, "Update response is too large");
    parse_update_check(std::str::from_utf8(&bytes)?, id)
}

fn parse_update_check(xml: &str, id: &str) -> Result<UpdateInfo> {
    let marker = format!("appid=\"{id}\"");
    let app = xml
        .find(&marker)
        .with_context(|| format!("Chrome Web Store did not return extension {id}"))?;
    let check = app
        + xml[app..]
            .find("<updatecheck ")
            .context("Chrome Web Store response has no updatecheck")?;
    let end = check
        + xml[check..]
            .find("/>")
            .context("Chrome Web Store updatecheck is incomplete")?;
    let tag = &xml[check..end];
    let attr = |name: &str| -> Option<&str> {
        let needle = format!("{name}=\"");
        let start = tag.find(&needle)? + needle.len();
        let length = tag[start..].find('"')?;
        Some(&tag[start..start + length])
    };
    ensure!(
        attr("status") == Some("ok"),
        "Chrome Web Store returned no update for this browser version"
    );
    let version = attr("version").context("Update response has no version")?;
    ensure!(
        !version.is_empty()
            && version.split('.').count() <= 4
            && version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())),
        "Update response has an invalid version"
    );
    let size: u64 = attr("size")
        .context("Update response has no size")?
        .parse()
        .context("Update response has an invalid size")?;
    ensure!(
        size > 0 && size <= extensions::MAX_PACKAGE_BYTES,
        "Extension is too large"
    );
    let sha256 = attr("hash_sha256").context("Update response has no checksum")?;
    ensure!(
        sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Update response has an invalid checksum"
    );
    let url = attr("codebase").context("Update response has no download URL")?;
    let parsed = reqwest::Url::parse(url).context("Update response has an invalid download URL")?;
    ensure!(
        parsed.scheme() == "https"
            && parsed
                .host_str()
                .is_some_and(|host| host == "clients2.googleusercontent.com"
                    || host.ends_with(".googleusercontent.com")),
        "Update response points outside the Chrome Web Store"
    );
    Ok(UpdateInfo {
        version: version.into(),
        size,
        sha256: sha256.to_ascii_lowercase(),
        url: url.into(),
    })
}

async fn download_verified(
    client: &reqwest::Client,
    info: &UpdateInfo,
    destination: &Path,
) -> Result<()> {
    let response = client
        .get(&info.url)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .context("Download the extension")?
        .error_for_status()
        .context("Download the extension")?;
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= extensions::MAX_PACKAGE_BYTES),
        "Extension is too large"
    );
    let mut file = tokio::fs::File::create(destination).await?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut response = response;
    while let Some(chunk) = response.chunk().await? {
        total = total
            .checked_add(chunk.len() as u64)
            .context("Extension size overflow")?;
        ensure!(
            total <= extensions::MAX_PACKAGE_BYTES,
            "Extension is too large"
        );
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    ensure!(
        total == info.size,
        "Downloaded extension size differs from the update response"
    );
    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    ensure!(
        actual == info.sha256,
        "Downloaded extension checksum differs from the update response"
    );
    Ok(())
}

async fn clear_downloads(directory: &Path) -> Result<()> {
    if !directory.try_exists()? {
        return Ok(());
    }
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
    Ok(())
}

fn version_is_newer(left: &str, right: &str) -> bool {
    let parse = |value: &str| -> Vec<u64> {
        value
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    let mut left = parse(left);
    let mut right = parse(right);
    let length = left.len().max(right.len());
    left.resize(length, 0);
    right.resize(length, 0);
    left > right
}

fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn read_state(path: &Path) -> Result<State> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Read extension sync state {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error).with_context(|| format!("Read {}", path.display())),
    }
}

fn write_state(path: &Path, state: &State) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    store::atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Neutral values: production code never pins a version or checksum.
    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?><gupdate xmlns="http://www.google.com/update2/response" protocol="2.0" server="prod"><daystart elapsed_days="1" elapsed_seconds="2"/><app appid="bgnkhhnnamicmpeenaelnjfhikgbkllg" cohort="1::" cohortname="" status="ok"><updatecheck _esbAllowlist="true" codebase="https://clients2.googleusercontent.com/crx/blobs/example/EXAMPLE_2_7_13_41.crx" fp="1.00" hash_sha256="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" protected="0" size="1234567" status="ok" version="2.7.13.41"/></app></gupdate>"#;

    #[test]
    fn parses_the_official_update_response() {
        let info = parse_update_check(SAMPLE, "bgnkhhnnamicmpeenaelnjfhikgbkllg").unwrap();
        assert_eq!(info.version, "2.7.13.41");
        assert_eq!(info.size, 1_234_567);
        assert_eq!(
            info.sha256,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert!(info.url.ends_with(".crx"));
        assert!(
            update_check_url("dhdgffkkebhmkfjojejmpbldmpobfkfo", "158.0.8127.36")
                .contains("prodversion=158.0.8127.36")
        );
    }

    #[test]
    fn versions_compare_numerically_for_the_downgrade_guard() {
        assert!(version_is_newer("5.5.4.78", "5.5.4.9"));
        assert!(version_is_newer("6.0", "5.99.99.99"));
        assert!(!version_is_newer("5.5.4.78", "5.5.4.78"));
        assert!(!version_is_newer("5.5.4.78", "5.5.5.1"));
    }

    #[test]
    fn rejects_incomplete_or_foreign_update_responses() {
        assert!(parse_update_check(SAMPLE, "dhdgffkkebhmkfjojejmpbldmpobfkfo").is_err());
        assert!(parse_update_check(
            &SAMPLE.replace(r#"status="ok" version="2.7.13.41""#, r#"status="noupdate""#),
            "bgnkhhnnamicmpeenaelnjfhikgbkllg"
        )
        .is_err());
        assert!(parse_update_check(
            &SAMPLE.replace("clients2.googleusercontent.com", "example.test"),
            "bgnkhhnnamicmpeenaelnjfhikgbkllg"
        )
        .is_err());
        assert!(parse_update_check(
            &SAMPLE.replace(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "not-a-checksum"
            ),
            "bgnkhhnnamicmpeenaelnjfhikgbkllg"
        )
        .is_err());
        assert!(parse_update_check(
            &SAMPLE.replace(r#"size="1234567""#, r#"size="0""#),
            "bgnkhhnnamicmpeenaelnjfhikgbkllg"
        )
        .is_err());
    }
}
