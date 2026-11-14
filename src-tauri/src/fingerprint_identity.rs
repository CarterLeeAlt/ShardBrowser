//! Consistency rules for newly created and cloned identities only.
use serde_json::{Map, Value};

// Released versions, in Chrome's major.minor.patch format.
const MACOS_VERSIONS: &[&str] = &[
    "14.6.1", "14.7.0", "14.7.1", "14.7.2", "15.4.0", "15.4.1", "15.5.0", "15.6.0", "15.6.1",
    "15.7.0", "26.0.0", "26.0.1", "26.1.0",
];
// Sec-CH-UA-Platform-Version reports UniversalApiContract, not the OS build:
// Windows 10 2004+, Windows 11 21H2, 22H2/23H2, 24H2/25H2 respectively.
const WINDOWS_VERSIONS: &[&str] = &["10.0.0", "14.0.0", "15.0.0", "19.0.0"];
const LINUX_VERSIONS: &[&str] = &[
    "5.15.0", "6.1.0", "6.5.0", "6.6.0", "6.8.0", "6.10.0", "6.11.0", "6.12.0", "6.14.0", "6.15.0",
    "6.16.0",
];

fn apple_generation(config: &Map<String, Value>) -> Option<u32> {
    let renderer = config.get("webgl")?.get("renderer")?.as_str()?;
    let rest = &renderer[renderer.find("Apple M")? + 7..];
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn minimum_macos(config: &Map<String, Value>) -> u32 {
    match apple_generation(config) {
        Some(g) if g >= 5 => 26,
        Some(4) => 15,
        _ => 14,
    }
}

fn write_version(config: &mut Map<String, Value>, version: &str) {
    for key in ["navigator", "client_hints"] {
        if let Some(obj) = config.get_mut(key).and_then(Value::as_object_mut) {
            obj.insert("platform_version".into(), version.into());
        }
    }
}

pub(crate) fn randomize_platform_version(config: &mut Map<String, Value>) {
    let platform = config
        .get("navigator")
        .and_then(|n| n.get("platform"))
        .and_then(Value::as_str);
    let pool: Vec<&str> = match platform {
        Some("macOS") => MACOS_VERSIONS
            .iter()
            .copied()
            .filter(|v| {
                v.split('.').next().unwrap().parse::<u32>().unwrap() >= minimum_macos(config)
            })
            .collect(),
        Some("Windows") => WINDOWS_VERSIONS.to_vec(),
        Some("Linux") => LINUX_VERSIONS.to_vec(),
        _ => return,
    };
    write_version(
        config,
        pool[uuid::Uuid::new_v4().as_bytes()[0] as usize % pool.len()],
    );
}

/// Preserve valid user choices, repair inconsistent platform fields, then use
/// the metadata belonging to the installed engine. Never called for edits/imports.
pub(crate) fn prepare_new(config: &mut Map<String, Value>) {
    let current = ["client_hints", "navigator"]
        .iter()
        .filter_map(|key| config.get(*key)?.get("platform_version")?.as_str())
        .find(|v| !v.is_empty())
        .unwrap_or("");
    let platform = config
        .get("navigator")
        .and_then(|n| n.get("platform"))
        .and_then(Value::as_str);
    let version = match platform {
        Some("Windows") => Some(
            if WINDOWS_VERSIONS.contains(&current) {
                current
            } else {
                "19.0.0"
            }
            .to_string(),
        ),
        Some("macOS") => {
            let parsed: Option<Vec<u32>> = current.split('.').map(|v| v.parse().ok()).collect();
            let mut parts = parsed
                .filter(|p| (1..=3).contains(&p.len()) && p[0] >= minimum_macos(config))
                .unwrap_or_else(|| vec![minimum_macos(config), 0, 0]);
            parts.resize(3, 0);
            Some(format!("{}.{}.{}", parts[0], parts[1], parts[2]))
        }
        _ => None,
    };
    if let Some(version) = version {
        write_version(config, &version);
    }
    crate::runtime::prepare_new_identity(config);
}

pub(crate) fn add_apple_features(config: &mut Map<String, Value>, chrome_major: u32) {
    let Some(generation) = apple_generation(config) else {
        return;
    };
    let Some(gpu) = config.get_mut("webgpu").and_then(Value::as_object_mut) else {
        return;
    };
    if gpu.get("vendor").and_then(Value::as_str) != Some("apple") {
        return;
    }
    let Some(features) = gpu.get_mut("features").and_then(Value::as_array_mut) else {
        return;
    };
    if features.is_empty() {
        return;
    }
    for (feature, supported) in [
        ("texture-compression-unaligned", chrome_major >= 156),
        (
            "atomic-vec2u-min-max",
            chrome_major >= 157 && generation >= 2,
        ),
    ] {
        if supported && !features.iter().any(|v| v.as_str() == Some(feature)) {
            let at = features
                .iter()
                .position(|v| v.as_str().is_some_and(|s| s > feature))
                .unwrap_or(features.len());
            features.insert(at, feature.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_identity_versions_and_gpu_features_match_device_and_engine() {
        for (chip, minimum) in [("Apple M1", 14), ("Apple M4 Pro", 15), ("Apple M5", 26)] {
            let source = serde_json::json!({
                "navigator": { "platform": "macOS", "platform_version": "14.7" },
                "client_hints": { "platform_version": "14.7" },
                "webgl": { "renderer": chip },
                "webgpu": { "vendor": "apple", "features": ["shader-f16"] }
            })
            .as_object()
            .unwrap()
            .clone();
            for _ in 0..100 {
                let mut created = source.clone();
                randomize_platform_version(&mut created);
                let version = created["navigator"]["platform_version"].as_str().unwrap();
                assert_eq!(version.split('.').count(), 3);
                assert!(version.split('.').next().unwrap().parse::<u32>().unwrap() >= minimum);
                assert_eq!(created["client_hints"]["platform_version"], version);
                add_apple_features(&mut created, 152);
                assert_eq!(created["webgpu"]["features"], source["webgpu"]["features"]);
                add_apple_features(&mut created, 157);
                let once = created.clone();
                add_apple_features(&mut created, 157);
                assert_eq!(created, once);
                let features = created["webgpu"]["features"].as_array().unwrap();
                assert!(features.contains(&Value::from("texture-compression-unaligned")));
                assert_eq!(
                    features.contains(&Value::from("atomic-vec2u-min-max")),
                    minimum >= 15
                );
                assert_eq!(source["navigator"]["platform_version"], "14.7");
            }
        }
        for _ in 0..100 {
            let mut config =
                serde_json::json!({ "navigator": { "platform": "Windows" }, "client_hints": {} })
                    .as_object()
                    .unwrap()
                    .clone();
            randomize_platform_version(&mut config);
            assert!(WINDOWS_VERSIONS
                .contains(&config["navigator"]["platform_version"].as_str().unwrap()));
            assert_eq!(
                config["navigator"]["platform_version"],
                config["client_hints"]["platform_version"]
            );
        }
    }
}
