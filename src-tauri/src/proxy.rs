use crate::{settings, store};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

const LATENCY_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LATENCY_TIMEOUT_ERROR: &str = "proxy latency test timed out after 5 seconds";
pub const TEST_CANCELLED_ERROR: &str = "proxy test cancelled because the proxy changed";
const BULK_TEST_CONCURRENCY: usize = 5;
const BACKGROUND_TEST_CONCURRENCY: usize = 3;
static TEST_CONCURRENCY: std::sync::OnceLock<tokio::sync::Semaphore> =
    std::sync::OnceLock::new();
static BACKGROUND_TEST_LIMIT: std::sync::OnceLock<tokio::sync::Semaphore> =
    std::sync::OnceLock::new();
static PROXY_TEST_REVISIONS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, tokio::sync::watch::Sender<u64>>>,
> = std::sync::OnceLock::new();
static PROXY_TEST_LOCKS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::OnceLock::new();
static MANUAL_TEST_COUNTS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, usize>>> =
    std::sync::OnceLock::new();
static MANUAL_TEST_EPOCH: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> =
    std::sync::OnceLock::new();
static PROXY_STORE_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());
static PROXY_HISTORY_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

#[derive(Clone, Copy)]
enum TestLane {
    Manual,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    Socks5,
    Http,
    Https,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyEntry {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// "PL", "US", …
    #[serde(default)]
    pub country: String,
    /// Free-form note.
    #[serde(default)]
    pub notes: String,
}

pub struct PreparedProxyTest {
    entry: ProxyEntry,
    revision: Option<u64>,
}

struct TestTicket {
    expected_revision: u64,
    receiver: tokio::sync::watch::Receiver<u64>,
}

struct ManualTestGuard {
    proxy_id: String,
}

impl Drop for ManualTestGuard {
    fn drop(&mut self) {
        let counts = MANUAL_TEST_COUNTS
            .get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let mut counts = counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remove = if let Some(count) = counts.get_mut(&self.proxy_id) {
            *count = count.saturating_sub(1);
            *count == 0
        } else {
            false
        };
        if remove {
            counts.remove(&self.proxy_id);
        }
        drop(counts);
        let sender = MANUAL_TEST_EPOCH.get_or_init(|| tokio::sync::watch::channel(0).0);
        let next = (*sender.borrow()).wrapping_add(1);
        sender.send_replace(next);
    }
}

impl TestTicket {
    fn is_cancelled(&self) -> bool {
        *self.receiver.borrow() != self.expected_revision
    }
}

fn revision_sender(proxy_id: &str) -> tokio::sync::watch::Sender<u64> {
    let revisions = PROXY_TEST_REVISIONS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut revisions = revisions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    revisions
        .entry(proxy_id.to_string())
        .or_insert_with(|| tokio::sync::watch::channel(0).0)
        .clone()
}

fn proxy_test_lock(proxy_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    if proxy_id.is_empty() {
        return std::sync::Arc::new(tokio::sync::Mutex::new(()));
    }
    let locks = PROXY_TEST_LOCKS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks
        .entry(proxy_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn begin_manual_test(proxy_id: &str) -> Option<ManualTestGuard> {
    if proxy_id.is_empty() {
        return None;
    }
    let counts = MANUAL_TEST_COUNTS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut counts = counts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *counts.entry(proxy_id.to_string()).or_insert(0) += 1;
    Some(ManualTestGuard {
        proxy_id: proxy_id.to_string(),
    })
}

fn manual_test_is_active(proxy_id: &str) -> bool {
    MANUAL_TEST_COUNTS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(proxy_id)
        .is_some_and(|count| *count > 0)
}

fn current_test_revision(proxy_id: &str) -> Option<u64> {
    if proxy_id.is_empty() {
        return None;
    }
    let sender = revision_sender(proxy_id);
    let revision = *sender.borrow();
    Some(revision)
}

fn invalidate_proxy_tests(proxy_id: &str) {
    if proxy_id.is_empty() {
        return;
    }
    let sender = revision_sender(proxy_id);
    let next = (*sender.borrow()).wrapping_add(1);
    sender.send_replace(next);
}

fn test_ticket(request: &PreparedProxyTest) -> Option<TestTicket> {
    let expected_revision = request.revision?;
    let sender = revision_sender(&request.entry.id);
    Some(TestTicket {
        expected_revision,
        receiver: sender.subscribe(),
    })
}

fn prepare_proxy_tests(entries: Vec<ProxyEntry>) -> Vec<PreparedProxyTest> {
    entries
        .into_iter()
        .map(|entry| PreparedProxyTest {
            revision: current_test_revision(&entry.id),
            entry,
        })
        .collect()
}

fn prepare_manual_test_request(request: &mut PreparedProxyTest) -> Result<()> {
    if request.entry.id.is_empty() {
        return Ok(());
    }
    // Resolve the latest durable entry while edits are excluded. This prevents
    // a just-closed editor's stale React row from testing old credentials.
    let _guard = PROXY_STORE_LOCK
        .read()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    let stored = load_unlocked()?
        .proxies
        .into_iter()
        .find(|proxy| proxy.id == request.entry.id);
    let current_revision = current_test_revision(&request.entry.id);
    if let Some(stored) = stored {
        request.entry = stored;
    } else if request.revision != current_revision {
        // A stored entry disappeared after this request was prepared. Do not
        // revive it by testing and recording the stale frontend copy.
        anyhow::bail!(TEST_CANCELLED_ERROR);
    }

    invalidate_proxy_tests(&request.entry.id);
    request.revision = current_test_revision(&request.entry.id);
    Ok(())
}

fn validate_background_test_request(request: &mut PreparedProxyTest) -> Result<()> {
    if request.entry.id.is_empty() {
        anyhow::bail!(TEST_CANCELLED_ERROR);
    }
    let _guard = PROXY_STORE_LOCK
        .read()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    if request.revision != current_test_revision(&request.entry.id) {
        anyhow::bail!(TEST_CANCELLED_ERROR);
    }
    request.entry = load_unlocked()?
        .proxies
        .into_iter()
        .find(|proxy| proxy.id == request.entry.id)
        .ok_or_else(|| anyhow::anyhow!(TEST_CANCELLED_ERROR))?;
    Ok(())
}

type ProxyDuplicateKey = (ProxyKind, String, u16, String);

fn normalized_host_key(host: &str) -> String {
    let host = host.trim();
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = unbracketed.parse::<std::net::IpAddr>() {
        return ip.to_string();
    }
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn duplicate_key(entry: &ProxyEntry) -> ProxyDuplicateKey {
    (
        entry.kind.clone(),
        normalized_host_key(&entry.host),
        entry.port,
        entry.username.clone(),
    )
}

fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() {
        anyhow::bail!("host is required");
    }
    if host.chars().any(|character| character.is_whitespace() || character.is_control()) {
        anyhow::bail!("host must not contain whitespace or control characters");
    }

    if host.starts_with('[') || host.ends_with(']') {
        let inner = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .ok_or_else(|| anyhow::anyhow!("IPv6 host must use matching brackets"))?;
        inner
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| anyhow::anyhow!("invalid IPv6 address"))?;
        return Ok(());
    }
    if host.contains('[') || host.contains(']') {
        anyhow::bail!("invalid host brackets");
    }
    if host.contains(':') {
        anyhow::bail!("IPv6 addresses must be enclosed in brackets");
    }
    url::Host::parse(host).map_err(|_| anyhow::anyhow!("invalid IP address or hostname"))?;
    Ok(())
}

fn normalize_entry(mut entry: ProxyEntry) -> Result<ProxyEntry> {
    entry.host = entry.host.trim().to_string();
    validate_host(&entry.host)?;
    if entry.port == 0 {
        anyhow::bail!("port must be between 1 and 65535");
    }
    if entry.username.chars().any(char::is_control)
        || entry.password.chars().any(char::is_control)
    {
        anyhow::bail!("credentials must not contain control characters");
    }
    if entry.username.is_empty() && !entry.password.is_empty() {
        anyhow::bail!("password requires a username");
    }
    if matches!(entry.kind, ProxyKind::Socks5)
        && (entry.username.as_bytes().len() > u8::MAX as usize
            || entry.password.as_bytes().len() > u8::MAX as usize)
    {
        anyhow::bail!("SOCKS5 username and password must each be at most 255 bytes");
    }

    entry.country = entry.country.trim().to_ascii_uppercase();
    if entry.country == "—" {
        entry.country.clear();
    }
    if !entry.country.is_empty()
        && (entry.country.len() != 2
            || !entry.country.bytes().all(|byte| byte.is_ascii_alphabetic()))
    {
        anyhow::bail!("country must be a two-letter code");
    }

    entry.name = entry.name.trim().to_string();
    if entry.name.is_empty() {
        entry.name = format!("{}:{}", entry.host, entry.port);
    }
    Ok(entry)
}

impl ProxyEntry {
    /// Build `--proxy-server=<scheme>://[user:pass@]host:port` for ShardX.
    pub fn to_proxy_server_arg(&self) -> String {
        let scheme = match self.kind {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
        };
        let host_port = format!("{}:{}", self.host, self.port);
        if self.username.is_empty() && self.password.is_empty() {
            format!("{scheme}://{host_port}")
        } else {
            let user = url::form_urlencoded::byte_serialize(self.username.as_bytes())
                .collect::<String>();
            let pass = url::form_urlencoded::byte_serialize(self.password.as_bytes())
                .collect::<String>();
            format!("{scheme}://{user}:{pass}@{host_port}")
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyStore {
    #[serde(default)]
    pub proxies: Vec<ProxyEntry>,
}

fn load_unlocked() -> Result<ProxyStore> {
    let path = store::proxies_path()?;
    if !path.exists() {
        return Ok(ProxyStore::default());
    }
    store::load_json_with_backup(&path)
}

pub fn load() -> Result<ProxyStore> {
    let _guard = PROXY_STORE_LOCK
        .read()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    load_unlocked()
}

fn save(s: &ProxyStore) -> Result<()> {
    let body = serde_json::to_string_pretty(s)?;
    store::atomic_write(&store::proxies_path()?, body.as_bytes())?;
    Ok(())
}

pub fn list() -> Result<Vec<ProxyEntry>> {
    Ok(load()?.proxies)
}

fn restore_store_snapshot_unlocked(snapshot: Option<&[u8]>) -> Result<()> {
    let path = store::proxies_path()?;
    match snapshot {
        Some(bytes) => store::atomic_write(&path, bytes)?,
        None if path.exists() => fs::remove_file(path)?,
        None => {}
    }
    Ok(())
}

pub fn upsert_with_status(
    mut entry: ProxyEntry,
) -> Result<(ProxyEntry, bool, bool, Option<PreparedProxyTest>)> {
    entry = normalize_entry(entry)?;
    let _resource_guard = crate::process::lock_profile_resources()?;
    if !entry.id.is_empty() {
        crate::profile::ensure_proxy_not_active(&entry.id)?;
    }
    let _guard = PROXY_STORE_LOCK
        .write()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    let mut s = load_unlocked()?;
    let key = duplicate_key(&entry);
    if s.proxies
        .iter()
        .any(|proxy| proxy.id != entry.id && duplicate_key(proxy) == key)
    {
        anyhow::bail!("proxy already exists with the same type, host, port, and username");
    }
    if entry.id.is_empty() {
        entry.id = uuid::Uuid::new_v4().to_string();
    }
    let existing_index = s.proxies.iter().position(|proxy| proxy.id == entry.id);
    let created = existing_index.is_none();
    let changed = existing_index
        .map(|index| s.proxies[index] != entry)
        .unwrap_or(true);

    if let Some(index) = existing_index {
        if changed {
            s.proxies[index] = entry.clone();
        }
    } else {
        s.proxies.push(entry.clone());
    }
    if changed {
        save(&s)?;
        if !created {
            // The edit is now durable. Wake and invalidate any test that was
            // prepared from the previous configuration before releasing the
            // short disk-write lock.
            invalidate_proxy_tests(&entry.id);
        }
    }
    let test = changed.then(|| PreparedProxyTest {
        revision: current_test_revision(&entry.id),
        entry: entry.clone(),
    });
    Ok((entry, created, changed, test))
}

fn insert_or_reuse_dedup(
    store_data: &mut ProxyStore,
    mut entry: ProxyEntry,
) -> Result<(ProxyEntry, bool)> {
    entry = normalize_entry(entry)?;
    let key = duplicate_key(&entry);
    if let Some(existing) = store_data
        .proxies
        .iter()
        .find(|proxy| duplicate_key(proxy) == key)
    {
        if existing.password != entry.password {
            anyhow::bail!(
                "proxy already exists with the same type, host, port, and username but a different password"
            );
        }
        return Ok((existing.clone(), false));
    }
    if entry.id.is_empty()
        || store_data
            .proxies
            .iter()
            .any(|proxy| proxy.id == entry.id)
    {
        entry.id = uuid::Uuid::new_v4().to_string();
    }
    store_data.proxies.push(entry.clone());
    Ok((entry, true))
}

/// Upsert that reuses an entry with the same endpoint and credentials.
pub fn upsert_dedup(entry: ProxyEntry) -> Result<ProxyEntry> {
    let _guard = PROXY_STORE_LOCK
        .write()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    let mut store_data = load_unlocked()?;
    let (stored, created) = insert_or_reuse_dedup(&mut store_data, entry)?;
    if created {
        save(&store_data)?;
    }
    Ok(stored)
}

/// Resolve and persist all proxy bindings for a complete-profile restore as
/// one transaction. Holding the proxy write lock through `commit` prevents a
/// failed restore from rolling back an unrelated concurrent proxy mutation.
pub fn with_restored_proxy_bindings<T>(
    entries: Vec<Option<ProxyEntry>>,
    commit: impl FnOnce(Vec<Option<String>>) -> Result<T>,
) -> Result<T> {
    let _guard = PROXY_STORE_LOCK
        .write()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    let path = store::proxies_path()?;
    let snapshot = if path.exists() {
        Some(fs::read(&path)?)
    } else {
        None
    };
    let mut store_data = load_unlocked()?;
    let mut changed = false;
    let mut binding_ids = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry {
            Some(entry) => {
                let (stored, created) = insert_or_reuse_dedup(&mut store_data, entry)?;
                changed |= created;
                binding_ids.push(Some(stored.id));
            }
            None => binding_ids.push(None),
        }
    }

    if changed {
        if let Err(error) = save(&store_data) {
            return match restore_store_snapshot_unlocked(snapshot.as_deref()) {
                Ok(()) => Err(error),
                Err(rollback) => Err(anyhow::anyhow!(
                    "{error}; proxy rollback was incomplete: {rollback}"
                )),
            };
        }
    }

    match commit(binding_ids) {
        Ok(value) => Ok(value),
        Err(error) if changed => match restore_store_snapshot_unlocked(snapshot.as_deref()) {
            Ok(()) => Err(error),
            Err(rollback) => Err(anyhow::anyhow!(
                "{error}; proxy rollback was incomplete: {rollback}"
            )),
        },
        Err(error) => Err(error),
    }
}

pub fn delete(id: &str) -> Result<()> {
    let _resource_guard = crate::process::lock_profile_resources()?;
    crate::profile::ensure_proxy_not_active(id)?;
    // Every operation that needs both files acquires history before store.
    let _history_guard = PROXY_HISTORY_LOCK
        .write()
        .map_err(|_| anyhow::anyhow!("proxy history lock poisoned"))?;
    let _store_guard = PROXY_STORE_LOCK
        .write()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    let mut s = load_unlocked()?;
    s.proxies.retain(|p| p.id != id);
    save(&s)?;
    invalidate_proxy_tests(id);
    // Also wipe persisted test history.
    let mut hs = load_history_unlocked()?;
    if hs.by_proxy.remove(id).is_some() {
        save_history(&hs)?;
    }
    Ok(())
}

pub fn get(id: &str) -> Result<Option<ProxyEntry>> {
    Ok(load()?.proxies.into_iter().find(|p| p.id == id))
}

/// SOCKS5/HTTP CONNECT probe; returns RTT in ms on success.
pub async fn probe(entry: &ProxyEntry) -> Result<u128> {
    tokio::time::timeout(LATENCY_TEST_TIMEOUT, probe_inner(entry))
        .await
        .context(LATENCY_TIMEOUT_ERROR)?
}

async fn probe_inner(entry: &ProxyEntry) -> Result<u128> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::time::Instant;

    let started = Instant::now();
    let addr = format!("{}:{}", entry.host, entry.port);
    let mut stream = TcpStream::connect(&addr).await.context("connect failed")?;

    match entry.kind {
        ProxyKind::Socks5 => {
            // RFC 1928 §3 greeting
            let auth_method: u8 = if entry.username.is_empty() { 0x00 } else { 0x02 };
            stream.write_all(&[0x05, 0x01, auth_method]).await?;
            let mut resp = [0u8; 2];
            stream.read_exact(&mut resp).await?;
            if resp[0] != 0x05 {
                anyhow::bail!("not SOCKS5");
            }
            if resp[1] == 0xFF {
                anyhow::bail!("no acceptable auth method");
            }
            if auth_method == 0x02 {
                // RFC 1929 user/pass sub-negotiation
                let mut buf = vec![0x01u8];
                buf.push(entry.username.len() as u8);
                buf.extend_from_slice(entry.username.as_bytes());
                buf.push(entry.password.len() as u8);
                buf.extend_from_slice(entry.password.as_bytes());
                stream.write_all(&buf).await?;
                let mut auth_resp = [0u8; 2];
                stream.read_exact(&mut auth_resp).await?;
                if auth_resp[1] != 0x00 {
                    anyhow::bail!("auth failed");
                }
            }
        }
        ProxyKind::Http | ProxyKind::Https => {
            // CONNECT with Basic auth; read until CRLFCRLF to avoid clipping headers.
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            let mut req = String::from(
                "CONNECT example.com:443 HTTP/1.1\r\n\
                 Host: example.com:443\r\n",
            );
            if !entry.username.is_empty() || !entry.password.is_empty() {
                let creds = format!("{}:{}", entry.username, entry.password);
                let encoded = STANDARD.encode(creds.as_bytes());
                req.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
            }
            req.push_str("Proxy-Connection: keep-alive\r\n\r\n");
            stream.write_all(req.as_bytes()).await?;

            // Read until CRLFCRLF or 4 KB cap.
            let mut buf = Vec::with_capacity(512);
            let mut tmp = [0u8; 256];
            let head: String = loop {
                let n = stream.read(&mut tmp).await?;
                if n == 0 { break String::from_utf8_lossy(&buf).to_string(); }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 4096 {
                    break String::from_utf8_lossy(&buf).to_string();
                }
            };
            let first_line = head.lines().next().unwrap_or("");
            if !first_line.starts_with("HTTP/1.1 200") && !first_line.starts_with("HTTP/1.0 200") {
                anyhow::bail!("CONNECT failed: {first_line}");
            }
        }
    }
    Ok(started.elapsed().as_millis())
}

// ---- Bulk import ----
//
// Accepted: socks5://user:pass@host:port, user:pass@host:port,
//           host:port:user:pass, host:port. `#` lines and trailing `# country=X note=Y`
//           supported. SOCKS5 default kind when scheme missing.

#[derive(Debug, Clone, Serialize)]
pub struct BulkParseIssue {
    pub line: usize,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BulkParsePreview {
    pub entries: Vec<ProxyEntry>,
    pub invalid: Vec<BulkParseIssue>,
    pub duplicate_lines: usize,
    pub existing_duplicates: usize,
}

/// Parse a single proxy line for inline (unsaved) use by the API.
pub fn parse_single(line: &str) -> Option<ProxyEntry> {
    parse_one_checked(line.trim(), &ProxyKind::Socks5).ok()
}

fn parse_bulk_against(
    text: &str,
    default_kind: ProxyKind,
    existing: &[ProxyEntry],
) -> BulkParsePreview {
    let existing_keys: HashSet<ProxyDuplicateKey> = existing.iter().map(duplicate_key).collect();
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    let mut invalid = Vec::new();
    let mut duplicate_lines = 0usize;
    let mut existing_duplicates = 0usize;

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_one_checked(line, &default_kind) {
            Ok(entry) => {
                let key = duplicate_key(&entry);
                if existing_keys.contains(&key) {
                    existing_duplicates += 1;
                } else if !seen.insert(key) {
                    duplicate_lines += 1;
                } else {
                    entries.push(entry);
                }
            }
            Err(error) => invalid.push(BulkParseIssue {
                line: index + 1,
                reason: error.to_string(),
            }),
        }
    }

    BulkParsePreview {
        entries,
        invalid,
        duplicate_lines,
        existing_duplicates,
    }
}

pub fn preview_bulk(text: &str, default_kind: ProxyKind) -> Result<BulkParsePreview> {
    let existing = load()?.proxies;
    Ok(parse_bulk_against(text, default_kind, &existing))
}

pub fn parse_bulk_strict(text: &str, default_kind: ProxyKind) -> Result<Vec<ProxyEntry>> {
    let preview = parse_bulk_against(text, default_kind, &[]);
    if !preview.invalid.is_empty() {
        let details = preview
            .invalid
            .iter()
            .take(5)
            .map(|issue| format!("line {}: {}", issue.line, issue.reason))
            .collect::<Vec<_>>()
            .join("; ");
        let remaining = preview.invalid.len().saturating_sub(5);
        if remaining > 0 {
            anyhow::bail!("invalid proxy lines: {details}; and {remaining} more");
        }
        anyhow::bail!("invalid proxy lines: {details}");
    }
    if preview.entries.is_empty() {
        anyhow::bail!("no valid proxy lines found");
    }
    Ok(preview.entries)
}

fn parse_one_checked(line: &str, default_kind: &ProxyKind) -> Result<ProxyEntry> {
    // Optional trailing `# country=US note=foo`.
    let (main, comment) = match line.find('#') {
        Some(i) => (line[..i].trim(), Some(line[i + 1..].trim())),
        None => (line, None),
    };
    if main.is_empty() {
        anyhow::bail!("proxy endpoint is empty");
    }
    let (kind, rest) = if let Some(r) = main.strip_prefix("socks5://") {
        (ProxyKind::Socks5, r)
    } else if let Some(r) = main.strip_prefix("https://") {
        (ProxyKind::Https, r)
    } else if let Some(r) = main.strip_prefix("http://") {
        (ProxyKind::Http, r)
    } else if main.contains("://") {
        anyhow::bail!("unsupported proxy scheme");
    } else {
        (default_kind.clone(), main)
    };

    let (host_part, user, pass) = if let Some((u, hp)) = rest.rsplit_once('@') {
        if u.contains('@') {
            anyhow::bail!("credentials contain an unescaped @ character");
        }
        let (un, pw) = u.split_once(':').unwrap_or((u, ""));
        (hp.to_string(), un.to_string(), pw.to_string())
    } else if rest.starts_with('[') {
        (rest.to_string(), String::new(), String::new())
    } else {
        // host:port or host:port:user:pass
        let parts: Vec<&str> = rest.split(':').collect();
        match parts.len() {
            2 => (rest.to_string(), String::new(), String::new()),
            4 => (
                format!("{}:{}", parts[0], parts[1]),
                parts[2].to_string(),
                parts[3].to_string(),
            ),
            _ => (rest.to_string(), String::new(), String::new()),
        }
    };

    let (host, port_s) = host_part
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("missing port"))?;
    let port: u16 = port_s
        .parse()
        .map_err(|_| anyhow::anyhow!("port must be between 1 and 65535"))?;
    let mut country = String::new();
    let mut notes = String::new();
    if let Some(c) = comment {
        for kv in c.split_whitespace() {
            if let Some(v) = kv.strip_prefix("country=") {
                country = v.to_string();
            } else if let Some(v) = kv.strip_prefix("note=") {
                notes = v.to_string();
            }
        }
    }
    normalize_entry(ProxyEntry {
        // ID assigned now so pre-save test snapshots key under the kept uuid.
        id: uuid::Uuid::new_v4().to_string(),
        name: format!("{host}:{port}"),
        kind,
        host: host.to_string(),
        port,
        username: user,
        password: pass,
        country,
        notes,
    })
}

/// Save many entries; validates the whole batch, then returns the entries that
/// were actually added so callers can schedule post-save work for only those.
pub fn bulk_save_with_entries(
    entries: Vec<ProxyEntry>,
) -> Result<(Vec<ProxyEntry>, Vec<PreparedProxyTest>)> {
    let entries = entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            normalize_entry(entry).with_context(|| format!("entry {} is invalid", index + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    let _guard = PROXY_STORE_LOCK
        .write()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    let mut store_data = load_unlocked()?;
    let mut seen: HashSet<ProxyDuplicateKey> =
        store_data.proxies.iter().map(duplicate_key).collect();
    let mut added = Vec::new();
    for mut e in entries {
        if !seen.insert(duplicate_key(&e)) {
            continue;
        }
        if e.id.is_empty() || store_data.proxies.iter().any(|proxy| proxy.id == e.id) {
            e.id = uuid::Uuid::new_v4().to_string();
        }
        store_data.proxies.push(e.clone());
        added.push(e);
    }
    save(&store_data)?;
    let tests = prepare_proxy_tests(added.clone());
    Ok((added, tests))
}

pub fn prepare_all_proxy_tests() -> Result<Vec<PreparedProxyTest>> {
    let _guard = PROXY_STORE_LOCK
        .read()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    Ok(prepare_proxy_tests(load_unlocked()?.proxies))
}

#[cfg(test)]
mod bulk_import_tests {
    use super::*;

    fn entry(kind: ProxyKind, host: &str, port: u16, username: &str) -> ProxyEntry {
        ProxyEntry {
            id: String::new(),
            name: String::new(),
            kind,
            host: host.to_string(),
            port,
            username: username.to_string(),
            password: String::new(),
            country: String::new(),
            notes: String::new(),
        }
    }

    #[test]
    fn validation_rejects_invalid_endpoints() {
        assert!(normalize_entry(entry(ProxyKind::Socks5, "", 1080, "")).is_err());
        assert!(normalize_entry(entry(ProxyKind::Socks5, "bad host", 1080, "")).is_err());
        assert!(normalize_entry(entry(ProxyKind::Socks5, "127.0.0.1", 0, "")).is_err());

        let mut password_without_user = entry(ProxyKind::Http, "proxy.example", 8080, "");
        password_without_user.password = "secret".to_string();
        assert!(normalize_entry(password_without_user).is_err());
    }

    #[test]
    fn parser_accepts_bracketed_ipv6() {
        let parsed = parse_one_checked("socks5://[2001:db8::1]:1080", &ProxyKind::Socks5)
            .expect("bracketed IPv6 should parse");
        assert_eq!(parsed.host, "[2001:db8::1]");
        assert_eq!(parsed.port, 1080);
    }

    #[test]
    fn preview_reports_invalid_lines_and_removes_duplicates() {
        let existing = vec![entry(ProxyKind::Socks5, "existing.example", 1080, "user")];
        let preview = parse_bulk_against(
            "socks5://user:pass@existing.example:1080\n\
             socks5://user:pass@new.example:1080\n\
             socks5://user:other@NEW.EXAMPLE:1080\n\
             socks4://unsupported.example:1080\n\
             missing-port.example\n\
             new.example:0",
            ProxyKind::Socks5,
            &existing,
        );

        assert_eq!(preview.entries.len(), 1);
        assert_eq!(preview.existing_duplicates, 1);
        assert_eq!(preview.duplicate_lines, 1);
        assert_eq!(preview.invalid.len(), 3);
        assert_eq!(preview.invalid[0].line, 4);
    }

    #[test]
    fn duplicate_key_keeps_distinct_proxy_protocols() {
        let socks = entry(ProxyKind::Socks5, "Proxy.Example.", 1080, "user");
        let http = entry(ProxyKind::Http, "proxy.example", 1080, "user");
        assert_ne!(duplicate_key(&socks), duplicate_key(&http));
    }

    #[test]
    fn restored_proxy_dedup_reuses_matching_credentials() {
        let mut store_data = ProxyStore::default();
        let mut first = entry(ProxyKind::Socks5, "Proxy.Example.", 1080, "user");
        first.id = "source-proxy".to_string();
        first.password = "secret".to_string();
        let (stored, created) = insert_or_reuse_dedup(&mut store_data, first).unwrap();
        assert!(created);

        let mut duplicate = entry(ProxyKind::Socks5, "proxy.example", 1080, "user");
        duplicate.id = "other-source-proxy".to_string();
        duplicate.password = "secret".to_string();
        let (reused, created) = insert_or_reuse_dedup(&mut store_data, duplicate).unwrap();

        assert!(!created);
        assert_eq!(reused.id, stored.id);
        assert_eq!(store_data.proxies.len(), 1);
    }

    #[test]
    fn restored_proxy_dedup_rejects_password_conflicts() {
        let mut store_data = ProxyStore::default();
        let mut first = entry(ProxyKind::Http, "proxy.example", 8080, "user");
        first.password = "first".to_string();
        insert_or_reuse_dedup(&mut store_data, first).unwrap();

        let mut conflict = entry(ProxyKind::Http, "PROXY.EXAMPLE.", 8080, "user");
        conflict.password = "second".to_string();
        assert!(insert_or_reuse_dedup(&mut store_data, conflict).is_err());
        assert_eq!(store_data.proxies.len(), 1);
    }

    #[test]
    fn prepared_test_is_cancelled_when_proxy_revision_changes() {
        let mut proxy = entry(ProxyKind::Socks5, "proxy.example", 1080, "user");
        proxy.id = uuid::Uuid::new_v4().to_string();
        let request = prepare_proxy_tests(vec![proxy]).pop().unwrap();
        let ticket = test_ticket(&request).unwrap();

        assert!(!ticket.is_cancelled());
        invalidate_proxy_tests(&request.entry.id);
        assert!(ticket.is_cancelled());
    }

    #[test]
    fn geo_provider_order_starts_at_preferred_and_wraps() {
        assert_eq!(
            geo_provider_order("country.is"),
            vec![
                "country.is",
                "bigdatacloud.com",
                "freeipapi.com",
                "ipapi.is",
                "ipwho.is",
                "geojs.io",
            ]
        );
    }

    #[test]
    fn unknown_geo_provider_uses_default_order() {
        assert_eq!(geo_provider_order("removed.example"), GEO_PROVIDERS.to_vec());
    }
}

// ---- UDP probe (SOCKS5 UDP_ASSOCIATE; RFC 1928 §7) ----

/// Resolve a public STUN server to IPv4 (probe target for the UDP relay).
async fn resolve_stun_ipv4() -> Result<(std::net::Ipv4Addr, u16)> {
    const HOSTS: &[&str] = &[
        "stun.l.google.com:19302",
        "stun1.l.google.com:19302",
        "stun.cloudflare.com:3478",
    ];
    for h in HOSTS {
        if let Ok(addrs) = tokio::net::lookup_host(*h).await {
            for a in addrs {
                if let std::net::IpAddr::V4(v4) = a.ip() {
                    return Ok((v4, a.port()));
                }
            }
        }
    }
    anyhow::bail!("no STUN server resolved to IPv4")
}

pub async fn probe_udp(entry: &ProxyEntry) -> Result<u128> {
    tokio::time::timeout(LATENCY_TEST_TIMEOUT, probe_udp_inner(entry))
        .await
        .context(LATENCY_TIMEOUT_ERROR)?
}

async fn probe_udp_inner(entry: &ProxyEntry) -> Result<u128> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UdpSocket};
    use tokio::time::Instant;

    if !matches!(entry.kind, ProxyKind::Socks5) {
        anyhow::bail!("UDP probe only supported for SOCKS5");
    }
    let started = Instant::now();
    let mut tcp = TcpStream::connect(format!("{}:{}", entry.host, entry.port))
        .await
        .context("connect failed")?;

    let auth_method: u8 = if entry.username.is_empty() { 0x00 } else { 0x02 };
    tcp.write_all(&[0x05, 0x01, auth_method]).await?;
    let mut greet = [0u8; 2];
    tcp.read_exact(&mut greet).await?;
    if greet[1] == 0xFF {
        anyhow::bail!("no acceptable auth method");
    }
    if auth_method == 0x02 {
        let mut buf = vec![0x01u8];
        buf.push(entry.username.len() as u8);
        buf.extend_from_slice(entry.username.as_bytes());
        buf.push(entry.password.len() as u8);
        buf.extend_from_slice(entry.password.as_bytes());
        tcp.write_all(&buf).await?;
        let mut ar = [0u8; 2];
        tcp.read_exact(&mut ar).await?;
        if ar[1] != 0x00 {
            anyhow::bail!("auth failed");
        }
    }
    // UDP_ASSOCIATE: cmd=0x03, ATYP=IPv4, addr=0.0.0.0, port=0
    tcp.write_all(&[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    let mut hdr = [0u8; 4];
    tcp.read_exact(&mut hdr).await?;
    if hdr[1] != 0x00 {
        anyhow::bail!("UDP_ASSOCIATE refused (rep={:#x})", hdr[1]);
    }
    let bind_addr: SocketAddr = match hdr[3] {
        0x01 => {
            // IPv4
            let mut ip = [0u8; 4];
            tcp.read_exact(&mut ip).await?;
            let mut p = [0u8; 2];
            tcp.read_exact(&mut p).await?;
            let port = u16::from_be_bytes(p);
            let v4 = std::net::Ipv4Addr::from(ip);
            // 0.0.0.0 → fall back to TCP peer (where the relay lives).
            if v4.is_unspecified() {
                let peer = tcp.peer_addr()?;
                SocketAddr::new(peer.ip(), port)
            } else {
                SocketAddr::new(std::net::IpAddr::V4(v4), port)
            }
        }
        0x04 => {
            let mut ip = [0u8; 16];
            tcp.read_exact(&mut ip).await?;
            let mut p = [0u8; 2];
            tcp.read_exact(&mut p).await?;
            SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::from(ip)), u16::from_be_bytes(p))
        }
        _ => anyhow::bail!("unsupported ATYP in UDP reply"),
    };

    // Probe with STUN binding request (DNS-port-53 often blocked, STUN passes).
    let (stun_ip, stun_port) = resolve_stun_ipv4()
        .await
        .context("could not resolve a STUN server to probe UDP with")?;

    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    udp.connect(bind_addr).await?;
    let mut pkt: Vec<u8> = Vec::with_capacity(32);
    // SOCKS5 UDP header: RSV(2)=0, FRAG=0, ATYP=IPv4, DST=<stun>, PORT.
    pkt.extend_from_slice(&[0, 0, 0, 0x01]);
    pkt.extend_from_slice(&stun_ip.octets());
    pkt.extend_from_slice(&stun_port.to_be_bytes());
    // STUN Binding Request (RFC 5389): type=0x0001, magic 0x2112A442, 12B txid.
    let mut stun = vec![0x00u8, 0x01, 0x00, 0x00, 0x21, 0x12, 0xA4, 0x42];
    stun.extend_from_slice(&uuid::Uuid::new_v4().as_bytes()[..12]);
    pkt.extend_from_slice(&stun);
    udp.send(&pkt).await?;

    let mut buf = vec![0u8; 1500];
    let n = udp.recv(&mut buf).await?;
    if n < 20 {
        anyhow::bail!("UDP reply too short");
    }
    // RFC 1928: dropping TCP control tears down the relay; keep it alive.
    drop(tcp);
    Ok(started.elapsed().as_millis())
}

// ---- Geo lookup ----

#[derive(Debug, Clone, Serialize)]
pub struct GeoInfo {
    pub ip: String,
    pub country: String,
    /// ISO 3166-1 alpha-2.
    pub country_code: String,
    pub region: String,
    pub city: String,
    pub isp: String,
    pub timezone: String,
    pub latitude: f64,
    pub longitude: f64,
    pub provider: String,
}

/// Probe IP/country the world sees when traffic exits the proxy.
pub async fn geo_check(entry: &ProxyEntry, provider_override: Option<String>) -> Result<GeoInfo> {
    geo_check_via(Some(entry), provider_override).await
}

const GEO_PROVIDERS: [&str; 6] = [
    "ipwho.is",
    "geojs.io",
    "country.is",
    "bigdatacloud.com",
    "freeipapi.com",
    "ipapi.is",
];
const GEO_PROVIDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn normalize_geo_provider(provider: &str) -> &'static str {
    GEO_PROVIDERS
        .iter()
        .copied()
        .find(|candidate| *candidate == provider)
        .unwrap_or(GEO_PROVIDERS[0])
}

/// Return every provider once, starting at the preferred provider and wrapping
/// around the Settings order.
fn geo_provider_order(preferred: &str) -> Vec<&'static str> {
    let preferred = normalize_geo_provider(preferred);
    let start = GEO_PROVIDERS
        .iter()
        .position(|provider| *provider == preferred)
        .unwrap_or(0);
    GEO_PROVIDERS
        .iter()
        .cycle()
        .skip(start)
        .take(GEO_PROVIDERS.len())
        .copied()
        .collect()
}

fn geo_error_allows_fallback(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|request_error| {
                request_error.is_timeout()
                    || request_error.is_decode()
                    || request_error.status() == Some(reqwest::StatusCode::TOO_MANY_REQUESTS)
            })
    })
}

/// Probe geo through `entry` if Some, else direct. The configured provider is
/// preferred; timeout, HTTP 429, and JSON decode failures advance to the next
/// provider in Settings order.
pub async fn geo_check_via(entry: Option<&ProxyEntry>, provider_override: Option<String>) -> Result<GeoInfo> {
    let requested_provider = provider_override
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| settings::load().ok().and_then(|s| s.geo_checker).unwrap_or_else(|| "ipwho.is".into()));
    let preferred = normalize_geo_provider(&requested_provider);
    let mut fallback_errors = Vec::new();

    for provider in geo_provider_order(preferred) {
        match geo_check_provider(entry, provider.to_string()).await {
            Ok(info) => {
                if provider != preferred {
                    eprintln!(
                        "[launcher] geo provider fallback succeeded: preferred={preferred}, actual={provider}"
                    );
                }
                return Ok(info);
            }
            Err(error) if geo_error_allows_fallback(&error) => {
                eprintln!(
                    "[launcher] geo provider {provider} timed out, returned HTTP 429, or sent invalid JSON; trying next: {error}"
                );
                fallback_errors.push(format!("{provider}: {error}"));
            }
            Err(error) => {
                return Err(error.context(format!("geo provider {provider} failed")));
            }
        }
    }

    anyhow::bail!(
        "all geo providers failed after starting with {preferred}: {}",
        fallback_errors.join("; ")
    )
}

async fn geo_check_provider(entry: Option<&ProxyEntry>, provider: String) -> Result<GeoInfo> {

    let url = match provider.as_str() {
        "ipwho.is" => "https://ipwho.is/",
        "geojs.io" => "https://get.geojs.io/v1/ip/geo.json",
        "country.is" => "https://api.country.is/?fields=city,subdivision,location,asn",
        "bigdatacloud.com" => "https://api.bigdatacloud.net/data/reverse-geocode-client?localityLanguage=en",
        "freeipapi.com" => "https://free.freeipapi.com/api/json",
        "ipapi.is" => "https://api.ipapi.is/",
        _ => unreachable!("provider is normalized above"),
    };

    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("ShardX Launcher/", env!("CARGO_PKG_VERSION")))
        .timeout(GEO_PROVIDER_TIMEOUT);
    if let Some(entry) = entry {
        let scheme = match entry.kind {
            ProxyKind::Socks5 => "socks5h", // DNS via proxy
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
        };
        let proxy_url = if entry.username.is_empty() && entry.password.is_empty() {
            format!("{scheme}://{}:{}", entry.host, entry.port)
        } else {
            let user = url::form_urlencoded::byte_serialize(entry.username.as_bytes()).collect::<String>();
            let pass = url::form_urlencoded::byte_serialize(entry.password.as_bytes()).collect::<String>();
            format!("{scheme}://{user}:{pass}@{}:{}", entry.host, entry.port)
        };
        let proxy = reqwest::Proxy::all(&proxy_url).context("bad proxy URL")?;
        builder = builder.proxy(proxy);
    } else {
        // Direct check: bypass any system proxy.
        builder = builder.no_proxy();
    }
    let client = builder.build()?;

    let body: serde_json::Value = client.get(url).send().await?.error_for_status()?.json().await?;
    let client_info: Option<serde_json::Value> = if provider == "bigdatacloud.com" {
        Some(
            client
                .get("https://api.bigdatacloud.net/data/client-info")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?,
        )
    } else {
        None
    };

    let s = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    let f = |v: &serde_json::Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse::<f64>().ok())))
            .unwrap_or(0.0)
    };
    let path_s = |v: &serde_json::Value, path: &[&str]| {
        let mut current = v;
        for key in path {
            let Some(next) = current.get(*key) else {
                return String::new();
            };
            current = next;
        }
        current.as_str().unwrap_or("").to_string()
    };
    let path_f = |v: &serde_json::Value, path: &[&str]| {
        let mut current = v;
        for key in path {
            let Some(next) = current.get(*key) else {
                return 0.0;
            };
            current = next;
        }
        current
            .as_f64()
            .or_else(|| current.as_str().and_then(|s| s.parse::<f64>().ok()))
            .unwrap_or(0.0)
    };

    let info = match provider.as_str() {
        "ipwho.is" => {
            if body.get("success").and_then(|v| v.as_bool()) == Some(false) {
                anyhow::bail!("ipwho.is: {}", s(&body, "message"));
            }
            GeoInfo {
                ip: s(&body, "ip"),
                country: s(&body, "country"),
                country_code: s(&body, "country_code"),
                region: s(&body, "region"),
                city: s(&body, "city"),
                isp: path_s(&body, &["connection", "isp"]),
                timezone: path_s(&body, &["timezone", "id"]),
                latitude: f(&body, "latitude"),
                longitude: f(&body, "longitude"),
                provider,
            }
        }
        "geojs.io" => GeoInfo {
            ip: s(&body, "ip"),
            country: s(&body, "country"),
            country_code: s(&body, "country_code"),
            region: s(&body, "region"),
            city: s(&body, "city"),
            isp: s(&body, "organization"),
            timezone: s(&body, "timezone"),
            latitude: f(&body, "latitude"),
            longitude: f(&body, "longitude"),
            provider,
        },
        "country.is" => {
            let country_code = s(&body, "country");
            GeoInfo {
                ip: s(&body, "ip"),
                country: country_code.clone(),
                country_code,
                region: s(&body, "subdivision"),
                city: s(&body, "city"),
                isp: path_s(&body, &["asn", "organization"]),
                timezone: path_s(&body, &["location", "time_zone"]),
                latitude: path_f(&body, &["location", "latitude"]),
                longitude: path_f(&body, &["location", "longitude"]),
                provider,
            }
        }
        "bigdatacloud.com" => {
            let timezone = body
                .pointer("/localityInfo/informative")
                .and_then(|v| v.as_array())
                .and_then(|items| {
                    items.iter().find(|item| {
                        item.get("description")
                            .and_then(|v| v.as_str())
                            .map(|description| {
                                description.eq_ignore_ascii_case("time zone")
                                    || description.eq_ignore_ascii_case("timezone")
                            })
                            .unwrap_or(false)
                    })
                })
                .and_then(|item| item.get("name").or_else(|| item.get("isoName")))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let city = {
                let city = s(&body, "city");
                if city.is_empty() { s(&body, "locality") } else { city }
            };
            GeoInfo {
                ip: client_info.as_ref().map(|v| s(v, "ipString")).unwrap_or_default(),
                country: s(&body, "countryName"),
                country_code: s(&body, "countryCode"),
                region: s(&body, "principalSubdivision"),
                city,
                isp: String::new(),
                timezone,
                latitude: f(&body, "latitude"),
                longitude: f(&body, "longitude"),
                provider,
            }
        }
        "freeipapi.com" => {
            // FreeIPAPI returns every timezone used by the country, not an
            // IP-specific zone. Only expose it when the country has one zone.
            let timezone = body
                .get("timeZones")
                .and_then(|v| v.as_array())
                .filter(|zones| zones.len() == 1)
                .and_then(|zones| zones.first())
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            GeoInfo {
                ip: s(&body, "ipAddress"),
                country: s(&body, "countryName"),
                country_code: s(&body, "countryCode"),
                region: s(&body, "regionName"),
                city: s(&body, "cityName"),
                isp: s(&body, "asnOrganization"),
                timezone,
                latitude: f(&body, "latitude"),
                longitude: f(&body, "longitude"),
                provider,
            }
        }
        "ipapi.is" => {
            if let Some(error) = body.get("error").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                anyhow::bail!("ipapi.is: {error}");
            }
            let company = path_s(&body, &["company", "name"]);
            let isp = if company.is_empty() { path_s(&body, &["asn", "org"]) } else { company };
            GeoInfo {
                ip: s(&body, "ip"),
                country: path_s(&body, &["location", "country"]),
                country_code: path_s(&body, &["location", "country_code"]),
                region: path_s(&body, &["location", "state"]),
                city: path_s(&body, &["location", "city"]),
                isp,
                timezone: path_s(&body, &["location", "timezone"]),
                latitude: path_f(&body, &["location", "latitude"]),
                longitude: path_f(&body, &["location", "longitude"]),
                provider,
            }
        }
        _ => unreachable!("provider is normalized above"),
    };
    Ok(info)
}

/// Map ISO-3166 alpha-2 to BCP-47 locale (coarse).
pub fn country_to_locale(cc: &str) -> &'static str {
    match cc.to_ascii_uppercase().as_str() {
        "US" => "en-US",
        "GB" | "UK" => "en-GB",
        "CA" => "en-CA",
        "AU" => "en-AU",
        "NZ" => "en-NZ",
        "IE" => "en-IE",
        "ZA" => "en-ZA",
        "IN" => "en-IN",
        "DE" => "de-DE",
        "AT" => "de-AT",
        "CH" => "de-CH",
        "FR" => "fr-FR",
        "BE" => "fr-BE",
        "ES" => "es-ES",
        "MX" => "es-MX",
        "AR" => "es-AR",
        "CO" => "es-CO",
        "CL" => "es-CL",
        "IT" => "it-IT",
        "NL" => "nl-NL",
        "PL" => "pl-PL",
        "BR" => "pt-BR",
        "PT" => "pt-PT",
        "RO" => "ro-RO",
        "RU" => "ru-RU",
        "BY" => "be-BY",
        "UA" => "uk-UA",
        "TR" => "tr-TR",
        "GR" => "el-GR",
        "CZ" => "cs-CZ",
        "SK" => "sk-SK",
        "HU" => "hu-HU",
        "SE" => "sv-SE",
        "FI" => "fi-FI",
        "NO" => "nb-NO",
        "DK" => "da-DK",
        "BG" => "bg-BG",
        "HR" => "hr-HR",
        "SI" => "sl-SI",
        "RS" => "sr-RS",
        "IL" => "he-IL",
        "SA" | "AE" | "EG" => "ar-SA",
        "ID" => "id-ID",
        "MY" => "ms-MY",
        "PH" => "fil-PH",
        "VN" => "vi-VN",
        "TH" => "th-TH",
        "CN" => "zh-CN",
        "HK" => "zh-HK",
        "TW" => "zh-TW",
        "JP" => "ja-JP",
        "KR" => "ko-KR",
        _ => "en-US",
    }
}

// ---- Test history ----

/// One observation of a proxy's exit state; same-IP consecutive entries collapse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSnapshot {
    pub first_seen: String,
    pub last_seen: String,
    pub ip: String,
    pub country_code: String,
    pub country: String,
    pub region: String,
    pub city: String,
    pub isp: String,
    pub timezone: String,
    pub latitude: f64,
    pub longitude: f64,
    pub tcp_ms: Option<u128>,
    pub udp_ms: Option<u128>,
    pub udp_error: Option<String>,
    pub provider: String,
}

#[derive(Debug, Serialize)]
pub struct BatchTestResult {
    pub index: usize,
    pub snapshot: Option<TestSnapshot>,
    pub error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HistoryStore {
    #[serde(default)]
    by_proxy: HashMap<String, Vec<TestSnapshot>>,
}

fn history_path() -> Result<PathBuf> {
    Ok(store::config_root()?.join("proxies-history.json"))
}

fn load_history_unlocked() -> Result<HistoryStore> {
    let path = history_path()?;
    if !path.exists() {
        return Ok(HistoryStore::default());
    }
    store::load_json_with_backup(&path)
}

fn load_history() -> Result<HistoryStore> {
    let _guard = PROXY_HISTORY_LOCK
        .read()
        .map_err(|_| anyhow::anyhow!("proxy history lock poisoned"))?;
    load_history_unlocked()
}

fn save_history(s: &HistoryStore) -> Result<()> {
    let body = serde_json::to_string_pretty(s)?;
    store::atomic_write(&history_path()?, body.as_bytes())?;
    Ok(())
}

/// Persist a test result; same-IP entries collapse, capped at 50 per proxy.
fn record_test(proxy_id: &str, mut snap: TestSnapshot) -> Result<TestSnapshot> {
    if proxy_id.is_empty() {
        if snap.first_seen.is_empty() {
            snap.first_seen = snap.last_seen.clone();
        }
        return Ok(snap);
    }
    let mut hs = load_history_unlocked()?;
    let entries = hs.by_proxy.entry(proxy_id.into()).or_default();
    if let Some(last) = entries.last_mut() {
        if !snap.ip.is_empty() && last.ip == snap.ip {
            last.last_seen = snap.last_seen.clone();
            last.tcp_ms = snap.tcp_ms;
            last.udp_ms = snap.udp_ms;
            last.udp_error = snap.udp_error.clone();
            let out = last.clone();
            save_history(&hs)?;
            return Ok(out);
        }
    }
    if snap.first_seen.is_empty() {
        snap.first_seen = snap.last_seen.clone();
    }
    entries.push(snap.clone());
    if entries.len() > 50 {
        let drop = entries.len() - 50;
        entries.drain(..drop);
    }
    save_history(&hs)?;
    Ok(snap)
}

pub fn history(proxy_id: &str) -> Result<Vec<TestSnapshot>> {
    let hs = load_history()?;
    Ok(hs.by_proxy.get(proxy_id).cloned().unwrap_or_default())
}

pub fn latest_test(proxy_id: &str) -> Option<TestSnapshot> {
    load_history()
        .ok()
        .and_then(|hs| hs.by_proxy.get(proxy_id).and_then(|v| v.last().cloned()))
}

fn unix_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{s}")
}

fn persist_test_result(
    request: &PreparedProxyTest,
    ticket: Option<&TestTicket>,
    snap: TestSnapshot,
) -> Result<TestSnapshot> {
    if ticket.is_some_and(TestTicket::is_cancelled) {
        anyhow::bail!(TEST_CANCELLED_ERROR);
    }
    // Tests and proxy edits may complete together. Serialize their read/modify/
    // write cycles, then re-check the revision after waiting for the lock.
    let _history_guard = PROXY_HISTORY_LOCK
        .write()
        .map_err(|_| anyhow::anyhow!("proxy history lock poisoned"))?;
    let _store_guard = PROXY_STORE_LOCK
        .write()
        .map_err(|_| anyhow::anyhow!("proxy store lock poisoned"))?;
    if ticket.is_some_and(TestTicket::is_cancelled) {
        anyhow::bail!(TEST_CANCELLED_ERROR);
    }
    let entry = &request.entry;
    let recorded = record_test(&entry.id, snap)?;

    // Backfill empty country tag on the stored entry.
    if !recorded.country_code.is_empty() {
        let mut store_data = load_unlocked()?;
        if let Some(p) = store_data.proxies.iter_mut().find(|p| p.id == entry.id) {
            if p.country.is_empty() || p.country == "—" {
                p.country = recorded.country_code.clone();
                save(&store_data)?;
            }
        }
    }
    Ok(recorded)
}

async fn collect_test_snapshot(entry: &ProxyEntry, lane: TestLane) -> Result<TestSnapshot> {
    // Background work is capped below the global limit, permanently reserving
    // capacity for manual tests. Acquiring the background permit first also
    // prevents a queue of scheduled tests from getting ahead of manual work.
    let background_limit = BACKGROUND_TEST_LIMIT
        .get_or_init(|| tokio::sync::Semaphore::new(BACKGROUND_TEST_CONCURRENCY));
    let _background_permit = match lane {
        TestLane::Manual => None,
        TestLane::Background => Some(
            background_limit
                .acquire()
                .await
                .map_err(|_| anyhow::anyhow!("background proxy test queue closed"))?,
        ),
    };
    let concurrency =
        TEST_CONCURRENCY.get_or_init(|| tokio::sync::Semaphore::new(BULK_TEST_CONCURRENCY));
    let _test_permit = concurrency
        .acquire()
        .await
        .map_err(|_| anyhow::anyhow!("proxy test queue closed"))?;

    let now = unix_now();

    let udp_probe = async {
        if matches!(entry.kind, ProxyKind::Socks5) {
            Some(probe_udp(entry).await)
        } else {
            None
        }
    };
    // Each provider owns its timeout so a timeout can advance to the next
    // provider instead of cancelling the entire fallback chain.
    let geo_probe = geo_check(entry, None);
    // A full test is one user-visible operation. Run TCP, UDP, and the complete
    // geo fallback chain concurrently; each geo provider owns its timeout.
    let (tcp_res, udp_res, geo_res) = tokio::join!(probe(entry), udp_probe, geo_probe);

    // TCP failure → zero geo so snapshot reads "Failed, no IP".
    let tcp_failed = tcp_res.is_err();
    let (ip, country_code, country, region, city, isp, tz, lat, lng, provider) =
        match (&geo_res, tcp_failed) {
            (Ok(g), false) => (
                g.ip.clone(), g.country_code.clone(), g.country.clone(),
                g.region.clone(), g.city.clone(), g.isp.clone(),
                g.timezone.clone(), g.latitude, g.longitude, g.provider.clone(),
            ),
            _ => (String::new(), String::new(), String::new(),
                  String::new(), String::new(), String::new(),
                  String::new(), 0.0, 0.0, String::new()),
        };

    Ok(TestSnapshot {
        first_seen: String::new(),
        last_seen: now,
        ip,
        country_code,
        country,
        region,
        city,
        isp,
        timezone: tz,
        latitude: lat,
        longitude: lng,
        tcp_ms: tcp_res.ok(),
        udp_ms: udp_res
            .as_ref()
            .and_then(|r| r.as_ref().ok().copied()),
        udp_error: udp_res
            .as_ref()
            .and_then(|r| r.as_ref().err().map(|e| e.to_string())),
        provider,
    })
}

async fn collect_test_snapshot_or_cancel(
    request: &PreparedProxyTest,
    lane: TestLane,
    ticket: &mut Option<TestTicket>,
) -> Result<TestSnapshot> {
    let Some(ticket) = ticket.as_mut() else {
        return collect_test_snapshot(&request.entry, lane).await;
    };
    if ticket.is_cancelled() {
        anyhow::bail!(TEST_CANCELLED_ERROR);
    }

    tokio::select! {
        biased;
        _ = ticket.receiver.changed() => Err(anyhow::anyhow!(TEST_CANCELLED_ERROR)),
        result = collect_test_snapshot(&request.entry, lane) => result,
    }
}

async fn retry_delay_or_cancel(ticket: &mut Option<TestTicket>) -> Result<()> {
    let delay = tokio::time::sleep(std::time::Duration::from_secs(1));
    let Some(ticket) = ticket.as_mut() else {
        delay.await;
        return Ok(());
    };
    if ticket.is_cancelled() {
        anyhow::bail!(TEST_CANCELLED_ERROR);
    }

    tokio::select! {
        biased;
        _ = ticket.receiver.changed() => Err(anyhow::anyhow!(TEST_CANCELLED_ERROR)),
        _ = delay => Ok(()),
    }
}

fn test_snapshot_is_complete(snapshot: &TestSnapshot) -> bool {
    snapshot.tcp_ms.is_some() && !snapshot.ip.is_empty()
}

async fn acquire_proxy_test_lock_or_cancel(
    lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    ticket: &mut Option<TestTicket>,
) -> Result<tokio::sync::OwnedMutexGuard<()>> {
    let Some(ticket) = ticket.as_mut() else {
        return Ok(lock.lock_owned().await);
    };
    if ticket.is_cancelled() {
        anyhow::bail!(TEST_CANCELLED_ERROR);
    }

    tokio::select! {
        biased;
        _ = ticket.receiver.changed() => Err(anyhow::anyhow!(TEST_CANCELLED_ERROR)),
        guard = lock.lock_owned() => Ok(guard),
    }
}

async fn wait_for_manual_test_to_finish(
    proxy_id: &str,
    ticket: &mut Option<TestTicket>,
) -> Result<()> {
    if proxy_id.is_empty() {
        return Ok(());
    }
    let epoch = MANUAL_TEST_EPOCH.get_or_init(|| tokio::sync::watch::channel(0).0);
    let mut epoch_changes = epoch.subscribe();
    loop {
        if !manual_test_is_active(proxy_id) {
            return Ok(());
        }
        let Some(ticket) = ticket.as_mut() else {
            let _ = epoch_changes.changed().await;
            continue;
        };
        if ticket.is_cancelled() {
            anyhow::bail!(TEST_CANCELLED_ERROR);
        }
        tokio::select! {
            biased;
            _ = ticket.receiver.changed() => return Err(anyhow::anyhow!(TEST_CANCELLED_ERROR)),
            _ = epoch_changes.changed() => {}
        }
    }
}

async fn full_test_with_retry_in_lane(
    mut request: PreparedProxyTest,
    max_attempts: usize,
    lane: TestLane,
) -> Result<TestSnapshot> {
    let _manual_guard = matches!(lane, TestLane::Manual)
        .then(|| begin_manual_test(&request.entry.id))
        .flatten();
    if matches!(lane, TestLane::Manual) {
        // A user-requested test supersedes queued/running automatic work for
        // this proxy. Other proxies remain fully parallel.
        prepare_manual_test_request(&mut request)?;
    } else {
        let mut waiting_ticket = test_ticket(&request);
        wait_for_manual_test_to_finish(&request.entry.id, &mut waiting_ticket).await?;
        validate_background_test_request(&mut request)?;
    }
    let attempts = max_attempts.max(1);
    let mut ticket = test_ticket(&request);
    let _proxy_guard =
        acquire_proxy_test_lock_or_cancel(proxy_test_lock(&request.entry.id), &mut ticket).await?;
    let mut last_snapshot = None;
    let mut last_error = None;

    for attempt in 0..attempts {
        match collect_test_snapshot_or_cancel(&request, lane, &mut ticket).await {
            Ok(snapshot) if test_snapshot_is_complete(&snapshot) => {
                return persist_test_result(&request, ticket.as_ref(), snapshot);
            }
            Ok(snapshot) => last_snapshot = Some(snapshot),
            Err(error) => last_error = Some(error),
        }

        if attempt + 1 < attempts {
            retry_delay_or_cancel(&mut ticket).await?;
        }
    }

    if let Some(snapshot) = last_snapshot {
        persist_test_result(&request, ticket.as_ref(), snapshot)
    } else {
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("proxy test did not run")))
    }
}

/// Run one user-requested TCP + UDP + geo test, persist it into history, and
/// auto-fill the country tag. Manual tests use the priority lane.
pub async fn full_test(entry: &ProxyEntry) -> Result<TestSnapshot> {
    let request = PreparedProxyTest {
        revision: current_test_revision(&entry.id),
        entry: entry.clone(),
    };
    full_test_with_retry_in_lane(request, 1, TestLane::Manual).await
}

/// Automatic one-off tests (for API-created profiles) share the capped
/// background lane so they cannot consume capacity reserved for UI actions.
pub async fn full_test_background(entry: &ProxyEntry) -> Result<TestSnapshot> {
    let request = PreparedProxyTest {
        revision: current_test_revision(&entry.id),
        entry: entry.clone(),
    };
    full_test_with_retry_in_lane(request, 1, TestLane::Background).await
}

async fn full_test_batch_in_lane(
    requests: Vec<PreparedProxyTest>,
    lane: TestLane,
    max_attempts: usize,
) -> Vec<BatchTestResult> {
    let count = requests.len();
    let mut queue: VecDeque<(usize, PreparedProxyTest)> =
        requests.into_iter().enumerate().collect();
    let mut tasks = tokio::task::JoinSet::new();
    while tasks.len() < BULK_TEST_CONCURRENCY {
        let Some((index, request)) = queue.pop_front() else {
            break;
        };
        tasks.spawn(async move {
            (
                index,
                full_test_with_retry_in_lane(request, max_attempts, lane).await,
            )
        });
    }

    let mut results: Vec<Option<BatchTestResult>> = (0..count).map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((index, Ok(snapshot))) => {
                results[index] = Some(BatchTestResult {
                    index,
                    snapshot: Some(snapshot),
                    error: None,
                });
            }
            Ok((index, Err(error))) => {
                results[index] = Some(BatchTestResult {
                    index,
                    snapshot: None,
                    error: Some(error.to_string()),
                });
            }
            Err(_) => {}
        }
        while tasks.len() < BULK_TEST_CONCURRENCY {
            let Some((index, request)) = queue.pop_front() else {
                break;
            };
            tasks.spawn(async move {
                (
                    index,
                    full_test_with_retry_in_lane(request, max_attempts, lane).await,
                )
            });
        }
    }

    results
        .into_iter()
        .enumerate()
        .map(|(index, result)| result.unwrap_or_else(|| BatchTestResult {
            index,
            snapshot: None,
            error: Some("proxy test task did not complete".to_string()),
        }))
        .collect()
}

/// Test every proxy with the manual-priority five-worker pool. Each proxy owns
/// its full timeout; queued entries are never failed because the batch is busy.
pub async fn full_test_batch(entries: Vec<ProxyEntry>) -> Vec<BatchTestResult> {
    full_test_batch_in_lane(prepare_proxy_tests(entries), TestLane::Manual, 1).await
}

/// Run automatic tests without consuming the capacity reserved for manual
/// tests. Failed TCP checks are retried up to `max_attempts` in total.
pub async fn full_test_batch_background(
    requests: Vec<PreparedProxyTest>,
    max_attempts: usize,
) -> Vec<BatchTestResult> {
    full_test_batch_in_lane(requests, TestLane::Background, max_attempts).await
}

/// Fallback country → IANA timezone for providers that omit timezone.
pub fn country_to_timezone(cc: &str) -> &'static str {
    match cc.to_ascii_uppercase().as_str() {
        "US" => "America/New_York",
        "CA" => "America/Toronto",
        "GB" | "UK" => "Europe/London",
        "DE" => "Europe/Berlin",
        "FR" => "Europe/Paris",
        "ES" => "Europe/Madrid",
        "IT" => "Europe/Rome",
        "NL" => "Europe/Amsterdam",
        "PL" => "Europe/Warsaw",
        "PT" => "Europe/Lisbon",
        "RO" => "Europe/Bucharest",
        "RU" => "Europe/Moscow",
        "UA" => "Europe/Kyiv",
        "TR" => "Europe/Istanbul",
        "GR" => "Europe/Athens",
        "CZ" => "Europe/Prague",
        "HU" => "Europe/Budapest",
        "SE" => "Europe/Stockholm",
        "FI" => "Europe/Helsinki",
        "NO" => "Europe/Oslo",
        "DK" => "Europe/Copenhagen",
        "CH" => "Europe/Zurich",
        "AT" => "Europe/Vienna",
        "BR" => "America/Sao_Paulo",
        "AR" => "America/Argentina/Buenos_Aires",
        "MX" => "America/Mexico_City",
        "AU" => "Australia/Sydney",
        "NZ" => "Pacific/Auckland",
        "IN" => "Asia/Kolkata",
        "ID" => "Asia/Jakarta",
        "MY" => "Asia/Kuala_Lumpur",
        "SG" => "Asia/Singapore",
        "TH" => "Asia/Bangkok",
        "VN" => "Asia/Ho_Chi_Minh",
        "CN" => "Asia/Shanghai",
        "HK" => "Asia/Hong_Kong",
        "TW" => "Asia/Taipei",
        "JP" => "Asia/Tokyo",
        "KR" => "Asia/Seoul",
        "IL" => "Asia/Jerusalem",
        "SA" => "Asia/Riyadh",
        "AE" => "Asia/Dubai",
        _ => "UTC",
    }
}
