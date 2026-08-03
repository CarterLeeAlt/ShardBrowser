use crate::{profile::ProfileMeta, proxy::ProxyEntry, store};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

const DISPLAY_ORDER_VERSION: u32 = 1;
static DISPLAY_ORDER_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DisplayOrder {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    profiles: Vec<String>,
    #[serde(default)]
    proxies: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    Before,
    After,
}

#[derive(Debug, Clone, Copy)]
enum NewItemPlacement {
    Front,
    Back,
}

fn load_unlocked() -> Result<DisplayOrder> {
    let path = store::display_order_path()?;
    if !path.exists() {
        return Ok(DisplayOrder::default());
    }
    store::load_json_with_backup(&path)
        .with_context(|| format!("failed to load {}", path.display()))
}

fn save_unlocked(order: &DisplayOrder) -> Result<()> {
    let path = store::display_order_path()?;
    let mut output = order.clone();
    output.version = DISPLAY_ORDER_VERSION;
    let body = serde_json::to_vec_pretty(&output)?;
    store::atomic_write(&path, &body)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Merge a possibly-stale saved order with the current default order.
/// Newly-created ids stay in default order at the requested edge, missing ids
/// are discarded, and duplicates are ignored.
fn reconcile_order(
    saved: &[String],
    defaults: &[String],
    new_item_placement: NewItemPlacement,
) -> Vec<String> {
    let valid: HashSet<&str> = defaults.iter().map(String::as_str).collect();
    let mut saved_seen: HashSet<String> = HashSet::new();
    let saved_valid: Vec<String> = saved
        .iter()
        .filter(|id| valid.contains(id.as_str()) && saved_seen.insert((*id).clone()))
        .cloned()
        .collect();
    let saved_ids: HashSet<String> = saved_valid.iter().cloned().collect();
    let new_valid: Vec<String> = defaults
        .iter()
        .filter(|id| !saved_ids.contains(id.as_str()))
        .cloned()
        .collect();
    let (first, second) = match new_item_placement {
        NewItemPlacement::Front => (new_valid, saved_valid),
        NewItemPlacement::Back => (saved_valid, new_valid),
    };

    let mut result = Vec::with_capacity(defaults.len());
    let mut result_seen: HashSet<String> = HashSet::new();
    for id in first.into_iter().chain(second) {
        if result_seen.insert(id.clone()) {
            result.push(id);
        }
    }
    result
}

fn move_in_order(
    saved: &[String],
    defaults: &[String],
    new_item_placement: NewItemPlacement,
    id: &str,
    anchor_id: Option<&str>,
    placement: Placement,
) -> Result<Vec<String>> {
    let mut order = reconcile_order(saved, defaults, new_item_placement);
    let Some(source_index) = order.iter().position(|current| current == id) else {
        anyhow::bail!("item no longer exists");
    };
    if anchor_id == Some(id) {
        return Ok(order);
    }
    if let Some(anchor) = anchor_id {
        if !order.iter().any(|current| current == anchor) {
            anyhow::bail!("drop target no longer exists");
        }
    }

    order.remove(source_index);
    let insert_at = match anchor_id {
        Some(anchor) => {
            let anchor_index = order
                .iter()
                .position(|current| current == anchor)
                .expect("anchor was validated before source removal");
            match placement {
                Placement::Before => anchor_index,
                Placement::After => anchor_index + 1,
            }
        }
        None => match placement {
            Placement::Before => 0,
            Placement::After => order.len(),
        },
    };
    order.insert(insert_at, id.to_string());
    Ok(order)
}

pub fn sort_profiles(profiles: Vec<ProfileMeta>) -> Result<Vec<ProfileMeta>> {
    let _guard = DISPLAY_ORDER_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("display order lock poisoned"))?;
    let saved = load_unlocked()?;
    let defaults: Vec<String> = profiles.iter().map(|profile| profile.id.clone()).collect();
    let order = reconcile_order(
        &saved.profiles,
        &defaults,
        NewItemPlacement::Front,
    );
    let mut by_id: HashMap<String, ProfileMeta> = profiles
        .into_iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect();
    Ok(order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect())
}

pub fn sort_proxies(proxies: Vec<ProxyEntry>) -> Result<Vec<ProxyEntry>> {
    let _guard = DISPLAY_ORDER_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("display order lock poisoned"))?;
    let saved = load_unlocked()?;
    let defaults: Vec<String> = proxies.iter().map(|proxy| proxy.id.clone()).collect();
    let order = reconcile_order(&saved.proxies, &defaults, NewItemPlacement::Back);
    let mut by_id: HashMap<String, ProxyEntry> = proxies
        .into_iter()
        .map(|proxy| (proxy.id.clone(), proxy))
        .collect();
    Ok(order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect())
}

pub fn move_profile(
    default_ids: &[String],
    id: &str,
    anchor_id: Option<&str>,
    placement: Placement,
) -> Result<()> {
    let _guard = DISPLAY_ORDER_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("display order lock poisoned"))?;
    let mut saved = load_unlocked()?;
    saved.profiles = move_in_order(
        &saved.profiles,
        default_ids,
        NewItemPlacement::Front,
        id,
        anchor_id,
        placement,
    )?;
    save_unlocked(&saved)
}

/// Append a batch of newly-created profiles to the saved order in one atomic
/// write. The caller's id order is preserved.
pub fn append_profiles(default_ids: &[String], ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let valid: HashSet<&str> = default_ids.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    for id in ids {
        if !valid.contains(id.as_str()) {
            anyhow::bail!("profile no longer exists");
        }
        if !seen.insert(id.as_str()) {
            anyhow::bail!("duplicate profile id");
        }
    }

    let _guard = DISPLAY_ORDER_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("display order lock poisoned"))?;
    let mut saved = load_unlocked()?;
    let mut order = reconcile_order(
        &saved.profiles,
        default_ids,
        NewItemPlacement::Front,
    );
    order.retain(|current| !seen.contains(current.as_str()));
    order.extend(ids.iter().cloned());
    saved.profiles = order;
    save_unlocked(&saved)
}

pub fn move_proxy(
    default_ids: &[String],
    id: &str,
    anchor_id: Option<&str>,
    placement: Placement,
) -> Result<()> {
    let _guard = DISPLAY_ORDER_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("display order lock poisoned"))?;
    let mut saved = load_unlocked()?;
    saved.proxies = move_in_order(
        &saved.proxies,
        default_ids,
        NewItemPlacement::Back,
        id,
        anchor_id,
        placement,
    )?;
    save_unlocked(&saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn reconcile_keeps_new_items_first_and_prunes_stale_ids() {
        assert_eq!(
            reconcile_order(
                &ids(&["b", "missing", "a", "b"]),
                &ids(&["c", "b", "a"]),
                NewItemPlacement::Front,
            ),
            ids(&["c", "b", "a"]),
        );
    }

    #[test]
    fn proxy_reconcile_appends_new_items_in_default_order() {
        assert_eq!(
            reconcile_order(
                &ids(&["existing-b", "existing-a"]),
                &ids(&["existing-a", "existing-b", "new-1", "new-2"]),
                NewItemPlacement::Back,
            ),
            ids(&["existing-b", "existing-a", "new-1", "new-2"]),
        );
    }

    #[test]
    fn proxy_move_keeps_unmoved_new_items_at_the_back() {
        let moved = move_in_order(
            &ids(&["existing-a", "existing-b"]),
            &ids(&["existing-a", "existing-b", "new"]),
            NewItemPlacement::Back,
            "existing-a",
            Some("existing-b"),
            Placement::After,
        )
        .unwrap();

        assert_eq!(moved, ids(&["existing-b", "existing-a", "new"]));
    }

    #[test]
    fn move_supports_before_and_after_anchors() {
        let defaults = ids(&["a", "b", "c", "d"]);
        let before = move_in_order(
            &[],
            &defaults,
            NewItemPlacement::Front,
            "d",
            Some("b"),
            Placement::Before,
        )
        .unwrap();
        assert_eq!(before, ids(&["a", "d", "b", "c"]));
        let after = move_in_order(
            &before,
            &defaults,
            NewItemPlacement::Front,
            "a",
            Some("c"),
            Placement::After,
        )
        .unwrap();
        assert_eq!(after, ids(&["d", "b", "c", "a"]));
    }

    #[test]
    fn move_without_anchor_supports_list_edges() {
        let defaults = ids(&["new", "a", "b"]);
        let end = move_in_order(
            &ids(&["a", "b"]),
            &defaults,
            NewItemPlacement::Front,
            "new",
            None,
            Placement::After,
        )
        .unwrap();
        assert_eq!(end, ids(&["a", "b", "new"]));
        let start = move_in_order(
            &end,
            &defaults,
            NewItemPlacement::Front,
            "b",
            None,
            Placement::Before,
        )
        .unwrap();
        assert_eq!(start, ids(&["b", "a", "new"]));
    }

    #[test]
    fn move_rejects_missing_source_or_anchor() {
        let defaults = ids(&["a", "b"]);
        assert!(move_in_order(
            &[],
            &defaults,
            NewItemPlacement::Front,
            "missing",
            Some("a"),
            Placement::Before,
        )
        .is_err());
        assert!(move_in_order(
            &[],
            &defaults,
            NewItemPlacement::Front,
            "a",
            Some("missing"),
            Placement::Before,
        )
        .is_err());
    }
}
