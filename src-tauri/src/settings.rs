use crate::{protected_data, store};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

const API_SECRET_PURPOSE: &[u8] = b"ShardX Launcher/settings-api-secret/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub geo_checker: Option<String>,
    #[serde(default)]
    pub screen_resolution_mode: Option<String>,
    #[serde(default = "default_minimize_to_tray")]
    pub minimize_to_tray: bool,
    #[serde(default = "default_api_enabled")]
    pub api_enabled: bool,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct SettingsDisk {
    #[serde(default = "default_theme")]
    theme: String,
    geo_checker: Option<String>,
    screen_resolution_mode: Option<String>,
    #[serde(default = "default_minimize_to_tray")]
    minimize_to_tray: bool,
    #[serde(default = "default_api_enabled")]
    api_enabled: bool,
    #[serde(default = "default_api_port")]
    api_port: u16,
    #[serde(default)]
    api_secret: Option<String>,
    #[serde(default)]
    api_secret_protected: Option<protected_data::ProtectedBlob>,
}

#[derive(Debug, Clone, Serialize)]
struct ProtectedSettingsDisk {
    schema_version: u8,
    theme: String,
    geo_checker: Option<String>,
    screen_resolution_mode: Option<String>,
    minimize_to_tray: bool,
    api_enabled: bool,
    api_port: u16,
    api_secret_protected: protected_data::ProtectedBlob,
}

#[derive(Debug, Clone)]
pub struct LoadedSettings {
    pub public: Settings,
    pub api_secret: String,
}

fn settings_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn default_theme() -> String {
    "light".into()
}

fn default_minimize_to_tray() -> bool {
    true
}

fn default_api_enabled() -> bool {
    true
}

fn default_api_port() -> u16 {
    40325
}

impl Default for SettingsDisk {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            geo_checker: Some("ipwho.is".into()),
            screen_resolution_mode: Some("fingerprint".into()),
            minimize_to_tray: default_minimize_to_tray(),
            api_enabled: default_api_enabled(),
            api_port: default_api_port(),
            api_secret: None,
            api_secret_protected: None,
        }
    }
}

fn normalize(mut settings: Settings) -> Settings {
    if !matches!(
        settings.geo_checker.as_deref(),
        Some(
            "ipwho.is"
                | "geojs.io"
                | "country.is"
                | "bigdatacloud.com"
                | "freeipapi.com"
                | "ipapi.is"
        )
    ) {
        settings.geo_checker = Some("ipwho.is".into());
    }
    settings
}

fn public(disk: &SettingsDisk) -> Settings {
    normalize(Settings {
        theme: disk.theme.clone(),
        geo_checker: disk.geo_checker.clone(),
        screen_resolution_mode: disk.screen_resolution_mode.clone(),
        minimize_to_tray: disk.minimize_to_tray,
        api_enabled: disk.api_enabled,
        api_port: disk.api_port,
    })
}

fn generate_secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn protected_disk(public: Settings, secret: &str) -> Result<ProtectedSettingsDisk> {
    Ok(ProtectedSettingsDisk {
        schema_version: 1,
        theme: public.theme,
        geo_checker: public.geo_checker,
        screen_resolution_mode: public.screen_resolution_mode,
        minimize_to_tray: public.minimize_to_tray,
        api_enabled: public.api_enabled,
        api_port: public.api_port,
        api_secret_protected: protected_data::protect(API_SECRET_PURPOSE, secret.as_bytes())?,
    })
}

fn persist(public: Settings, secret: &str) -> Result<()> {
    let disk = protected_disk(normalize(public), secret)?;
    let bytes = serde_json::to_vec_pretty(&disk)?;
    store::atomic_write_sensitive(&store::settings_path()?, &bytes)
}

fn load_unlocked() -> Result<LoadedSettings> {
    let path = store::settings_path()?;
    if !path.exists() {
        let public = Settings {
            theme: default_theme(),
            geo_checker: Some("ipwho.is".into()),
            screen_resolution_mode: Some("fingerprint".into()),
            minimize_to_tray: default_minimize_to_tray(),
            api_enabled: default_api_enabled(),
            api_port: default_api_port(),
        };
        let secret = generate_secret();
        persist(public.clone(), &secret)?;
        return Ok(LoadedSettings {
            public,
            api_secret: secret,
        });
    }

    let disk: SettingsDisk = store::load_json_with_backup(&path)?;
    let public = public(&disk);
    let secret = match disk.api_secret_protected.as_ref() {
        Some(value) => match protected_data::unprotect(API_SECRET_PURPOSE, value) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("protected API secret is not UTF-8"))?,
            Err(protected_data::UnprotectError::Unavailable) => {
                let fresh = generate_secret();
                persist(public.clone(), &fresh)?;
                eprintln!("[launcher] API secret belonged to another Windows user; generated a new token secret");
                fresh
            }
            Err(error) => anyhow::bail!("cannot read protected API secret: {error}"),
        },
        None => {
            let legacy = disk
                .api_secret
                .filter(|value| !value.is_empty())
                .unwrap_or_else(generate_secret);
            persist(public.clone(), &legacy)?;
            legacy
        }
    };
    Ok(LoadedSettings {
        public,
        api_secret: secret,
    })
}

pub fn load() -> Result<Settings> {
    let _guard = settings_lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("settings lock poisoned"))?;
    Ok(load_unlocked()?.public)
}

pub fn ensure_secret() -> Result<LoadedSettings> {
    let _guard = settings_lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("settings lock poisoned"))?;
    load_unlocked()
}

pub fn save(public: &Settings) -> Result<()> {
    let _guard = settings_lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("settings lock poisoned"))?;
    let loaded = load_unlocked()?;
    persist(public.clone(), &loaded.api_secret)
}

pub fn rotate_secret() -> Result<LoadedSettings> {
    let _guard = settings_lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("settings lock poisoned"))?;
    let loaded = load_unlocked()?;
    let secret = generate_secret();
    persist(loaded.public.clone(), &secret)?;
    Ok(LoadedSettings {
        public: loaded.public,
        api_secret: secret,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_geo_provider_is_normalized() {
        let normalized = normalize(Settings {
            theme: "dark".into(),
            geo_checker: Some("unknown".into()),
            screen_resolution_mode: None,
            minimize_to_tray: false,
            api_enabled: true,
            api_port: 1,
        });
        assert_eq!(normalized.geo_checker.as_deref(), Some("ipwho.is"));
    }
}
