use crate::{settings, store};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

const LATENCY_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LATENCY_TIMEOUT_ERROR: &str = "proxy latency test timed out after 5 seconds";
const BULK_TEST_CONCURRENCY: usize = 5;
static TEST_RESULT_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    Socks5,
    Http,
    Https,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

pub fn load() -> Result<ProxyStore> {
    let path = store::proxies_path()?;
    if !path.exists() {
        return Ok(ProxyStore::default());
    }
    let body = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn save(s: &ProxyStore) -> Result<()> {
    let body = serde_json::to_string_pretty(s)?;
    fs::write(store::proxies_path()?, body)?;
    Ok(())
}

pub fn list() -> Result<Vec<ProxyEntry>> {
    Ok(load()?.proxies)
}

pub fn upsert(mut entry: ProxyEntry) -> Result<ProxyEntry> {
    entry = normalize_entry(entry)?;
    let mut s = load()?;
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
    if let Some(slot) = s.proxies.iter_mut().find(|p| p.id == entry.id) {
        *slot = entry.clone();
    } else {
        s.proxies.push(entry.clone());
    }
    save(&s)?;
    Ok(entry)
}

/// Upsert that reuses an entry with the same kind/host/port/username.
pub fn upsert_dedup(mut entry: ProxyEntry) -> Result<ProxyEntry> {
    entry = normalize_entry(entry)?;
    let mut s = load()?;
    let key = duplicate_key(&entry);
    if let Some(existing) = s.proxies.iter().find(|proxy| duplicate_key(proxy) == key) {
        return Ok(existing.clone());
    }
    if entry.id.is_empty() || s.proxies.iter().any(|proxy| proxy.id == entry.id) {
        entry.id = uuid::Uuid::new_v4().to_string();
    }
    s.proxies.push(entry.clone());
    save(&s)?;
    Ok(entry)
}

pub fn delete(id: &str) -> Result<()> {
    let mut s = load()?;
    s.proxies.retain(|p| p.id != id);
    save(&s)?;
    // Also wipe persisted test history.
    let mut hs = load_history()?;
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

/// Save many entries; validates the whole batch, then persists unique endpoints.
pub fn bulk_save(entries: Vec<ProxyEntry>) -> Result<usize> {
    let entries = entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            normalize_entry(entry).with_context(|| format!("entry {} is invalid", index + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut store_data = load()?;
    let mut seen: HashSet<ProxyDuplicateKey> =
        store_data.proxies.iter().map(duplicate_key).collect();
    let mut added = 0usize;
    for mut e in entries {
        if !seen.insert(duplicate_key(&e)) {
            continue;
        }
        if e.id.is_empty() || store_data.proxies.iter().any(|proxy| proxy.id == e.id) {
            e.id = uuid::Uuid::new_v4().to_string();
        }
        store_data.proxies.push(e);
        added += 1;
    }
    save(&store_data)?;
    Ok(added)
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

/// Probe geo through `entry` if Some, else direct; provider default ip-api.com.
pub async fn geo_check_via(entry: Option<&ProxyEntry>, provider_override: Option<String>) -> Result<GeoInfo> {
    let provider = provider_override
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| settings::load().ok().and_then(|s| s.geo_checker).unwrap_or_else(|| "ip-api.com".into()));

    let url = match provider.as_str() {
        "ip-api.com" => "http://ip-api.com/json/?fields=status,message,query,country,countryCode,regionName,city,isp,timezone,lat,lon",
        "ipapi.co" => "https://ipapi.co/json/",
        "ipwho.is" => "https://ipwho.is/",
        _ => "http://ip-api.com/json/?fields=status,message,query,country,countryCode,regionName,city,isp,timezone,lat,lon",
    };

    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8));
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

    let body: serde_json::Value = client.get(url).send().await?.json().await?;

    let s = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    let f = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
    };
    let info = match provider.as_str() {
        "ip-api.com" => {
            if s(&body, "status") == "fail" {
                anyhow::bail!("ip-api.com: {}", s(&body, "message"));
            }
            GeoInfo {
                ip: s(&body, "query"),
                country: s(&body, "country"),
                country_code: s(&body, "countryCode"),
                region: s(&body, "regionName"),
                city: s(&body, "city"),
                isp: s(&body, "isp"),
                timezone: s(&body, "timezone"),
                latitude: f(&body, "lat"),
                longitude: f(&body, "lon"),
                provider,
            }
        }
        "ipapi.co" => GeoInfo {
            ip: s(&body, "ip"),
            country: s(&body, "country_name"),
            country_code: s(&body, "country_code"),
            region: s(&body, "region"),
            city: s(&body, "city"),
            isp: s(&body, "org"),
            timezone: s(&body, "timezone"),
            latitude: f(&body, "latitude"),
            longitude: f(&body, "longitude"),
            provider,
        },
        "ipwho.is" => GeoInfo {
            ip: s(&body, "ip"),
            country: s(&body, "country"),
            country_code: s(&body, "country_code"),
            region: s(&body, "region"),
            city: s(&body, "city"),
            isp: body.get("connection").and_then(|c| c.get("isp")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
            timezone: body.get("timezone").and_then(|t| t.get("id")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
            latitude: f(&body, "latitude"),
            longitude: f(&body, "longitude"),
            provider,
        },
        _ => GeoInfo {
            ip: s(&body, "query"),
            country: s(&body, "country"),
            country_code: s(&body, "countryCode"),
            region: String::new(),
            city: String::new(),
            isp: String::new(),
            timezone: String::new(),
            latitude: 0.0,
            longitude: 0.0,
            provider,
        },
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

fn load_history() -> Result<HistoryStore> {
    let path = history_path()?;
    if !path.exists() {
        return Ok(HistoryStore::default());
    }
    let body = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn save_history(s: &HistoryStore) -> Result<()> {
    let body = serde_json::to_string_pretty(s)?;
    fs::write(history_path()?, body)?;
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
    let mut hs = load_history()?;
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

fn persist_test_result(entry: &ProxyEntry, snap: TestSnapshot) -> Result<TestSnapshot> {
    // Parallel batch probes must not overwrite each other's history or
    // country backfills after their network work completes together.
    let _guard = TEST_RESULT_WRITE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("proxy test result lock poisoned"))?;
    let recorded = record_test(&entry.id, snap)?;

    // Backfill empty country tag on the stored entry.
    if !recorded.country_code.is_empty() {
        let mut store_data = load()?;
        if let Some(p) = store_data.proxies.iter_mut().find(|p| p.id == entry.id) {
            if p.country.is_empty() || p.country == "—" {
                p.country = recorded.country_code.clone();
                save(&store_data)?;
            }
        }
    }
    Ok(recorded)
}

/// Run TCP + UDP + geo, persist into history, auto-fill country tag.
pub async fn full_test(entry: &ProxyEntry) -> Result<TestSnapshot> {
    let now = unix_now();

    let udp_probe = async {
        if matches!(entry.kind, ProxyKind::Socks5) {
            Some(probe_udp(entry).await)
        } else {
            None
        }
    };
    let geo_probe = async {
        tokio::time::timeout(LATENCY_TEST_TIMEOUT, geo_check(entry, None))
            .await
            .context(LATENCY_TIMEOUT_ERROR)?
    };
    // A full test is one user-visible operation. Run its network checks
    // concurrently so a non-responsive proxy is classified within five seconds.
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

    let snap = TestSnapshot {
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
    };

    persist_test_result(entry, snap)
}

/// Test every proxy with a five-worker pool. Each proxy owns its full timeout;
/// queued entries are never failed merely because the batch is still running.
pub async fn full_test_batch(entries: Vec<ProxyEntry>) -> Vec<BatchTestResult> {
    let count = entries.len();
    let mut queue: VecDeque<(usize, ProxyEntry)> = entries.into_iter().enumerate().collect();
    let mut tasks = tokio::task::JoinSet::new();
    while tasks.len() < BULK_TEST_CONCURRENCY {
        let Some((index, entry)) = queue.pop_front() else {
            break;
        };
        tasks.spawn(async move { (index, full_test(&entry).await) });
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
            let Some((index, entry)) = queue.pop_front() else {
                break;
            };
            tasks.spawn(async move { (index, full_test(&entry).await) });
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
