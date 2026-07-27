// ShardX Launcher — Tauri backend.

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
compile_error!("ShardX Launcher supports only Windows x64 (x86_64-pc-windows-msvc)");

mod api;
mod cookies;
mod fingerprints;
mod launch;
mod mcp_setup;
mod pixel_font_data;
mod process;
mod profile;
mod proxy;
mod runtime;
mod settings;
mod store;
mod taskbar_icon;

use serde_json::Value;

/// App handle set in `run()` setup; lets the axum API reach a webview window.
static APP_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();
static HOST_LOGICAL_PROCESSORS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
static HOST_RAM_BUCKET_GB: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

pub fn app_handle() -> Option<&'static tauri::AppHandle> {
    APP_HANDLE.get()
}

/// Launcher's own webview window (for monitor queries); None when headless.
pub fn main_window() -> Option<tauri::WebviewWindow> {
    use tauri::Manager;
    let app = APP_HANDLE.get()?;
    app.get_webview_window("main")
        .or_else(|| app.webview_windows().into_values().next())
}

/// Tell any open UI window that the on-disk store changed out-of-band — i.e. a
/// profile/proxy created or removed through the automation API or MCP, which
/// writes straight to disk without the React state ever knowing.  The view
/// listens for `store-changed` and reloads, so the new items appear without an
/// app restart.  `kind` ("profiles" | "proxies") is informational; the UI
/// reloads both lists regardless.  No-op when headless (no window).
pub fn notify_store_changed(kind: &str) {
    use tauri::Emitter;
    if let Some(w) = main_window() {
        let _ = w.emit("store-changed", kind);
    }
}

// ---- MCP server download ----

/// Download MCP server source into the portable app data directory.
#[tauri::command]
async fn mcp_download() -> Result<String, String> {
    mcp_setup::download_mcp()
        .await
        .map(|p| p.display().to_string())
        .map_err(|e| e.to_string())
}

// ---- Profiles ----

#[tauri::command]
fn profile_list() -> Result<Vec<profile::ProfileMeta>, String> {
    profile::list_all().map_err(|e| e.to_string())
}

#[tauri::command]
fn profile_get(id: String) -> Result<Value, String> {
    let mut stored = profile::load_raw(&id).map_err(|e| e.to_string())?;
    // Backfill gpu_preset_id for legacy profiles by matching webgl.renderer.
    if stored.meta.gpu_preset_id.is_none() {
        if let Some(gid) = infer_gpu_preset_id(&stored.config) {
            stored.meta.gpu_preset_id = Some(gid);
            let _ = profile::save_raw(&mut stored);
        }
    }
    serde_json::to_value(stored).map_err(|e| e.to_string())
}

/// Recover library fingerprint id by matching webgl.renderer (+ screen if ambiguous).
fn infer_gpu_preset_id(config: &serde_json::Map<String, Value>) -> Option<String> {
    let renderer = config.get("webgl")?.get("renderer")?.as_str()?;
    let scr = config.get("screen");
    let sw = scr.and_then(|s| s.get("width")).and_then(|v| v.as_i64());
    let sh = scr.and_then(|s| s.get("height")).and_then(|v| v.as_i64());

    let entries = fingerprints::list_all().ok()?;
    let mut renderer_match: Option<String> = None;
    for e in &entries {
        let er = e
            .payload
            .get("webgl")
            .and_then(|w| w.get("renderer"))
            .and_then(|v| v.as_str());
        if er != Some(renderer) {
            continue;
        }
        let es = e.payload.get("screen");
        let ew = es.and_then(|s| s.get("width")).and_then(|v| v.as_i64());
        let eh = es.and_then(|s| s.get("height")).and_then(|v| v.as_i64());
        if sw.is_some() && ew == sw && eh == sh {
            return Some(e.id.clone());
        }
        renderer_match.get_or_insert_with(|| e.id.clone());
    }
    renderer_match
}

// ---- Realistic Sec-CH-UA-Platform-Version pools (spread per profile) ----

// macOS Sonoma 14.x, Sequoia 15.x, Tahoe 26.x.
const MACOS_PLATFORM_VERSIONS: &[&str] = &[
    "14.6.1", "14.7", "14.7.1", "14.7.2",
    "15.4", "15.4.1", "15.5", "15.6", "15.6.1", "15.7",
    "26.0", "26.0.1", "26.1",
];

// Win 10 21H1+ ("10.0.0"), Win 11 21H2..25H2 ("13"–"17"); weighted to 22H2/23H2/24H2.
const WINDOWS_PLATFORM_VERSIONS: &[&str] = &[
    "10.0.0",
    "13.0.0",
    "14.0.0", "14.0.0", "14.0.0",
    "15.0.0", "15.0.0", "15.0.0", "15.0.0",
    "16.0.0", "16.0.0", "16.0.0",
    "17.0.0",
];

// LTS kernels + current mainline.
const LINUX_PLATFORM_VERSIONS: &[&str] = &[
    "5.15.0", "6.1.0", "6.5.0",
    "6.6.0", "6.8.0", "6.10.0", "6.11.0", "6.12.0",
    "6.14.0", "6.15.0", "6.16.0",
];

/// Write a random platform_version into navigator + client_hints; unknown platforms left alone.
pub(crate) fn randomize_platform_version(payload: &mut serde_json::Map<String, Value>) {
    let platform = payload
        .get("navigator")
        .and_then(|n| n.get("platform"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let pool: &[&str] = match platform {
        "macOS"   => MACOS_PLATFORM_VERSIONS,
        "Windows" => WINDOWS_PLATFORM_VERSIONS,
        "Linux"   => LINUX_PLATFORM_VERSIONS,
        _         => return,
    };
    let pick_idx = (uuid::Uuid::new_v4().as_bytes()[0] as usize) % pool.len();
    let version = pool[pick_idx].to_string();

    if let Some(nav) = payload.get_mut("navigator").and_then(|v| v.as_object_mut()) {
        nav.insert("platform_version".into(), Value::String(version.clone()));
    }
    if let Some(ch) = payload.get_mut("client_hints").and_then(|v| v.as_object_mut()) {
        ch.insert("platform_version".into(), Value::String(version));
    }
}

/// Realistic (hardware_concurrency, deviceMemory) combos per Mac model id.
fn mac_hw_configs(model: &str) -> Option<&'static [(u32, u32)]> {
    Some(match model {
        "mac-m1-air13" | "mac-m1-mbp13" | "mac-m1-imac24" => &[(8, 8), (8, 16)],
        "mac-m1-pro-mbp14" | "mac-m1-pro-mbp16" => &[(8, 16), (10, 16), (10, 32)],
        "mac-m1-max-mbp14" | "mac-m1-max-mbp16" => &[(10, 32)],
        "mac-m2-air13" | "mac-m2-air15" | "mac-m2-mbp13" => &[(8, 8), (8, 16)],
        "mac-m2-pro-mbp14" | "mac-m2-pro-mbp16" => &[(10, 16), (12, 16), (12, 32)],
        "mac-m2-max-mbp14" | "mac-m2-max-mbp16" => &[(12, 32)],
        "mac-m3-air13" | "mac-m3-air15" | "mac-m3-mbp14" | "mac-m3-imac24" => {
            &[(8, 8), (8, 16)]
        }
        "mac-m3-pro-mbp14" | "mac-m3-pro-mbp16" => &[(11, 16), (12, 16), (12, 32)],
        "mac-m3-max-mbp14" | "mac-m3-max-mbp16" => &[(14, 32), (16, 32)],
        "mac-m4-air13" | "mac-m4-air15" | "mac-m4-mbp14" | "mac-m4-imac24" => {
            &[(10, 16), (10, 32)]
        }
        "mac-m4-pro-mbp14" | "mac-m4-pro-mbp16" => &[(12, 16), (14, 16), (14, 32)],
        "mac-m4-max-mbp14" | "mac-m4-max-mbp16" => &[(14, 32), (16, 32)],
        "mac-m5-mbp14" => &[(10, 16), (10, 32)],
        _ => return None,
    })
}

/// A valid logical-processor / device-memory pairing for one fingerprint.
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardwareConfig {
    pub hardware_concurrency: u32,
    pub device_memory: u32,
}

/// Host logical CPU count (counts SMT threads); fallback 8.
fn host_logical_cores() -> u32 {
    *HOST_LOGICAL_PROCESSORS.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(8)
    })
}

/// Host physical RAM in GiB from the native Windows API.
fn host_ram_gb() -> Option<u32> {
    use windows_sys::Win32::System::SystemInformation::{
        GlobalMemoryStatusEx, MEMORYSTATUSEX,
    };

    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return None;
    }
    Some((status.ullTotalPhys / (1024 * 1024 * 1024)) as u32)
}

/// Physical RAM rounded to Chrome's {8,16,32} deviceMemory bucket; unknown → 16.
fn host_ram_bucket_gb() -> u32 {
    *HOST_RAM_BUCKET_GB.get_or_init(|| match host_ram_gb() {
        Some(gb) if gb >= 32 => 32,
        Some(gb) if gb >= 16 => 16,
        Some(_) => 8,
        None => 16,
    })
}

fn hardware_from_payload(payload: &serde_json::Map<String, Value>) -> Option<HardwareConfig> {
    let nav = payload.get("navigator")?.as_object()?;
    let hardware_concurrency = nav.get("hardware_concurrency")?.as_u64()? as u32;
    let device_memory = nav.get("device_memory")?.as_u64()? as u32;
    Some(HardwareConfig {
        hardware_concurrency,
        device_memory,
    })
}

/// The canonical hardware choices shared by generation, validation and the UI.
fn hardware_configs(
    model: &str,
    platform: &str,
    donor: Option<HardwareConfig>,
) -> Vec<HardwareConfig> {
    if let Some(pool) = mac_hw_configs(model) {
        return pool
            .iter()
            .map(|&(hardware_concurrency, device_memory)| HardwareConfig {
                hardware_concurrency,
                device_memory,
            })
            .collect();
    }

    if platform == "Windows" || platform == "Linux" {
        let host_cores = host_logical_cores();
        // Logical-processor counts seen on current x86 CPUs (SMT + hybrid cores).
        const X86_LOGICAL_PROCESSORS: [u32; 9] = [4, 6, 8, 12, 16, 20, 24, 28, 32];
        let lo = host_cores.saturating_sub(4);
        let hi = host_cores.saturating_add(2);
        let mut core_candidates: Vec<u32> = X86_LOGICAL_PROCESSORS
            .into_iter()
            .filter(|&n| n >= lo && n <= hi)
            .collect();
        if core_candidates.is_empty() {
            if let Some(nearest) = X86_LOGICAL_PROCESSORS
                .into_iter()
                .min_by_key(|&n| n.abs_diff(host_cores))
            {
                core_candidates.push(nearest);
            }
        }

        let host_memory = host_ram_bucket_gb();
        let mut out = Vec::new();
        for hardware_concurrency in core_candidates {
            // Keep memory at or below the host, with 16 GiB preferred for
            // higher-thread-count machines. Low-memory hosts remain usable.
            let floor = if hardware_concurrency >= 12 { 16 } else { 8 };
            let mut memory_candidates: Vec<u32> = [8u32, 16, 32]
                .into_iter()
                .filter(|&m| m >= floor && m <= host_memory)
                .collect();
            if memory_candidates.is_empty() {
                memory_candidates.push(host_memory);
            }
            out.extend(memory_candidates.into_iter().map(|device_memory| HardwareConfig {
                hardware_concurrency,
                device_memory,
            }));
        }
        return out;
    }

    // User-imported fingerprints outside the curated model tables retain
    // their donor pairing as a single coherent option.
    donor.into_iter().collect()
}

fn hardware_configs_for_entry(entry: &fingerprints::LibraryEntry) -> Vec<HardwareConfig> {
    let platform = entry
        .payload
        .get("navigator")
        .and_then(|n| n.get("platform"))
        .and_then(|v| v.as_str())
        .unwrap_or(&entry.platform);
    let donor = entry.payload.as_object().and_then(hardware_from_payload);
    hardware_configs(&entry.id, platform, donor)
}

fn hardware_configs_for_preset(preset_id: &str) -> Result<Vec<HardwareConfig>, String> {
    let entry = fingerprints::get(preset_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("unknown fingerprint id: {preset_id}"))?;
    let configs = hardware_configs_for_entry(&entry);
    if configs.is_empty() {
        return Err(format!("no hardware configurations available for fingerprint: {preset_id}"));
    }
    Ok(configs)
}

fn validate_hardware_selection(payload: &serde_json::Map<String, Value>) -> Result<(), String> {
    let preset_id = payload
        .get("_meta")
        .and_then(|m| m.get("gpu_preset_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("gpu_preset_id is required for hardware validation")?;
    let selected = hardware_from_payload(payload)
        .ok_or("hardware_concurrency and device_memory are required")?;
    let configs = hardware_configs_for_preset(preset_id)?;
    if !configs.contains(&selected) {
        return Err(format!(
            "invalid hardware combination for {preset_id}: {} logical processors / {} GiB memory",
            selected.hardware_concurrency, selected.device_memory
        ));
    }
    Ok(())
}

/// Pick (hardware_concurrency, device_memory): Mac → curated table, Win/Linux → host-bracketed.
pub(crate) fn randomize_hardware(payload: &mut serde_json::Map<String, Value>) {
    let model = payload
        .get("_meta")
        .and_then(|m| m.get("gpu_preset_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let platform = payload
        .get("navigator")
        .and_then(|n| n.get("platform"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let donor = hardware_from_payload(payload);
    let configs = hardware_configs_for_preset(model)
        .unwrap_or_else(|_| hardware_configs(model, platform, donor));
    if configs.is_empty() {
        return;
    }
    let pick = configs[(uuid::Uuid::new_v4().as_bytes()[0] as usize) % configs.len()];

    if let Some(nav) = payload.get_mut("navigator").and_then(|v| v.as_object_mut()) {
        nav.insert(
            "hardware_concurrency".into(),
            Value::from(pick.hardware_concurrency),
        );
        nav.insert("device_memory".into(), Value::from(pick.device_memory));
    }
}

/// Keep profile.screen aligned with the real Windows display.
pub(crate) fn clamp_screen_to_real_display(
    window: &tauri::WebviewWindow,
    payload: &mut serde_json::Map<String, Value>,
) {
    let Some(monitor) = window
        .primary_monitor()
        .ok()
        .flatten()
        .or_else(|| window.current_monitor().ok().flatten())
    else {
        eprintln!("[launcher] display: no monitor info — screen clamp skipped");
        return;
    };
    let scale = monitor.scale_factor();
    if scale <= 0.0 {
        eprintln!("[launcher] display: bad scale_factor {scale} — screen clamp skipped");
        return;
    }
    let phys = monitor.size();
    let real_w = (phys.width as f64 / scale).round() as i64;
    let real_h = (phys.height as f64 / scale).round() as i64;
    eprintln!(
        "[launcher] display: name={:?} physical={}x{} scale={} -> logical={}x{}",
        monitor.name(), phys.width, phys.height, scale, real_w, real_h
    );
    if real_w <= 0 || real_h <= 0 {
        return;
    }

    let Some(scr) = payload.get("screen").and_then(|v| v.as_object()) else {
        eprintln!("[launcher] display: profile has no `screen` block — clamp skipped");
        return;
    };
    let fp_w = scr.get("width").and_then(|v| v.as_i64()).unwrap_or(0);
    let fp_h = scr.get("height").and_then(|v| v.as_i64()).unwrap_or(0);
    eprintln!("[launcher] display: fingerprint screen={fp_w}x{fp_h}");
    if fp_w <= 0 || fp_h <= 0 {
        return;
    }
    // Preserve FP menubar/dock insets for avail_*.
    let fp_avail_w = scr.get("avail_width").and_then(|v| v.as_i64()).unwrap_or(fp_w);
    let fp_avail_h = scr.get("avail_height").and_then(|v| v.as_i64()).unwrap_or(fp_h);
    let chrome_w = (fp_w - fp_avail_w).max(0);
    let chrome_h = (fp_h - fp_avail_h).max(0);
    let avail_w = (real_w - chrome_w).max(1);
    let avail_h = (real_h - chrome_h).max(1);

    if let Some(scr_mut) = payload.get_mut("screen").and_then(|v| v.as_object_mut()) {
        scr_mut.insert("width".into(), Value::from(real_w));
        scr_mut.insert("height".into(), Value::from(real_h));
        scr_mut.insert("avail_width".into(), Value::from(avail_w));
        scr_mut.insert("avail_height".into(), Value::from(avail_h));
        scr_mut.insert("device_pixel_ratio".into(), Value::from(scale));
    }
    // Keep window inside the avail area.
    if let Some(win) = payload.get_mut("window").and_then(|v| v.as_object_mut()) {
        win.insert("outer_width".into(), Value::from(avail_w));
        win.insert("inner_width".into(), Value::from(avail_w));
        let outer_h = (avail_h - 1).max(1);
        win.insert("outer_height".into(), Value::from(outer_h));
        win.insert("inner_height".into(), Value::from((outer_h - 87).max(1)));
    }
    eprintln!(
        "[launcher] display: CLAMPED screen to real {real_w}x{real_h} \
         (avail {avail_w}x{avail_h}, dpr {scale}) — FP claimed {fp_w}x{fp_h}"
    );
}

#[tauri::command]
fn profile_save(
    window: tauri::WebviewWindow,
    mut payload: Value,
) -> Result<profile::ProfileMeta, String> {
    let is_new = payload
        .get("_meta")
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.is_empty())
        .unwrap_or(true);
    let preset_changed = if is_new {
        false
    } else {
        let id = payload
            .get("_meta")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .ok_or("existing profile payload is missing its id")?;
        let incoming_preset = payload
            .get("_meta")
            .and_then(|m| m.get("gpu_preset_id"))
            .and_then(|v| v.as_str());
        profile::load_raw(id)
            .map_err(|e| e.to_string())?
            .meta
            .gpu_preset_id
            .as_deref()
            != incoming_preset
    };
    if is_new || preset_changed {
        let obj = payload
            .as_object_mut()
            .ok_or("profile payload must be an object")?;
        // A new fingerprint identity already contains one coherent hardware /
        // platform pick from Rust. Validate it and clamp its donor display to
        // the real monitor. Ordinary edits retain the existing durable
        // screen/window block and therefore skip this identity initialization.
        validate_hardware_selection(obj)?;
        clamp_screen_to_real_display(&window, obj);
    }
    save_profile_core(Some(&window), payload, false)
}

/// Enrich a new profile in place: platform_version, hardware, screen clamp.
pub fn enrich_new_config(
    window: Option<&tauri::WebviewWindow>,
    obj: &mut serde_json::Map<String, Value>,
) {
    randomize_platform_version(obj);
    randomize_hardware(obj);
    if let Some(w) = window {
        clamp_screen_to_real_display(w, obj);
    }
}

/// Core of `profile_save` callable without Tauri context; `enrich=false` stores verbatim.
pub fn save_profile_core(
    window: Option<&tauri::WebviewWindow>,
    payload: Value,
    enrich: bool,
) -> Result<profile::ProfileMeta, String> {
    let mut payload = payload;

    let is_new = payload
        .get("_meta")
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.is_empty())
        .unwrap_or(true);
    if is_new && enrich {
        if let Some(obj) = payload.as_object_mut() {
            enrich_new_config(window, obj);
        }
    }

    let mut stored: profile::StoredProfile =
        serde_json::from_value(payload).map_err(|e| e.to_string())?;
    let profile_name = stored
        .config
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // New profiles always validate. Existing legacy names are tolerated until
    // the name itself changes, so editing notes/proxy settings is not blocked.
    let name_changed = is_new
        || profile::load_raw(&stored.meta.id)
            .ok()
            .map(|existing| {
                existing
                    .config
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    != profile_name.as_str()
            })
            .unwrap_or(true);
    if name_changed {
        profile::validate_profile_name(&profile_name).map_err(|e| e.to_string())?;
    }
    if !is_new {
        profile::ensure_stopped(&stored.meta.id).map_err(|e| e.to_string())?;
    }
    profile::save_raw(&mut stored).map_err(|e| e.to_string())?;
    let name = stored
        .config
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("(unnamed)")
        .to_string();
    let notes = stored
        .config
        .get("notes")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(profile::ProfileMeta {
        id: stored.meta.id,
        name,
        notes,
        proxy_id: stored.meta.proxy_id,
        last_launched_at: stored.meta.last_launched_at,
        created_at: stored.meta.created_at,
        pinned: stored.meta.pinned,
        folder: stored.meta.folder,
        total_runtime_ms: stored.meta.total_runtime_ms,
    })
}

#[tauri::command]
fn profile_delete(id: String) -> Result<(), String> {
    profile::delete(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn profile_bind_proxy(profile_id: String, proxy_id: Option<String>) -> Result<(), String> {
    profile::ensure_stopped(&profile_id).map_err(|e| e.to_string())?;
    let mut p = profile::load_raw(&profile_id).map_err(|e| e.to_string())?;
    p.meta.proxy_id = proxy_id;
    profile::save_raw(&mut p).map_err(|e| e.to_string())
}

#[tauri::command]
fn profile_clone(id: String) -> Result<profile::ProfileMeta, String> {
    profile::clone_profile(&id).map_err(|e| e.to_string())
}

/// Import profiles verbatim under fresh ids; returns the count.
#[tauri::command]
fn profile_import(payloads: Vec<Value>) -> Result<usize, String> {
    // Validate the entire import before writing anything, avoiding a partial
    // import when a later profile contains an invalid name.
    for payload in &payloads {
        let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
        profile::validate_profile_name(name).map_err(|e| e.to_string())?;
    }
    let mut n = 0;
    for mut payload in payloads {
        if let Some(obj) = payload.as_object_mut() {
            match obj.get_mut("_meta").and_then(|m| m.as_object_mut()) {
                Some(meta) => {
                    meta.insert("id".into(), Value::String(String::new()));
                }
                None => {
                    obj.insert("_meta".into(), serde_json::json!({ "id": "" }));
                }
            }
        }
        save_profile_core(None, payload, false)?;
        n += 1;
    }
    Ok(n)
}

// ---- Clipboard (via tauri-plugin-clipboard-manager; webview navigator.clipboard throws) ----

#[tauri::command]
fn clipboard_write(app: tauri::AppHandle, text: String) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

#[tauri::command]
fn clipboard_read(app: tauri::AppHandle) -> Result<String, String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    app.clipboard().read_text().map_err(|e| e.to_string())
}

#[tauri::command]
fn profile_set_pin(id: String, pinned: bool) -> Result<(), String> {
    profile::set_pin(&id, pinned).map_err(|e| e.to_string())
}

#[tauri::command]
fn profile_set_folder(id: String, folder: String) -> Result<(), String> {
    profile::set_folder(&id, &folder).map_err(|e| e.to_string())
}

/// Rename folder (retag profiles); returns count.
#[tauri::command]
fn folder_rename(old: String, new: String) -> Result<usize, String> {
    profile::rename_folder(&old, &new).map_err(|e| e.to_string())
}

/// Delete folder; `delete_profiles` true → remove, false → unfile.
#[tauri::command]
fn folder_delete(folder: String, delete_profiles: bool) -> Result<usize, String> {
    profile::delete_folder(&folder, delete_profiles).map_err(|e| e.to_string())
}

/// Host OS in fingerprint-library vocabulary.
#[tauri::command]
fn host_platform() -> String {
    "Windows".to_string()
}

#[tauri::command]
fn profile_create_from_template(
    window: tauri::WebviewWindow,
    template_id: String,
) -> Result<profile::ProfileMeta, String> {
    create_from_fingerprint_core(Some(&window), &template_id)
}

/// Merge library fingerprint into fresh profile map; tz/lang/geo set to "auto" sentinel.
pub fn merge_library_fingerprint(
    template_id: &str,
) -> Result<serde_json::Map<String, Value>, String> {
    let entry = fingerprints::get(template_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("unknown fingerprint id: {template_id}"))?;

    let mut merged = serde_json::Map::new();
    merged.insert(
        "_meta".into(),
        serde_json::json!({
            "id": "",
            "proxy_id": null,
            "last_launched_at": null,
            "gpu_preset_id": entry.id,
        }),
    );
    if let Some(o) = entry.payload.as_object() {
        for (k, v) in o {
            if k == "_meta" { continue; }
            merged.insert(k.clone(), v.clone());
        }
    }
    let raw_name = merged.get("name").and_then(|v| v.as_str()).unwrap_or("");
    merged.insert(
        "name".into(),
        Value::String(profile::generated_profile_name(raw_name, template_id)),
    );

    // launch-time resolver fills tz/lang/geo from the bound proxy
    merged.insert("timezone".into(), Value::String("auto".into()));
    if let Some(nav) = merged.get_mut("navigator").and_then(|v| v.as_object_mut()) {
        nav.insert("language".into(), Value::String("auto".into()));
        nav.remove("accept_language");
        nav.remove("languages");
    }
    merged.insert("geolocation".into(), serde_json::json!({ "mode": "auto" }));
    Ok(merged)
}

/// Build + persist a profile from a library fingerprint id (UI template path).
pub fn create_from_fingerprint_core(
    window: Option<&tauri::WebviewWindow>,
    template_id: &str,
) -> Result<profile::ProfileMeta, String> {
    let merged = merge_library_fingerprint(template_id)?;
    save_profile_core(window, Value::Object(merged), true)
}

/// Produce uniquified fingerprint config WITHOUT persisting (API get-new-fingerprint).
pub fn build_fingerprint_config(
    window: Option<&tauri::WebviewWindow>,
    template_id: &str,
) -> Result<serde_json::Map<String, Value>, String> {
    let mut merged = merge_library_fingerprint(template_id)?;
    enrich_new_config(window, &mut merged);
    ensure_default_noise(&mut merged);
    Ok(merged)
}

/// Add the UI's default noise block (every vector present, disabled, seed 0 —
/// the sentinel `save_raw` fills per-profile) when a config carries none, so
/// API/SDK profiles match UI profiles and get a unique seed instead of none.
pub fn ensure_default_noise(cfg: &mut serde_json::Map<String, Value>) {
    if cfg.contains_key("noise") {
        return;
    }
    cfg.insert(
        "noise".into(),
        serde_json::json!({
            "canvas":       { "enabled": false, "seed": 0 },
            "webgl":        { "enabled": false, "seed": 0, "intensity": 0 },
            "audio":        { "enabled": false, "seed": 0 },
            "client_rects": { "enabled": false, "seed": 0, "max_offset": 0 },
            "sensors":      { "enabled": false, "seed": 0 },
            "fonts":        { "enabled": false, "seed": 0 }
        }),
    );
}

#[derive(serde::Serialize)]
pub struct PresetEnrichPicks {
    pub hardware_concurrency: u32,
    pub device_memory: u32,
    pub platform_version: Option<String>,
    pub hardware_configs: Vec<HardwareConfig>,
}

/// Editor preview: draw a fresh hw + platform_version triple from the same tables save uses.
#[tauri::command]
fn enrich_picks_for_preset(preset_id: String) -> Result<PresetEnrichPicks, String> {
    let entry = fingerprints::get(&preset_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("unknown fingerprint id: {preset_id}"))?;
    let hardware_configs = hardware_configs_for_entry(&entry);
    if hardware_configs.is_empty() {
        return Err(format!("no hardware configurations available for fingerprint: {preset_id}"));
    }
    let platform = entry
        .payload
        .get("navigator")
        .and_then(|n| n.get("platform"))
        .and_then(|v| v.as_str())
        .unwrap_or("macOS")
        .to_string();
    let mut payload = serde_json::Map::new();
    payload.insert(
        "_meta".into(),
        serde_json::json!({ "gpu_preset_id": preset_id }),
    );
    payload.insert(
        "navigator".into(),
        serde_json::json!({ "platform": platform }),
    );
    // Generate the editor's platform version and hardware exactly once.
    randomize_platform_version(&mut payload);
    let selected = hardware_configs
        [(uuid::Uuid::new_v4().as_bytes()[0] as usize) % hardware_configs.len()];
    if let Some(nav) = payload.get_mut("navigator").and_then(|v| v.as_object_mut()) {
        nav.insert(
            "hardware_concurrency".into(),
            Value::from(selected.hardware_concurrency),
        );
        nav.insert("device_memory".into(), Value::from(selected.device_memory));
    }
    let nav = payload
        .get("navigator")
        .and_then(|v| v.as_object())
        .ok_or("internal: navigator missing after randomize")?;
    let cores = nav
        .get("hardware_concurrency")
        .and_then(|v| v.as_u64())
        .ok_or("internal: hardware_concurrency missing")? as u32;
    let mem = nav
        .get("device_memory")
        .and_then(|v| v.as_u64())
        .ok_or("internal: device_memory missing")? as u32;
    let pv = nav
        .get("platform_version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(PresetEnrichPicks {
        hardware_concurrency: cores,
        device_memory: mem,
        platform_version: pv,
        hardware_configs,
    })
}

// ---- Fingerprint library ----

#[tauri::command]
fn fingerprint_list() -> Result<Vec<fingerprints::LibraryEntry>, String> {
    fingerprints::list_all().map_err(|e| e.to_string())
}

#[tauri::command]
fn fingerprint_get(id: String) -> Result<Option<fingerprints::LibraryEntry>, String> {
    fingerprints::get(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn fingerprint_import(json_text: String, id_hint: Option<String>) -> Result<fingerprints::LibraryEntry, String> {
    fingerprints::import(&json_text, id_hint).map_err(|e| e.to_string())
}

#[tauri::command]
fn fingerprint_delete(id: String) -> Result<(), String> {
    fingerprints::delete(&id).map_err(|e| e.to_string())
}

/// Open one of the launcher's own portable directories from trusted Rust.
/// Canonicalizing both paths prevents a junction/symlink inside the portable
/// tree from redirecting the opener to an external directory.
fn open_portable_directory(
    app: &tauri::AppHandle,
    directory: std::path::PathBuf,
) -> Result<(), String> {
    let root = store::config_root().map_err(|e| e.to_string())?;
    let canonical_root = std::fs::canonicalize(&root).map_err(|e| e.to_string())?;
    let canonical_directory = std::fs::canonicalize(&directory).map_err(|e| e.to_string())?;

    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let executable_directory = executable
        .parent()
        .ok_or_else(|| "executable directory unavailable".to_string())?;
    let canonical_executable_directory =
        std::fs::canonicalize(executable_directory).map_err(|e| e.to_string())?;
    if !canonical_root.starts_with(&canonical_executable_directory) {
        return Err("portable root resolves outside the executable directory".into());
    }

    if !canonical_directory.starts_with(&canonical_root) {
        return Err("refusing to open a directory outside the portable root".into());
    }

    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(directory.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Open the fixed portable fingerprint library directory.
#[tauri::command]
fn open_fingerprint_dir(app: tauri::AppHandle) -> Result<(), String> {
    let directory = store::fingerprints_dir().map_err(|e| e.to_string())?;
    open_portable_directory(&app, directory)
}

/// Open the fixed portable cookie-export directory.
#[tauri::command]
fn open_exports_dir(app: tauri::AppHandle) -> Result<(), String> {
    let directory = store::exports_dir().map_err(|e| e.to_string())?;
    open_portable_directory(&app, directory)
}

// ---- Process tracker ----

#[tauri::command]
fn process_list() -> Vec<process::RunningProfile> {
    process::Tracker::shared().running()
}

#[tauri::command]
async fn process_kill(profile_id: String) -> Result<bool, String> {
    process::Tracker::shared()
        .kill(&profile_id)
        .await
        .map_err(|e| e.to_string())
}

// ---- Proxies ----

const AUTOMATIC_PROXY_TEST_ATTEMPTS: usize = 3;
const PROXY_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(120);

async fn test_proxies_in_background(
    requests: Vec<proxy::PreparedProxyTest>,
    max_attempts: usize,
) {
    if requests.is_empty() {
        return;
    }

    let total = requests.len();
    let results = proxy::full_test_batch_background(requests, max_attempts).await;
    let incomplete = results
        .iter()
        .filter(|result| {
            result.snapshot.is_none()
                && result.error.as_deref() != Some(proxy::TEST_CANCELLED_ERROR)
        })
        .count();
    let failed = results
        .iter()
        .filter(|result| {
            result
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.tcp_ms.is_none() || snapshot.ip.is_empty())
        })
        .count();
    if failed > 0 || incomplete > 0 {
        eprintln!(
            "[launcher] automatic proxy test finished: {total} total, {failed} failed, {incomplete} incomplete"
        );
    }
    notify_store_changed("proxies");
}

fn spawn_automatic_proxy_tests(requests: Vec<proxy::PreparedProxyTest>) {
    if requests.is_empty() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        test_proxies_in_background(requests, AUTOMATIC_PROXY_TEST_ATTEMPTS).await;
    });
}

fn start_proxy_refresh_loop() {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(PROXY_REFRESH_INTERVAL).await;
            match proxy::prepare_all_proxy_tests() {
                Ok(requests) => test_proxies_in_background(requests, 1).await,
                Err(error) => {
                    eprintln!("[launcher] automatic proxy list refresh failed: {error}")
                }
            }
        }
    });
}

#[tauri::command]
fn proxy_list() -> Result<Vec<proxy::ProxyEntry>, String> {
    // Newest-first display order; internal paths still read raw on-disk order.
    let mut list = proxy::list().map_err(|e| e.to_string())?;
    list.reverse();
    Ok(list)
}

#[tauri::command]
fn proxy_save(entry: proxy::ProxyEntry) -> Result<proxy::ProxyEntry, String> {
    let (saved, _created, _changed, test) =
        proxy::upsert_with_status(entry).map_err(|e| e.to_string())?;
    if let Some(test) = test {
        spawn_automatic_proxy_tests(vec![test]);
    }
    Ok(saved)
}

#[tauri::command]
fn proxy_delete(id: String) -> Result<(), String> {
    proxy::delete(&id).map_err(|e| e.to_string())
}

#[tauri::command]
async fn proxy_check(entry: proxy::ProxyEntry) -> Result<u128, String> {
    proxy::probe(&entry).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn proxy_check_udp(entry: proxy::ProxyEntry) -> Result<u128, String> {
    proxy::probe_udp(&entry).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn proxy_geo(entry: proxy::ProxyEntry, provider: Option<String>) -> Result<proxy::GeoInfo, String> {
    proxy::geo_check(&entry, provider).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn proxy_full_test(entry: proxy::ProxyEntry) -> Result<proxy::TestSnapshot, String> {
    proxy::full_test(&entry).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn proxy_full_test_batch(entries: Vec<proxy::ProxyEntry>) -> Vec<proxy::BatchTestResult> {
    proxy::full_test_batch(entries).await
}

#[tauri::command]
fn proxy_history(id: String) -> Result<Vec<proxy::TestSnapshot>, String> {
    proxy::history(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn proxy_last_test(id: String) -> Option<proxy::TestSnapshot> {
    proxy::latest_test(&id)
}

#[tauri::command]
fn proxy_bulk_import(text: String, kind: String) -> Result<usize, String> {
    let default_kind = match kind.as_str() {
        "http" => proxy::ProxyKind::Http,
        "https" => proxy::ProxyKind::Https,
        _ => proxy::ProxyKind::Socks5,
    };
    let parsed = proxy::parse_bulk_strict(&text, default_kind).map_err(|e| e.to_string())?;
    let (added, tests) =
        proxy::bulk_save_with_entries(parsed).map_err(|e| e.to_string())?;
    let count = added.len();
    spawn_automatic_proxy_tests(tests);
    Ok(count)
}

/// Parse bulk-import text without saving (preview list with per-row test).
#[tauri::command]
fn proxy_bulk_parse(text: String, kind: String) -> Result<proxy::BulkParsePreview, String> {
    let default_kind = match kind.as_str() {
        "http" => proxy::ProxyKind::Http,
        "https" => proxy::ProxyKind::Https,
        _ => proxy::ProxyKind::Socks5,
    };
    proxy::preview_bulk(&text, default_kind).map_err(|e| e.to_string())
}

/// Persist pre-tested proxies (bulk dialog).
#[tauri::command]
fn proxy_bulk_save(entries: Vec<proxy::ProxyEntry>) -> Result<usize, String> {
    let (added, tests) =
        proxy::bulk_save_with_entries(entries).map_err(|e| e.to_string())?;
    let count = added.len();
    spawn_automatic_proxy_tests(tests);
    Ok(count)
}

// ---- Launcher ----

#[tauri::command]
async fn launch(profile_id: String) -> Result<u32, String> {
    // UI launches: no CDP, headed.
    launch::launch_profile(&profile_id, false, false)
        .await
        .map(|o| o.pid)
        .map_err(|e| e.to_string())
}

// ---- Cookies ----

/// True if profile has a running browser process.
pub fn is_profile_running(profile_id: &str) -> bool {
    process::Tracker::shared()
        .running()
        .iter()
        .any(|r| r.profile_id == profile_id)
}

#[tauri::command]
fn cookies_export(profile_id: String) -> Result<Vec<cookies::Cookie>, String> {
    cookies::export(&profile_id).map_err(|e| e.to_string())
}

/// Export cookies to a generated file under the portable exports directory.
#[tauri::command]
fn cookies_export_portable(profile_id: String) -> Result<usize, String> {
    if profile_id.is_empty()
        || !profile_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("invalid profile id".into());
    }
    let cookies = cookies::export(&profile_id).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&cookies).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let path = store::exports_dir()
        .map_err(|e| e.to_string())?
        .join(format!("{profile_id}-cookies-{stamp}.json"));
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(cookies.len())
}

#[tauri::command]
fn cookies_import(profile_id: String, cookies: Vec<cookies::Cookie>) -> Result<usize, String> {
    // Running browser would clobber the import on exit.
    if is_profile_running(&profile_id) {
        return Err("stop the profile before importing cookies".into());
    }
    cookies::import(&profile_id, &cookies).map_err(|e| e.to_string())
}

// ---- Settings ----

#[tauri::command]
fn settings_get() -> Result<settings::Settings, String> {
    settings::load().map_err(|e| e.to_string())
}

#[tauri::command]
fn settings_save(value: settings::Settings) -> Result<(), String> {
    settings::save(&value).map_err(|e| e.to_string())
}

// ---- Automation API ----

/// API connection info: base URL + permanent Bearer JWT (no raw key exposed).
#[tauri::command]
fn api_info() -> Result<Value, String> {
    let s = settings::ensure_secret().map_err(|e| e.to_string())?;
    let token = api::long_lived_token(&s.api_secret)?;
    Ok(serde_json::json!({
        "enabled": s.api_enabled,
        "port": s.api_port,
        "base_url": format!("http://127.0.0.1:{}", s.api_port),
        "token": token,
    }))
}

/// Rotate API secret; live-swap on running server invalidates prior tokens.
#[tauri::command]
fn api_regenerate_token() -> Result<Value, String> {
    let mut s = settings::load().map_err(|e| e.to_string())?;
    s.api_secret = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    settings::save(&s).map_err(|e| e.to_string())?;
    api::set_secret(&s.api_secret);
    let token = api::long_lived_token(&s.api_secret)?;
    Ok(serde_json::json!({
        "enabled": s.api_enabled,
        "port": s.api_port,
        "base_url": format!("http://127.0.0.1:{}", s.api_port),
        "token": token,
    }))
}

/// Bring the main window back from the tray / minimized state and focus it.
fn show_main_window(app: &tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[derive(Clone, serde::Serialize)]
struct ExitBlockedPayload {
    running_count: usize,
}

/// Refuse normal launcher exit paths while a ShardX browser child is alive.
/// The backend tracker is authoritative, so this protection does not depend on
/// the frontend's two-second process poll being current.
fn block_exit_if_browsers_running(app: &tauri::AppHandle) -> bool {
    use tauri::Emitter;

    let running_count = process::Tracker::shared().running().len();
    if running_count == 0 {
        return false;
    }

    eprintln!(
        "[launcher] exit blocked: {running_count} browser process(es) still running"
    );
    show_main_window(app);
    let _ = app.emit(
        "launcher:exit-blocked",
        ExitBlockedPayload { running_count },
    );
    true
}

pub fn run() {
    tauri::Builder::default()
        // Must be the first plugin: a second launch focuses the running window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .on_window_event(|window, event| {
            use tauri::Manager;

            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let to_tray = settings::load()
                        .map(|s| s.minimize_to_tray)
                        .unwrap_or(true);
                    if to_tray {
                        api.prevent_close();
                        let _ = window.hide();
                    } else if block_exit_if_browsers_running(window.app_handle()) {
                        // With minimize-to-tray disabled this close request is
                        // a real process exit, so the browser guard applies.
                        api.prevent_close();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            profile_list,
            profile_get,
            profile_save,
            profile_delete,
            profile_bind_proxy,
            profile_clone,
            profile_import,
            clipboard_write,
            clipboard_read,
            profile_set_pin,
            profile_set_folder,
            folder_rename,
            folder_delete,
            host_platform,
            profile_create_from_template,
            enrich_picks_for_preset,
            fingerprint_list,
            fingerprint_get,
            fingerprint_import,
            fingerprint_delete,
            open_fingerprint_dir,
            open_exports_dir,
            process_list,
            process_kill,
            proxy_list,
            proxy_save,
            proxy_delete,
            proxy_check,
            proxy_check_udp,
            proxy_geo,
            proxy_full_test,
            proxy_full_test_batch,
            proxy_history,
            proxy_last_test,
            proxy_bulk_import,
            proxy_bulk_parse,
            proxy_bulk_save,
            launch,
            settings_get,
            settings_save,
            api_info,
            api_regenerate_token,
            cookies_export,
            cookies_export_portable,
            cookies_import,
            mcp_download,
            runtime::runtime_check_updates,
            runtime::runtime_local_status,
            runtime::runtime_install,
        ])
        .setup(|app| {
            // The configured main window uses `create: false`, so it must be
            // created here. On Windows, explicitly use an absolute WebView2
            // data directory beside the executable; otherwise Tauri defaults
            // to `%LOCALAPPDATA%\com.shardx.launcher`.
            let window_config = app
                .config()
                .app
                .windows
                .first()
                .cloned()
                .expect("main window configuration missing");
            let mut window_builder =
                tauri::WebviewWindowBuilder::from_config(app.handle(), &window_config)?;

            let launcher_root = {
                let exe_dir = std::env::current_exe()?
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "executable directory unavailable",
                        )
                    })?;
                let launcher_root = exe_dir.join("shardx-launcher");
                let webview_data = launcher_root.join("webview2");
                std::fs::create_dir_all(&webview_data)?;
                window_builder = window_builder.data_directory(webview_data);
                launcher_root
            };

            let main_window = window_builder.build()?;
            taskbar_icon::apply_launcher_taskbar_icon(
                main_window.hwnd()?.0 as isize,
                &launcher_root.join("icons"),
            )?;
            let _ = APP_HANDLE.set(app.handle().clone());

            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
                let show = MenuItem::with_id(app, "tray_show", "Show Launcher", true, None::<&str>)?;
                let quit = MenuItem::with_id(app, "tray_quit", "Quit", true, None::<&str>)?;
                let menu = Menu::with_items(app, &[&show, &quit])?;
                if let Some(icon) = app.default_window_icon().cloned() {
                    TrayIconBuilder::with_id("main")
                        .icon(icon)
                        .tooltip("ShardX Launcher")
                        .menu(&menu)
                        .show_menu_on_left_click(false)
                        .on_menu_event(|app, e| match e.id.as_ref() {
                            "tray_show" => show_main_window(app),
                            "tray_quit" => {
                                if !block_exit_if_browsers_running(app) {
                                    app.exit(0);
                                }
                            }
                            _ => {}
                        })
                        .on_tray_icon_event(|tray, e| {
                            if let TrayIconEvent::Click {
                                button: MouseButton::Left,
                                button_state: MouseButtonState::Up,
                                ..
                            } = e
                            {
                                show_main_window(tray.app_handle());
                            }
                        })
                        .build(app)?;
                }
            }

            // The custom Windows titlebar owns the full client area.
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_decorations(false);
            }

            // Clean up temporary profiles from crashed runs.
            match profile::purge_temporary() {
                Ok(n) if n > 0 => eprintln!("[launcher] purged {n} stale temporary profile(s)"),
                Ok(_) => {}
                Err(e) => eprintln!("[launcher] temporary purge failed: {e}"),
            }

            // Keep stored proxy information fresh while the launcher remains
            // running (including when its window is hidden to the tray).
            start_proxy_refresh_loop();

            // API task on the shared tokio runtime.
            match settings::ensure_secret() {
                Ok(s) if s.api_enabled => {
                    let (secret, port) = (s.api_secret.clone(), s.api_port);
                    tauri::async_runtime::spawn(async move {
                        api::serve(secret, port).await;
                    });
                }
                Ok(_) => eprintln!("[launcher] automation API disabled in settings"),
                Err(e) => eprintln!("[launcher] API secret init failed: {e}"),
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
