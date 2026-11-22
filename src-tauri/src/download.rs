//! Launcher-owned download client. The resolved proxy applies to this client
//! only: browser processes keep their own `--proxy-server` / `--no-proxy-server`
//! flags and never read these settings. No proxy environment variables are set,
//! so spawned browsers cannot inherit the download route.

use crate::settings::Settings;
use anyhow::{bail, Context, Result};
use std::time::Duration;

pub(crate) const SYSTEM: &str = "system";
pub(crate) const DIRECT: &str = "direct";
const INTERNET_SETTINGS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Internet Settings";

/// Normalize a stored value without rejecting it, so an invalid entry stays
/// visible in Settings instead of silently changing the download route.
pub(crate) fn canonical(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case(SYSTEM) {
        SYSTEM.into()
    } else if value.eq_ignore_ascii_case(DIRECT) {
        DIRECT.into()
    } else {
        value.to_string()
    }
}

/// Validate and canonicalize a value at the settings-save boundary.
pub(crate) fn normalize(value: &str) -> std::result::Result<String, String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case(SYSTEM) {
        return Ok(SYSTEM.into());
    }
    if value.eq_ignore_ascii_case(DIRECT) {
        return Ok(DIRECT.into());
    }
    if value.is_empty() {
        return Err("Choose a download proxy or enter its URL".into());
    }
    let url =
        reqwest::Url::parse(value).map_err(|_| "Download proxy URL is invalid".to_string())?;
    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") {
        return Err("Download proxy must use http, https, socks5 or socks5h".into());
    }
    if url.host_str().is_none() || url.port().is_none() {
        return Err("Download proxy needs a host and port".into());
    }
    Ok(url.to_string())
}

pub(crate) fn client(
    settings: &Settings,
    connect: Duration,
    read: Duration,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(read);
    match resolve(&settings.download_proxy)? {
        Endpoint::Direct => builder = builder.no_proxy(),
        Endpoint::Proxy(url) => {
            builder = builder.proxy(reqwest::Proxy::all(&url).context("Invalid download proxy")?)
        }
    }
    builder.build().context("Build download client")
}

enum Endpoint {
    Direct,
    Proxy(String),
}

fn resolve(value: &str) -> Result<Endpoint> {
    let value = value.trim();
    if value.eq_ignore_ascii_case(DIRECT) {
        return Ok(Endpoint::Direct);
    }
    if value.is_empty() || value.eq_ignore_ascii_case(SYSTEM) {
        return Ok(match windows_system_proxy()? {
            Some(url) => Endpoint::Proxy(url),
            None => Endpoint::Direct,
        });
    }
    Ok(Endpoint::Proxy(
        normalize(value).map_err(anyhow::Error::msg)?,
    ))
}

fn windows_system_proxy() -> Result<Option<String>> {
    decide_system_proxy(
        registry_dword("ProxyEnable").unwrap_or(0),
        registry_string("ProxyServer"),
        registry_string("AutoConfigURL"),
    )
}

fn decide_system_proxy(
    proxy_enable: u32,
    proxy_server: Option<String>,
    auto_config_url: Option<String>,
) -> Result<Option<String>> {
    if proxy_enable == 0 {
        if auto_config_url.is_some_and(|value| !value.trim().is_empty()) {
            bail!("Windows uses an automatic proxy script; set a download proxy URL explicitly");
        }
        return Ok(None);
    }
    let server =
        proxy_server.context("Windows ProxyEnable is on but ProxyServer cannot be read")?;
    Ok(Some(
        pick_proxy(&server).context("Windows ProxyServer has no usable address")?,
    ))
}

/// Windows stores either `host:port` or a per-protocol list such as
/// `http=host:port;https=host:port;socks=host:port`.
fn pick_proxy(server: &str) -> Option<String> {
    let mut chosen = None;
    let mut fallback = None;
    for part in server.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (scheme, address) = match part.split_once('=') {
            Some((scheme, address)) => (Some(scheme.trim()), address.trim()),
            None => (None, part),
        };
        if address.is_empty() {
            continue;
        }
        let candidate = if address.contains("://") {
            address.to_string()
        } else {
            let scheme = match scheme {
                Some(value)
                    if value.eq_ignore_ascii_case("socks")
                        || value.eq_ignore_ascii_case("socks5") =>
                {
                    "socks5"
                }
                _ => "http",
            };
            format!("{scheme}://{address}")
        };
        if scheme.is_some_and(|value| value.eq_ignore_ascii_case("https")) {
            return Some(candidate);
        }
        if chosen.is_none() {
            chosen = Some(candidate.clone());
        }
        fallback.get_or_insert(candidate);
    }
    chosen.or(fallback)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn registry_dword(name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

    let subkey = wide(INTERNET_SETTINGS);
    let value = wide(name);
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let mut kind = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut kind,
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (status == 0).then_some(data)
}

fn registry_string(name: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    let subkey = wide(INTERNET_SETTINGS);
    let value = wide(name);
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status != 0 || size < 2 {
        return None;
    }
    let mut buffer = vec![0u16; size as usize / 2];
    let mut kind = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            &mut kind,
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let length = buffer
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..length]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_proxy_values_are_validated_and_stored_explicitly() {
        assert_eq!(normalize(" SYSTEM ").unwrap(), "system");
        assert_eq!(normalize("Direct").unwrap(), "direct");
        assert_eq!(
            normalize("http://127.0.0.1:7897").unwrap(),
            "http://127.0.0.1:7897/"
        );
        assert_eq!(
            normalize("socks5://proxy.example:1080").unwrap(),
            "socks5://proxy.example:1080"
        );
        assert!(normalize("").is_err());
        assert!(normalize("http://127.0.0.1").is_err());
        assert!(normalize("ftp://127.0.0.1:21").is_err());
        assert!(normalize("not a proxy").is_err());
        assert_eq!(canonical(""), "system");
        assert_eq!(canonical(" DIRECT "), "direct");
        assert_eq!(canonical("http://bad value"), "http://bad value");
    }

    #[test]
    fn windows_proxy_lists_prefer_https_then_the_first_entry() {
        assert_eq!(
            pick_proxy("127.0.0.1:7897").as_deref(),
            Some("http://127.0.0.1:7897")
        );
        // Windows labels which requests a proxy serves; both entries are HTTP
        // CONNECT proxies, so the selected URL keeps the http scheme.
        assert_eq!(
            pick_proxy("http=127.0.0.1:8080;https=127.0.0.1:8443").as_deref(),
            Some("http://127.0.0.1:8443")
        );
        assert_eq!(
            pick_proxy("socks=127.0.0.1:1080").as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
        assert_eq!(
            pick_proxy("http=127.0.0.1:8080;socks=127.0.0.1:1080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(pick_proxy("  "), None);
        assert_eq!(pick_proxy("http="), None);
    }

    #[test]
    fn system_proxy_decisions_cover_direct_pac_and_missing_server() {
        assert_eq!(decide_system_proxy(0, None, None).unwrap(), None);
        assert!(decide_system_proxy(0, None, Some("http://pac.test/proxy.pac".into())).is_err());
        assert!(decide_system_proxy(1, None, None).is_err());
        assert_eq!(
            decide_system_proxy(1, Some("127.0.0.1:7897".into()), None)
                .unwrap()
                .as_deref(),
            Some("http://127.0.0.1:7897")
        );
    }

    #[test]
    fn explicit_routes_build_without_reading_the_system_proxy() {
        let base = Settings {
            theme: "light".into(),
            geo_checker: None,
            screen_resolution_mode: None,
            minimize_to_tray: true,
            api_enabled: true,
            api_port: 30325,
            download_proxy: "direct".into(),
        };
        assert!(client(&base, Duration::from_secs(1), Duration::from_secs(1)).is_ok());
        let custom = Settings {
            download_proxy: "http://127.0.0.1:9".into(),
            ..base
        };
        assert!(client(&custom, Duration::from_secs(1), Duration::from_secs(1)).is_ok());
        let invalid = Settings {
            download_proxy: "http://127.0.0.1".into(),
            ..custom
        };
        assert!(client(&invalid, Duration::from_secs(1), Duration::from_secs(1)).is_err());
    }
}
