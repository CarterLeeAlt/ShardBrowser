// Local automation HTTP API (axum) for ShardX Launcher.
// 127.0.0.1:<api_port>; every endpoint except /health requires Bearer JWT (HS256).

use std::sync::{OnceLock, RwLock};

use axum::{
    extract::{Path, Query, Request},
    http::{header::AUTHORIZATION, header::HOST, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

// ---- HS256 secret (process-global so live rotation invalidates old tokens) ----

fn secret_cell() -> &'static RwLock<String> {
    static SECRET: OnceLock<RwLock<String>> = OnceLock::new();
    SECRET.get_or_init(|| RwLock::new(String::new()))
}

/// Install/replace the signing secret.
pub fn set_secret(s: &str) {
    if let Ok(mut g) = secret_cell().write() {
        *g = s.to_string();
    }
}

fn read_secret() -> String {
    secret_cell().read().map(|g| g.clone()).unwrap_or_default()
}

// ---- JWT ----

#[derive(serde::Serialize, serde::Deserialize)]
struct Claims {
    sub: String,
    iat: u64,
    exp: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mint(secret: &str, ttl_secs: u64) -> Result<String, String> {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    let now = unix_now();
    let claims = Claims {
        sub: "shardx-api".into(),
        iat: now,
        exp: now.saturating_add(ttl_secs),
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| e.to_string())
}

fn verify(secret: &str, token: &str) -> bool {
    use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::new(Algorithm::HS256),
    )
    .is_ok()
}

/// 10-year token shown in Settings UI.
pub fn long_lived_token(secret: &str) -> Result<String, String> {
    mint(secret, 60 * 60 * 24 * 365 * 10)
}

// ---- error type ----

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = match self.0 {
            StatusCode::CONFLICT => "conflict",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::BAD_REQUEST => "invalid_input",
            _ => "internal_error",
        };
        (self.0, Json(json!({ "error": self.1, "code": code }))).into_response()
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(code, msg.into())
}

type ApiResult = Result<Json<Value>, ApiError>;

fn storage_error(error: anyhow::Error) -> ApiError {
    use crate::errors::Failure;
    let code = if let Some(failure) = error.downcast_ref::<Failure>() {
        match failure {
            Failure::Conflict(_) => StatusCode::CONFLICT,
            Failure::InvalidInput(_) => StatusCode::BAD_REQUEST,
            Failure::NotFound(_) => StatusCode::NOT_FOUND,
        }
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    err(code, format!("{error:#}"))
}

#[derive(Clone, Default, serde::Serialize)]
pub struct ListenerStatus {
    state: &'static str,
    port: Option<u16>,
    error: Option<String>,
}
fn listener_cell() -> &'static RwLock<ListenerStatus> {
    static STATUS: OnceLock<RwLock<ListenerStatus>> = OnceLock::new();
    STATUS.get_or_init(|| {
        RwLock::new(ListenerStatus {
            state: "stopped",
            port: None,
            error: None,
        })
    })
}
pub fn listener_status() -> ListenerStatus {
    listener_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}
fn set_listener(state: &'static str, port: u16, error: Option<String>) {
    *listener_cell().write().unwrap_or_else(|e| e.into_inner()) = ListenerStatus {
        state,
        port: Some(port),
        error,
    };
    crate::notify_store_changed("api");
}

// ---- auth middleware ----

/// Reject requests whose Host header does not name the loopback bind. This
/// closes the DNS-rebinding vector: a web page whose origin re-resolves to
/// 127.0.0.1 would otherwise become same-origin and could read API responses.
async fn enforce_loopback_host(
    allowed: [String; 3],
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let host = req.headers().get(HOST).and_then(|h| h.to_str().ok());
    if host.is_some_and(|value| allowed.iter().any(|candidate| candidate == value)) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::BAD_REQUEST)
    }
}

async fn auth(req: Request, next: Next) -> Result<Response, StatusCode> {
    let secret = read_secret();
    let ok = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
        })
        .map(|t| verify(&secret, t.trim()))
        .unwrap_or(false);
    if ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ---- handlers ----

async fn storage_job<T: serde::Serialize + Send + 'static>(
    job: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> ApiResult {
    let value = tokio::task::spawn_blocking(move || {
        let _activity = crate::operations::activity();
        job()
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(storage_error)?;
    Ok(Json(serde_json::to_value(value).map_err(|e| {
        err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?))
}

async fn list_trash() -> ApiResult {
    storage_job(crate::trash::list).await
}
async fn restore_trash(Path(id): Path<String>) -> ApiResult {
    storage_job(move || Ok(json!({"restored_id": crate::trash::restore(&id)?}))).await
}
async fn purge_trash(Path(id): Path<String>) -> ApiResult {
    storage_job(move || Ok(json!({"purged": crate::trash::purge(&[id])?}))).await
}
async fn empty_trash() -> ApiResult {
    storage_job(|| Ok(json!({"purged": crate::trash::empty()?}))).await
}
async fn list_extensions() -> ApiResult {
    storage_job(crate::extensions::list).await
}
#[derive(Deserialize)]
struct ExtensionImportReq {
    path: String,
}
async fn import_extension(Json(body): Json<ExtensionImportReq>) -> ApiResult {
    storage_job(move || {
        let entry =
            crate::extensions::import(std::path::Path::new(&body.path)).map_err(|error| {
                if error.downcast_ref::<std::io::Error>().is_some()
                    || error.downcast_ref::<crate::errors::Failure>().is_some()
                {
                    error
                } else {
                    error.context(crate::errors::Failure::InvalidInput(
                        "Invalid extension package".into(),
                    ))
                }
            })?;
        crate::notify_store_changed("extensions");
        Ok(entry)
    })
    .await
}
async fn delete_extension(Path(id): Path<String>) -> ApiResult {
    storage_job(move || {
        crate::extensions::delete(&id)?;
        crate::notify_store_changed("extensions");
        Ok(json!({"deleted": id}))
    })
    .await
}

async fn health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "name": "shardx-launcher",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn list_profiles() -> ApiResult {
    let metas = crate::profile::list_all()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let running = crate::process::Tracker::shared().running();
    let by_id: std::collections::HashMap<String, crate::process::RunningProfile> = running
        .into_iter()
        .map(|r| (r.profile_id.clone(), r))
        .collect();
    let out: Vec<Value> = metas
        .into_iter()
        .map(|m| {
            let r = by_id.get(&m.id);
            json!({
                "id": m.id,
                "name": m.name,
                "notes": m.notes,
                "proxy_id": m.proxy_id,
                "last_launched_at": m.last_launched_at,
                "created_at": m.created_at,
                "pinned": m.pinned,
                "folder": m.folder,
                "running": r.is_some(),
                "pid": r.map(|x| x.pid),
                "cdp": r.and_then(|x| x.cdp.clone()),
            })
        })
        .collect();
    Ok(Json(json!(out)))
}

async fn get_profile(Path(id): Path<String>) -> ApiResult {
    let _ = crate::process::Tracker::shared().refresh_cdp(&id).await;
    let stored = crate::profile::load_raw(&id).map_err(storage_error)?;
    let mut val = serde_json::to_value(stored)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(inline) = val
        .pointer_mut("/_meta/inline_proxy")
        .and_then(|value| value.as_object_mut())
    {
        inline.remove("username");
        inline.remove("password");
        inline.remove("credentials_protected");
    }
    val["running"] = json!(crate::is_profile_running(&id));
    val["cdp"] = json!(crate::process::Tracker::shared().cdp(&id));
    val["cdp_error"] = json!(crate::process::Tracker::shared()
        .running()
        .iter()
        .find(|r| r.profile_id == id)
        .and_then(|r| r.cdp_error.clone()));
    Ok(Json(val))
}

// ---- get-new-fingerprint ----

/// Uniquified fingerprint without persisting; creation preserves hardware picks
/// and applies new-identity consistency rules before saving.
async fn new_fingerprint() -> ApiResult {
    new_fingerprint_impl(None).await
}

async fn new_fingerprint_for(Path(platform): Path<String>) -> ApiResult {
    new_fingerprint_impl(Some(platform)).await
}

async fn new_fingerprint_impl(platform: Option<String>) -> ApiResult {
    let fid = least_used_fingerprint_for(platform.as_deref())?;
    let mut cfg = crate::build_fingerprint_config(crate::main_window().as_ref(), &fid)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    cfg.remove("_meta");
    Ok(Json(json!({ "fingerprint": cfg })))
}

// ---- create-profile ----

#[derive(Deserialize)]
struct CreateReq {
    #[serde(default)]
    extensions: Vec<String>,
    name: Option<String>,
    notes: Option<String>,
    proxy_id: Option<String>,
    /// Proxy string: added to store + full-tested, bound by id.
    proxy: Option<String>,
    folder: Option<String>,
    fingerprint: Value,
}

/// Preserve selected hardware and normalize new-identity fields; proxy_id binds,
/// proxy string upserts+tests.
async fn persist_created(folder_override: Option<String>, body: CreateReq) -> ApiResult {
    let mut cfg = body
        .fingerprint
        .as_object()
        .cloned()
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "`fingerprint` must be an object"))?;
    cfg.remove("_meta");
    if let Some(n) = body.name.as_ref() {
        cfg.insert("name".into(), json!(n));
    }
    if let Some(n) = body.notes.as_ref() {
        cfg.insert("notes".into(), json!(n));
    }
    let profile_name = cfg.get("name").and_then(|v| v.as_str()).unwrap_or("");
    crate::display_name::normalize(profile_name)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;

    let folder = folder_override.or(body.folder).unwrap_or_default();
    let mut meta = json!({ "id": "", "folder": folder, "extensions": body.extensions });
    if let Some(pid) = body.proxy_id.as_ref() {
        meta["proxy_id"] = json!(pid);
    } else if let Some(pstr) = body.proxy.as_ref() {
        let entry = crate::proxy::parse_single(pstr).ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                format!("unparseable proxy: {pstr}"),
            )
        })?;
        let stored = crate::proxy::upsert_dedup(entry)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // Best-effort full test (UDP + geo); launch re-probes UDP live anyway.
        let _ = crate::proxy::full_test_background(&stored).await;
        meta["proxy_id"] = json!(stored.id);
        crate::notify_store_changed("proxies");
    }
    crate::ensure_default_noise(&mut cfg);
    cfg.insert("_meta".into(), meta);

    let pm = crate::save_profile_core(crate::main_window().as_ref(), Value::Object(cfg), false)
        .map_err(storage_error)?;
    crate::notify_store_changed("profiles");
    Ok(Json(serde_json::to_value(pm).unwrap_or(Value::Null)))
}

async fn create_profile(Json(body): Json<CreateReq>) -> ApiResult {
    persist_created(None, body).await
}

async fn create_profile_in_folder(
    Path(folder): Path<String>,
    Json(body): Json<CreateReq>,
) -> ApiResult {
    persist_created(Some(folder), body).await
}

// ---- temporary profiles ----

#[derive(Deserialize)]
struct TempReq {
    #[serde(default)]
    extensions: Vec<String>,
    fingerprint_id: Option<String>,
    platform: Option<String>,
    /// Inline proxy (not stored).
    proxy: Option<String>,
    name: Option<String>,
    folder: Option<String>,
}

/// Temporary profile (hidden, auto-deleted on close); pair with /start.
async fn create_temporary(Json(body): Json<TempReq>) -> ApiResult {
    let fid = match body.fingerprint_id {
        Some(f) => f,
        None => least_used_fingerprint_for(body.platform.as_deref())?,
    };
    let mut cfg = crate::build_fingerprint_config(crate::main_window().as_ref(), &fid)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    cfg.remove("_meta");
    if let Some(n) = body.name.as_ref() {
        cfg.insert("name".into(), json!(n));
    }
    let profile_name = cfg.get("name").and_then(|v| v.as_str()).unwrap_or("");
    crate::display_name::normalize(profile_name)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut meta = json!({ "id": "", "folder": body.folder.unwrap_or_default(), "temporary": true, "extensions": body.extensions });
    if let Some(pstr) = body.proxy.as_ref() {
        let entry = crate::proxy::parse_single(pstr).ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                format!("unparseable proxy: {pstr}"),
            )
        })?;
        let protected = crate::proxy::serialize_protected_inline_entry(&entry)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        meta["inline_proxy"] = protected;
    }
    cfg.insert("_meta".into(), meta);

    let pm = crate::save_profile_core(crate::main_window().as_ref(), Value::Object(cfg), false)
        .map_err(storage_error)?;
    Ok(Json(json!({
        "id": pm.id,
        "name": pm.name,
        "fingerprint_id": fid,
        "temporary": true,
        "proxy_inline": body.proxy.is_some(),
    })))
}

async fn delete_profile(Path(id): Path<String>) -> ApiResult {
    crate::profile::ensure_stopped(&id).map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
    storage_job(move || {
        let temporary = crate::profile::load_raw(&id)?.meta.temporary;
        if temporary {
            crate::profile::delete(&id)?;
        } else {
            crate::trash::move_profiles(std::slice::from_ref(&id))?;
        }
        crate::notify_store_changed("profiles");
        Ok(json!({ "deleted": true, "id": id, "recycled": !temporary }))
    })
    .await
}

#[derive(Deserialize)]
struct EditReq {
    extensions: Option<Vec<String>>,
    name: Option<String>,
    notes: Option<String>,
    /// "" unfiles.
    folder: Option<String>,
    /// "" unbinds.
    proxy_id: Option<String>,
    /// Proxy string: stored + tested, then bound.
    proxy: Option<String>,
    /// Replace stored fingerprint verbatim.
    fingerprint: Option<Value>,
}

/// Edit profile; only provided fields change. Returns the updated profile.
async fn edit_profile(Path(id): Path<String>, Json(body): Json<EditReq>) -> ApiResult {
    crate::profile::ensure_stopped(&id).map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
    let prepared_proxy = if body.proxy_id.is_none() {
        if let Some(pstr) = body.proxy.as_ref() {
            let entry = crate::proxy::parse_single(pstr)
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, "Unparseable proxy"))?;
            let saved = crate::proxy::upsert_dedup(entry)
                .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
            let _ = crate::proxy::full_test_background(&saved).await;
            crate::notify_store_changed("proxies");
            Some(saved.id)
        } else {
            None
        }
    } else {
        None
    };
    // Apply a partial edit to the latest state after network I/O, under the
    // same lock as every profile writer and launch reservation.
    let _resource_guard = crate::process::lock_profile_resources()
        .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
    crate::profile::ensure_stopped(&id).map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
    let mut stored = crate::profile::load_raw(&id).map_err(storage_error)?;
    let original_name = stored
        .config
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if let Some(fp) = body.fingerprint {
        let mut cfg = fp
            .as_object()
            .cloned()
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "`fingerprint` must be an object"))?;
        cfg.remove("_meta");
        stored.config = cfg;
    }
    if let Some(n) = body.name.as_ref() {
        stored.config.insert("name".into(), json!(n));
    }
    if let Some(n) = body.notes.as_ref() {
        stored.config.insert("notes".into(), json!(n));
    }
    let edited_name = stored
        .config
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if edited_name != original_name.as_str() {
        let name = crate::display_name::normalize(edited_name)
            .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
        stored.config.insert("name".into(), json!(name));
    }
    if let Some(pid) = body.proxy_id.as_ref() {
        stored.meta.proxy_id = if pid.is_empty() {
            None
        } else {
            Some(pid.clone())
        };
        stored.meta.inline_proxy = None;
    } else if let Some(proxy_id) = prepared_proxy {
        stored.meta.proxy_id = Some(proxy_id);
        stored.meta.inline_proxy = None;
    }

    // Re-check under the same resource lock used by launch reservation. The
    // optional proxy test above awaits network I/O and therefore cannot safely
    // hold a synchronous mutex guard across it.
    if let Some(extensions) = body.extensions {
        crate::extensions::load_paths(&extensions)
            .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
        stored.meta.extensions = extensions;
    }
    crate::profile::save_raw(&mut stored)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // set_folder handles unfile; save_raw keeps the existing folder when empty.
    if let Some(f) = body.folder.as_ref() {
        crate::profile::set_folder(&id, f)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let updated = crate::profile::load_raw(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut updated = serde_json::to_value(updated)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(inline) = updated
        .pointer_mut("/_meta/inline_proxy")
        .and_then(|value| value.as_object_mut())
    {
        inline.remove("username");
        inline.remove("password");
        inline.remove("credentials_protected");
    }
    crate::notify_store_changed("profiles");
    Ok(Json(updated))
}

#[derive(Deserialize)]
struct RenameFolderReq {
    name: String,
}

async fn rename_folder_ep(
    Path(folder): Path<String>,
    Json(body): Json<RenameFolderReq>,
) -> ApiResult {
    let n = crate::profile::rename_folder(&folder, &body.name)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    crate::notify_store_changed("profiles");
    Ok(Json(json!({ "renamed_to": body.name, "profiles": n })))
}

#[derive(Deserialize)]
struct DeleteFolderQuery {
    /// true → delete profiles; false (default) → unfile.
    #[serde(default)]
    delete_profiles: bool,
}

async fn delete_folder_ep(
    Path(folder): Path<String>,
    Query(q): Query<DeleteFolderQuery>,
) -> ApiResult {
    storage_job(move || {
        let n = crate::profile::delete_folder(&folder, q.delete_profiles)?;
        crate::notify_store_changed("profiles");
        Ok(json!({ "deleted_folder": folder, "delete_profiles": q.delete_profiles, "profiles": n }))
    })
    .await
}

#[derive(Deserialize, Default)]
struct StartReq {
    #[serde(default)]
    headless: bool,
}

/// Launch with CDP; body `{ "headless": true }` opt-in.
async fn start_profile(Path(id): Path<String>, body: Option<Json<StartReq>>) -> ApiResult {
    crate::profile::load_raw(&id).map_err(storage_error)?;
    if crate::is_profile_active(&id) {
        return Err(err(
            StatusCode::CONFLICT,
            "this browser profile is already running or starting",
        ));
    }
    let headless = body.map(|Json(b)| b.headless).unwrap_or(false);
    let outcome = crate::launch::launch_profile(&id, true, headless)
        .await
        .map_err(storage_error)?;
    Ok(Json(json!({
        "profile_id": id,
        "pid": outcome.pid,
        "headless": headless,
        "cdp": outcome.cdp,
        "warnings": outcome.warnings,
    })))
}

async fn stop_profile(Path(id): Path<String>) -> ApiResult {
    let stopped = crate::process::Tracker::shared()
        .close(&id)
        .await
        .map_err(storage_error)?;
    Ok(Json(json!({ "profile_id": id, "stopped": stopped })))
}

async fn export_cookies(Path(id): Path<String>) -> ApiResult {
    crate::profile::load_raw(&id).map_err(storage_error)?;
    let _resource_guard = crate::process::lock_profile_resources()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    crate::profile::ensure_stopped(&id).map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
    let cookies = crate::cookies::export(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "cookies": cookies })))
}

#[derive(Deserialize)]
struct ImportCookiesReq {
    cookies: Vec<crate::cookies::Cookie>,
}

async fn import_cookies(Path(id): Path<String>, Json(body): Json<ImportCookiesReq>) -> ApiResult {
    crate::profile::load_raw(&id).map_err(storage_error)?;
    let _resource_guard = crate::process::lock_profile_resources()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Running browser would clobber imports on exit.
    crate::profile::ensure_stopped(&id).map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
    let n = crate::cookies::import(&id, &body.cookies)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "imported": n })))
}

async fn list_running() -> Json<Value> {
    Json(json!(
        crate::process::Tracker::shared().running_with_cdp().await
    ))
}

async fn list_fingerprints() -> ApiResult {
    let all = crate::fingerprints::list_all()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let out: Vec<Value> = all
        .into_iter()
        .map(|e| {
            json!({
                "id": e.id,
                "label": e.label,
                "platform": e.platform,
                "chrome": e.chrome,
                "gpu": e.gpu,
                "builtin": e.builtin,
            })
        })
        .collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
struct AddProxyReq {
    /// "scheme://user:pass@host:port" or "host:port:user:pass"; wins over fields.
    proxy: Option<String>,
    /// socks5 | http | https (default socks5).
    kind: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    password: Option<String>,
    name: Option<String>,
    country: Option<String>,
    notes: Option<String>,
}

/// Add proxy (deduped by endpoint); returns summary.
async fn add_proxy(Json(body): Json<AddProxyReq>) -> ApiResult {
    let mut entry = if let Some(s) = body.proxy.as_ref() {
        crate::proxy::parse_single(s)
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("unparseable proxy: {s}")))?
    } else {
        let host = body.host.clone().ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "`proxy` string or host+port required",
            )
        })?;
        let port = body
            .port
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "`port` required"))?;
        let kind = match body.kind.as_deref() {
            Some("http") => crate::proxy::ProxyKind::Http,
            Some("https") => crate::proxy::ProxyKind::Https,
            _ => crate::proxy::ProxyKind::Socks5,
        };
        crate::proxy::ProxyEntry {
            id: String::new(),
            name: String::new(),
            kind,
            host,
            port,
            username: body.username.clone().unwrap_or_default(),
            password: body.password.clone().unwrap_or_default(),
            credentials_unavailable: false,
            protected_credentials: None,
            country: String::new(),
            notes: String::new(),
        }
    };
    // metadata overrides (applied to parsed entries too).
    if let Some(name) = body.name.as_deref() {
        entry.name = crate::display_name::normalize_optional(name)
            .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?
            .to_string();
    }
    if let Some(c) = body.country {
        entry.country = c;
    }
    if let Some(nt) = body.notes {
        entry.notes = nt;
    }
    let stored = crate::proxy::upsert_dedup(entry)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    crate::notify_store_changed("proxies");
    Ok(Json(json!({
        "id": stored.id,
        "name": stored.name,
        "kind": stored.kind,
        "host": stored.host,
        "port": stored.port,
        "country": stored.country,
    })))
}

async fn delete_proxy(Path(id): Path<String>) -> ApiResult {
    crate::profile::ensure_proxy_not_active(&id)
        .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
    crate::proxy::delete(&id).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    crate::notify_store_changed("proxies");
    Ok(Json(json!({ "deleted": true, "id": id })))
}

async fn list_proxies() -> ApiResult {
    let list =
        crate::proxy::list().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Credentials never exposed over API.
    let out: Vec<Value> = list
        .into_iter()
        .map(|p| {
            let metadata = crate::proxy::metadata(&p);
            json!({
                "id": metadata.id,
                "name": metadata.name,
                "kind": metadata.kind,
                "host": metadata.host,
                "port": metadata.port,
                "credentials_configured": metadata.credentials_configured,
                "credentials_unavailable": metadata.credentials_unavailable,
                "country": metadata.country,
                "notes": metadata.notes,
            })
        })
        .collect();
    Ok(Json(json!(out)))
}

async fn list_folders() -> ApiResult {
    let metas = crate::profile::list_all()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut set = std::collections::BTreeSet::new();
    for m in metas {
        if !m.folder.is_empty() {
            set.insert(m.folder);
        }
    }
    Ok(Json(json!(set.into_iter().collect::<Vec<_>>())))
}

/// Least-used fingerprint id for platform (host OS when None); falls back to all.
fn least_used_fingerprint_for(platform: Option<&str>) -> Result<String, ApiError> {
    crate::recommended_fingerprint_for(platform)
        .map_err(|error| err(StatusCode::INTERNAL_SERVER_ERROR, error))
}

// ---- server ----

pub async fn serve(secret: String, port: u16) {
    set_secret(&secret);
    set_listener("starting", port, None);

    let protected = Router::new()
        .route("/trash", get(list_trash).delete(empty_trash))
        .route("/trash/:id", delete(purge_trash))
        .route("/trash/:id/restore", post(restore_trash))
        .route("/extensions", get(list_extensions).post(import_extension))
        .route("/extensions/:id", delete(delete_extension))
        .route("/profiles", get(list_profiles).post(create_profile))
        .route("/profiles/temporary", post(create_temporary))
        .route(
            "/profiles/:id",
            get(get_profile).patch(edit_profile).delete(delete_profile),
        )
        .route("/profiles/:id/start", post(start_profile))
        .route("/profiles/:id/stop", post(stop_profile))
        .route(
            "/profiles/:id/cookies",
            get(export_cookies).post(import_cookies),
        )
        .route("/folders", get(list_folders))
        .route(
            "/folders/:folder",
            patch(rename_folder_ep).delete(delete_folder_ep),
        )
        .route("/folders/:folder/profiles", post(create_profile_in_folder))
        .route("/fingerprint/new", get(new_fingerprint))
        .route("/fingerprint/new/:platform", get(new_fingerprint_for))
        .route("/fingerprints", get(list_fingerprints))
        .route("/running", get(list_running))
        .route("/proxies", get(list_proxies).post(add_proxy))
        .route("/proxies/:id", delete(delete_proxy))
        .route_layer(middleware::from_fn(auth));

    let loopback_hosts = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    let app = Router::new()
        .route("/health", get(health))
        .merge(protected)
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let allowed = loopback_hosts.clone();
            async move { enforce_loopback_host(allowed, req, next).await }
        }));

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            set_listener("listening", port, None);
            eprintln!("[launcher] automation API listening on http://{addr}");
            if let Err(e) = axum::serve(listener, app).await {
                set_listener("failed", port, Some(e.to_string()));
                eprintln!("[launcher] API server error: {e}");
            } else {
                set_listener("stopped", port, None);
            }
        }
        Err(e) => {
            set_listener("failed", port, Some(e.to_string()));
            eprintln!("[launcher] API bind {addr} failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn profile_and_proxy_display_names_share_the_manual_rules() {
        let _test = crate::store::TestStore::new();
        let error_details = |error: ApiError| (error.0, error.1);
        let create = |name| {
            Json(serde_json::from_value(json!({ "name": name, "fingerprint": {} })).unwrap())
        };
        assert_eq!(
            create_profile(create("A:B")).await.unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        let Json(profile) = create_profile(create("  US-West_01  (v2.1) "))
            .await
            .map_err(error_details)
            .unwrap();
        assert_eq!(profile["name"], "US-West_01  (v2.1)");
        let id = profile["id"].as_str().unwrap();
        let edit = |name| Json(serde_json::from_value(json!({ "name": name })).unwrap());
        assert_eq!(
            edit_profile(Path(id.into()), edit("Bad/Name"))
                .await
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        let Json(edited) = edit_profile(Path(id.into()), edit("  Edited (v3.0) "))
            .await
            .map_err(error_details)
            .unwrap();
        assert_eq!(edited["name"], "Edited (v3.0)");
        assert_eq!(
            crate::profile::load_raw(id).unwrap().config["name"],
            "Edited (v3.0)"
        );
        crate::profile_rename(id.into(), "  Line_Edit-1 (v3.0)  ".into()).unwrap();
        assert_eq!(
            crate::profile::clone_profile(id).unwrap().name,
            "Line_Edit-1 (v3.0)-C"
        );

        let proxy = |host, name| {
            Json(
                serde_json::from_value(json!({ "host": host, "port": 1080, "name": name }))
                    .unwrap(),
            )
        };
        assert_eq!(
            add_proxy(proxy("invalid.example", "A:B"))
                .await
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        let Json(saved) = add_proxy(proxy("proxy.example", "  US-West_01  (v2.1)  "))
            .await
            .map_err(error_details)
            .unwrap();
        assert_eq!(saved["name"], "US-West_01  (v2.1)");
        let Json(automatic) = add_proxy(proxy("[2001:db8::1]", "  "))
            .await
            .map_err(error_details)
            .unwrap();
        assert_eq!(automatic["name"], "[2001:db8::1]:1080");
    }

    #[tokio::test]
    async fn partial_patch_after_proxy_probe_preserves_concurrent_note_edit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _test = crate::store::TestStore::new();
        let mut profile: crate::profile::StoredProfile = serde_json::from_value(
            json!({"_meta":{"id":"concurrent-edit"},"name":"Before","notes":"old"}),
        )
        .unwrap();
        crate::profile::save_raw(&mut profile).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let signal = started.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                signal.notify_one();
                tokio::spawn(async move {
                    let mut buf = [0; 4096];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                });
            }
        });
        let request = serde_json::from_value(
            json!({"name":"Renamed","proxy": format!("http://127.0.0.1:{port}")}),
        )
        .unwrap();
        let update_notes = async {
            started.notified().await;
            edit_profile(
                Path("concurrent-edit".into()),
                Json(serde_json::from_value(json!({"notes":"new"})).unwrap()),
            )
            .await
        };
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(45), async {
            tokio::join!(
                edit_profile(Path("concurrent-edit".into()), Json(request)),
                update_notes
            )
        })
        .await
        .unwrap();
        server.abort();
        assert!(first.is_ok());
        assert!(second.is_ok());
        let saved = crate::profile::load_raw("concurrent-edit").unwrap();
        assert_eq!(saved.config["name"], "Renamed");
        assert_eq!(saved.config["notes"], "new");
    }

    #[tokio::test]
    async fn listener_reports_bind_failure_and_storage_errors_keep_categories() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        serve("test-secret".into(), port).await;
        let status = listener_status();
        assert_eq!(status.state, "failed");
        assert_eq!(status.port, Some(port));
        assert!(status.error.is_some());
        assert_eq!(
            storage_error(crate::errors::conflict("busy").into()).0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            storage_error(crate::errors::Failure::NotFound("missing".into()).into()).0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            storage_error(std::io::Error::from(std::io::ErrorKind::NotFound).into()).0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            storage_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()).0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
