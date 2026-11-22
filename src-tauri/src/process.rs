// Tracker for launched ShardX browser processes; keyed by profile_id.
//
// A browser can outlive the Tauri launcher (for example after a launcher crash
// or update). Durable leases retain enough immutable process identity to safely
// reattach on the next start without trusting a reused PID.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, MutexGuard,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Child;

const LEASE_FORMAT_VERSION: u32 = 1;
/// Record a diagnostic snapshot after repeated failures; unresolved live
/// processes remain protected until a later inspection proves they exited.
const LEASE_DIAGNOSTIC_THRESHOLD: u32 = 3;
const RECOVERY_POLL_INTERVAL: Duration = Duration::from_secs(1);
static LEASE_RECOVERY_INCOMPLETE: AtomicBool = AtomicBool::new(false);

pub fn lease_recovery_is_incomplete() -> bool {
    LEASE_RECOVERY_INCOMPLETE.load(Ordering::Acquire)
}

fn set_lease_recovery_incomplete(value: bool) {
    LEASE_RECOVERY_INCOMPLETE.store(value, Ordering::Release);
}

pub fn ensure_lease_recovery_complete() -> Result<()> {
    if lease_recovery_is_incomplete() {
        anyhow::bail!(crate::errors::conflict("Browser process recovery is incomplete. Close the affected browsers or resolve process access, then retry recovery."));
    }
    Ok(())
}

pub struct Tracker {
    inner: Mutex<HashMap<String, ChildEntry>>,
    launching: Mutex<HashMap<String, Arc<LaunchControl>>>,
    /// Leases that could not be inspected during startup. They must survive
    /// later lifecycle writes so the next launcher start remains fail-closed.
    unverified: Mutex<Vec<ProcessLease>>,
    /// Serializes read/modify/write lease-file operations with tracker changes.
    lease_file: Mutex<()>,
    /// Consecutive incomplete recoveries, mirrored into the lease file so the
    /// count survives restarts.
    lease_incomplete_count: Mutex<u32>,
    finish_lock: Mutex<()>,
    recovery_error: Mutex<Option<String>>,
}

/// Keeps a profile reserved while launch preflight is in progress. Dropping
/// the guard after an error releases the reservation; successful tracking also
/// clears it once the process lease becomes authoritative.
pub struct LaunchReservation<'a> {
    tracker: &'a Tracker,
    profile_id: String,
    control: Arc<LaunchControl>,
}

struct LaunchControl {
    cancelled: tokio::sync::watch::Sender<bool>,
    committed: Mutex<bool>,
}

pub const LAUNCH_CANCELLED: &str = "Launch cancelled";

impl LaunchReservation<'_> {
    /// Dropping the losing future also drops its pending network requests.
    pub async fn wait<T>(&self, future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        let mut cancelled = self.control.cancelled.subscribe();
        if *cancelled.borrow() {
            anyhow::bail!(LAUNCH_CANCELLED);
        }
        tokio::select! {
            biased;
            _ = cancelled.changed() => anyhow::bail!(LAUNCH_CANCELLED),
            result = future => result,
        }
    }

    /// Cancellation and process creation have one linearization point. Once
    /// committed, cancellation reports false and the caller may close the run.
    pub fn commit(&self) -> Result<()> {
        let mut committed = self
            .control
            .committed
            .lock()
            .map_err(|_| anyhow::anyhow!("launch state lock poisoned"))?;
        if *self.control.cancelled.borrow() {
            anyhow::bail!(LAUNCH_CANCELLED);
        }
        *committed = true;
        Ok(())
    }
}

impl Drop for LaunchReservation<'_> {
    fn drop(&mut self) {
        if let Ok(mut launching) = self.tracker.launching.lock() {
            if launching
                .get(&self.profile_id)
                .is_some_and(|control| Arc::ptr_eq(control, &self.control))
            {
                launching.remove(&self.profile_id);
            }
        }
    }
}

struct ChildEntry {
    pid: u32,
    closer: ProcessCloser,
    /// Set once DevToolsActivePort is read; None for UI launches or recovered runs.
    cdp: Option<CdpInfo>,
    cdp_error: Option<String>,
    /// Monotonic launch time used for the current-session uptime display.
    started_at: Instant,
    lease: ProcessLease,
    closing: bool,
}

enum ProcessCloser {
    Child(tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<Result<()>>>),
    Recovered,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ProcessLease {
    profile_id: String,
    pid: u32,
    executable_path: String,
    user_data_dir: String,
    run_id: String,
    temporary: bool,
    /// Windows FILETIME creation timestamp. This distinguishes a reused PID.
    process_started_at_100ns: u64,
    /// Wall-clock launch time for uptime/runtime accounting across recovery.
    launched_at_unix_ms: u64,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct LeaseFile {
    #[serde(default)]
    format_version: u32,
    /// Consecutive recovery attempts that ended with unverifiable leases.
    /// Tracked across restarts for recovery diagnostics.
    #[serde(default)]
    consecutive_incomplete_recoveries: u32,
    #[serde(default)]
    leases: Vec<ProcessLease>,
}

/// CDP endpoint for an API-launched profile.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CdpInfo {
    pub port: u16,
    pub http_url: String,
    /// ws://127.0.0.1:<port>/devtools/browser/<id> for Puppeteer/Playwright.
    pub web_socket_debugger_url: String,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            launching: Mutex::new(HashMap::new()),
            unverified: Mutex::new(Vec::new()),
            lease_file: Mutex::new(()),
            lease_incomplete_count: Mutex::new(0),
            finish_lock: Mutex::new(()),
            recovery_error: Mutex::new(None),
        }
    }

    /// Restore leases before profile mutation or temporary-profile cleanup. A
    /// lease is accepted only if the PID still identifies the same executable
    /// and process creation time; when Windows exposes the command line, its
    /// --user-data-dir must also match the persisted directory.
    pub fn recover(&'static self) -> Result<usize> {
        let _resources = resource_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("profile resource lock poisoned"))?;
        let _finish = self
            .finish_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("process finish lock poisoned"))?;
        let result = self.recover_locked();
        *self
            .recovery_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = result.as_ref().err().map(|e| format!("{e:#}"));
        crate::notify_store_changed("recovery");
        result
    }

    pub fn recovery_status(&self) -> serde_json::Value {
        serde_json::json!({
            "incomplete": lease_recovery_is_incomplete(),
            "attempts": *self.lease_incomplete_count.lock().unwrap_or_else(|e| e.into_inner()),
            "error": *self.recovery_error.lock().unwrap_or_else(|e| e.into_inner()),
        })
    }

    fn recover_locked(&'static self) -> Result<usize> {
        // Remain fail-closed until the complete recovery path proves that no
        // persisted browser lease needs protection.
        set_lease_recovery_incomplete(true);
        let path = crate::store::process_leases_path()?;
        if !path.exists() {
            set_lease_recovery_incomplete(false);
            return Ok(0);
        }

        let persisted: LeaseFile = crate::store::load_json_with_backup(&path)
            .with_context(|| format!("load process leases from {}", path.display()))?;
        if persisted.format_version > LEASE_FORMAT_VERSION {
            anyhow::bail!(
                "process lease format {} is newer than supported format {}",
                persisted.format_version,
                LEASE_FORMAT_VERSION
            );
        }

        let expected_user_data_root = crate::store::user_data_root()?;
        let mut recovered = Vec::new();
        let mut unresolved = Vec::new();
        let mut seen_profiles = HashSet::new();
        let mut inspection_failed = false;
        {
            let mut count = self
                .lease_incomplete_count
                .lock()
                .map_err(|_| anyhow::anyhow!("process lease counter lock poisoned"))?;
            *count = persisted.consecutive_incomplete_recoveries;
        }
        let mut candidates = persisted.leases;
        candidates.sort_by(|left, right| {
            right
                .launched_at_unix_ms
                .cmp(&left.launched_at_unix_ms)
                .then_with(|| left.run_id.cmp(&right.run_id))
        });

        for lease in candidates {
            if !lease_has_required_fields(&lease)
                || !lease_user_data_dir_is_expected(&lease, &expected_user_data_root)
            {
                // An incomplete creation-time snapshot cannot identify a live
                // process, but a confirmed absent PID proves this lease ended.
                if lease.pid != 0 && matches!(inspect_process(lease.pid), Ok(None)) {
                    continue;
                }
                // A partial/corrupt lease cannot prove its browser stopped.
                // Leave every temporary profile intact for this startup.
                inspection_failed = true;
                unresolved.push(lease.clone());
                eprintln!(
                    "[launcher] could not verify malformed process lease for profile {}",
                    lease.profile_id
                );
                continue;
            }
            match inspect_process(lease.pid) {
                Ok(Some(process)) if lease_matches_process(&lease, &process) => {
                    // The most recent successfully verified lease wins. A
                    // malformed or stale duplicate must not hide a still-live
                    // older lease for the same profile.
                    if seen_profiles.insert(lease.profile_id.clone()) {
                        recovered.push(lease);
                    } else {
                        eprintln!(
                            "[launcher] discarded duplicate process lease for profile {}",
                            lease.profile_id
                        );
                    }
                }
                Ok(Some(_)) => eprintln!(
                    "[launcher] discarded stale process lease for profile {}: process identity changed",
                    lease.profile_id
                ),
                Ok(None) => eprintln!(
                    "[launcher] discarded stale process lease for profile {}: PID {} exited",
                    lease.profile_id, lease.pid
                ),
                Err(error) => {
                    inspection_failed = true;
                    unresolved.push(lease.clone());
                    eprintln!(
                        "[launcher] could not verify process lease for profile {}: {error}",
                        lease.profile_id
                    );
                }
            }
        }

        let failures = if inspection_failed {
            self.bump_incomplete_recoveries()
        } else {
            self.reset_incomplete_recoveries();
            0
        };
        let mut new_monitors = Vec::new();
        // Persist and expose every verified live lease before reporting an
        // incomplete inspection. This preserves the mutation/launch lock for
        // browsers whose identity was proven even if another lease needs a
        // later recovery attempt.
        let persisted_result = {
            let _lease_guard = self
                .lease_file
                .lock()
                .map_err(|_| anyhow::anyhow!("process lease lock poisoned"))?;
            let mut entries = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("process tracker lock poisoned"))?;
            let mut unverified = self
                .unverified
                .lock()
                .map_err(|_| anyhow::anyhow!("process unverified lease lock poisoned"))?;
            for lease in &recovered {
                if entries.contains_key(&lease.profile_id) {
                    continue;
                }
                new_monitors.push((lease.profile_id.clone(), lease.run_id.clone()));
                entries.insert(
                    lease.profile_id.clone(),
                    ChildEntry {
                        pid: lease.pid,
                        closer: ProcessCloser::Recovered,
                        cdp: None,
                        cdp_error: None,
                        started_at: instant_from_lease(lease),
                        lease: lease.clone(),
                        closing: false,
                    },
                );
            }
            *unverified = unresolved;
            drop(unverified);
            self.persist_leases_locked(&entries)
        };

        for (profile_id, run_id) in new_monitors {
            self.spawn_recovery_monitor(profile_id, run_id);
        }
        persisted_result.context("persist process recovery state")?;

        if inspection_failed {
            if failures == LEASE_DIAGNOSTIC_THRESHOLD {
                let unverified = self
                    .unverified
                    .lock()
                    .map_err(|_| anyhow::anyhow!("process unverified lease lock poisoned"))?;
                let payload = serde_json::to_vec_pretty(&serde_json::json!({
                    "format_version": LEASE_FORMAT_VERSION, "recorded_at_unix_ms": unix_now_ms(), "leases": &*unverified,
                }))?;
                crate::store::atomic_write(
                    &crate::store::process_leases_path()?
                        .with_file_name("process-leases.unresolved.json"),
                    &payload,
                )?;
            }
            anyhow::bail!("Process recovery attempt {failures} could not verify all browsers. Close the affected browsers or resolve process access, then retry recovery.");
        }

        set_lease_recovery_incomplete(false);
        Ok(recovered.len())
    }

    /// Atomically reserve one profile for launch. This blocks both duplicate
    /// starts and mutations during the potentially slow launch preflight.
    pub fn reserve_launch(&self, profile_id: &str) -> Result<LaunchReservation<'_>> {
        // Serialize launch reservation against proxy edits/deletes. Whichever
        // operation starts first completes its atomic state transition first.
        let _resource_guard = lock_profile_resources()?;
        crate::operations::ensure_profile_available(profile_id)?;
        if self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("process tracker lock poisoned"))?
            .contains_key(profile_id)
        {
            anyhow::bail!(crate::errors::conflict(
                "This browser profile is already running"
            ));
        }

        let mut launching = self
            .launching
            .lock()
            .map_err(|_| anyhow::anyhow!("launch reservation lock poisoned"))?;
        if launching.contains_key(profile_id) {
            anyhow::bail!(crate::errors::conflict(
                "This browser profile is already starting"
            ));
        }
        let control = Arc::new(LaunchControl {
            cancelled: tokio::sync::watch::channel(false).0,
            committed: Mutex::new(false),
        });
        launching.insert(profile_id.to_string(), control.clone());

        Ok(LaunchReservation {
            tracker: self,
            profile_id: profile_id.to_string(),
            control,
        })
    }

    pub fn cancel_launch(&self, profile_id: &str) -> Result<bool> {
        let control = self
            .launching
            .lock()
            .map_err(|_| anyhow::anyhow!("launch reservation lock poisoned"))?
            .get(profile_id)
            .cloned();
        let Some(control) = control else {
            return Ok(false);
        };
        let committed = control
            .committed
            .lock()
            .map_err(|_| anyhow::anyhow!("launch state lock poisoned"))?;
        if *committed {
            return Ok(false);
        }
        control.cancelled.send_replace(true);
        Ok(true)
    }

    /// Take a spawned child, persist its durable lease, and monitor it. The
    /// actual launched executable is recorded because a headed profile can use
    /// a generated taskbar-badged copy instead of the base runtime binary.
    pub fn track(
        &'static self,
        profile_id: String,
        mut child: Child,
        executable: &Path,
        user_data_dir: &Path,
        temporary: bool,
    ) -> u32 {
        let pid = child.id().unwrap_or(0);
        let executable_path = stable_path_string(executable);
        let user_data_dir = stable_path_string(user_data_dir);
        let process_started_at_100ns = match inspect_process(pid) {
            Ok(Some(process)) => process.started_at_100ns,
            Ok(None) => {
                eprintln!(
                    "[launcher] browser profile {profile_id} exited before tracking completed"
                );
                0
            }
            Err(error) => {
                // The live child remains tracked in memory. A zero timestamp
                // deliberately makes this lease fail closed on future recovery
                // instead of risking a reused PID.
                eprintln!(
                    "[launcher] could not read process creation time for {profile_id}: {error}"
                );
                0
            }
        };
        let lease = ProcessLease {
            profile_id: profile_id.clone(),
            pid,
            executable_path,
            user_data_dir,
            run_id: uuid::Uuid::new_v4().to_string(),
            temporary,
            process_started_at_100ns,
            launched_at_unix_ms: unix_now_ms(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let run_id = lease.run_id.clone();
        let close_lease = lease.clone();

        self.insert_entry(ChildEntry {
            pid,
            closer: ProcessCloser::Child(tx),
            cdp: None,
            cdp_error: None,
            started_at: Instant::now(),
            lease,
            closing: false,
        });
        if let Ok(mut launching) = self.launching.lock() {
            launching.remove(&profile_id);
        }

        // Keep monitoring after a failed or slow close. A timed-out request
        // reports the live process honestly and allows another graceful attempt.
        tokio::spawn(async move {
            let mut pending: Option<tokio::sync::oneshot::Sender<Result<()>>> = None;
            let mut deadline = tokio::time::Instant::now();
            let mut accepting = true;
            loop {
                tokio::select! {
                    result = child.wait() => {
                        Self::shared().finish_run(&profile_id, &run_id, true, true);
                        if let Some(reply) = pending.take() { let _ = reply.send(result.map(|_| ()).map_err(Into::into)); }
                        break;
                    }
                    request = rx.recv(), if accepting && pending.is_none() => {
                        if let Some(reply) = request {
                            match Self::shared().request_close(&close_lease).await {
                                Ok(()) => { pending = Some(reply); deadline = tokio::time::Instant::now() + Duration::from_secs(20); }
                                Err(error) => { let _ = reply.send(Err(error)); }
                            }
                        } else { accepting = false; }
                    }
                    _ = tokio::time::sleep_until(deadline), if pending.is_some() => {
                        if let Some(reply) = pending.take() { let _ = reply.send(Err(anyhow::anyhow!("Browser is still running after the close request. Check its dialogs and try stopping again."))); }
                    }
                }
            }
        });

        pid
    }

    /// Attach CDP to a tracked profile; no-op if the profile already exited.
    pub fn set_cdp(&self, profile_id: &str, pid: u32, cdp: CdpInfo) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(e) = g.get_mut(profile_id) {
                if e.pid == pid {
                    e.cdp = Some(cdp);
                    e.cdp_error = None;
                }
            }
        }
    }

    /// CDP endpoint when the profile was launched with remote debugging.
    pub fn cdp(&self, profile_id: &str) -> Option<CdpInfo> {
        self.inner.lock().ok()?.get(profile_id)?.cdp.clone()
    }

    pub fn running(&self) -> Vec<RunningProfile> {
        let g = self.inner.lock().unwrap();
        g.iter()
            .map(|(id, e)| RunningProfile {
                profile_id: id.clone(),
                pid: e.pid,
                cdp: e.cdp.clone(),
                cdp_error: e.cdp_error.clone(),
                uptime_ms: e.started_at.elapsed().as_millis() as u64,
            })
            .collect()
    }

    pub fn launcher_state(&self) -> Result<LauncherProcessState> {
        let entries = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("process tracker lock poisoned"))?;
        let launching = self
            .launching
            .lock()
            .map_err(|_| anyhow::anyhow!("launch reservation lock poisoned"))?;
        Ok(LauncherProcessState {
            running: entries.keys().cloned().collect(),
            starting: launching
                .keys()
                .filter(|id| !entries.contains_key(*id))
                .cloned()
                .collect(),
            stopping: entries
                .iter()
                .filter(|(_, entry)| entry.closing)
                .map(|(id, _)| id.clone())
                .collect(),
        })
    }

    pub fn is_running_pid(&self, pid: u32) -> bool {
        self.inner
            .lock()
            .map(|entries| entries.values().any(|entry| entry.pid == pid))
            .unwrap_or(false)
    }

    pub fn is_running_profile(&self, profile_id: &str) -> bool {
        self.inner
            .lock()
            .map(|entries| entries.contains_key(profile_id))
            .unwrap_or(false)
    }

    /// Running and launch-preflight profiles are both locked against mutation.
    pub fn is_profile_active(&self, profile_id: &str) -> bool {
        if self.is_running_profile(profile_id) {
            return true;
        }
        self.launching
            .lock()
            .map(|profiles| profiles.contains_key(profile_id))
            .unwrap_or(false)
    }

    pub fn active_profile_ids(&self) -> Vec<String> {
        let mut active: HashSet<String> = self
            .inner
            .lock()
            .map(|entries| entries.keys().cloned().collect())
            .unwrap_or_default();
        if let Ok(launching) = self.launching.lock() {
            active.extend(launching.keys().cloned());
        }
        active.into_iter().collect()
    }

    pub async fn refresh_cdp(&self, profile_id: &str) -> Result<()> {
        let lease = {
            let entries = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("process tracker lock poisoned"))?;
            let Some(entry) = entries.get(profile_id) else {
                return Ok(());
            };
            if entry.cdp.is_some() {
                return Ok(());
            }
            entry.lease.clone()
        };
        let process = inspect_process(lease.pid)
            .map_err(anyhow::Error::msg)?
            .context("Browser exited during debugging recovery")?;
        anyhow::ensure!(
            lease_matches_process(&lease, &process),
            "Browser identity changed during debugging recovery"
        );
        let enabled = process.command_line.as_deref().is_some_and(|line| {
            windows_command_line_arguments(line).iter().any(|arg| {
                arg == "--remote-debugging-port" || arg.starts_with("--remote-debugging-port=")
            })
        });
        if !enabled {
            return Ok(());
        }
        let endpoint = crate::browser_cdp::discover(Path::new(&lease.user_data_dir))
            .await
            .and_then(|endpoint| endpoint.context("Browser debugging endpoint is not ready"));
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("process tracker lock poisoned"))?;
        if let Some(entry) = entries
            .get_mut(profile_id)
            .filter(|entry| entry.lease.run_id == lease.run_id)
        {
            match endpoint {
                Ok(endpoint) => {
                    entry.cdp = Some(endpoint);
                    entry.cdp_error = None;
                }
                Err(error) => {
                    entry.cdp_error = Some(format!("{error:#}"));
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub async fn running_with_cdp(&self) -> Vec<RunningProfile> {
        use futures_util::StreamExt;
        let ids: Vec<_> = self
            .running()
            .into_iter()
            .map(|entry| entry.profile_id)
            .collect();
        futures_util::stream::iter(ids)
            .map(|id| async move {
                let _ = self.refresh_cdp(&id).await;
            })
            .buffer_unordered(8)
            .collect::<Vec<_>>()
            .await;
        self.running()
    }

    async fn request_close(&self, lease: &ProcessLease) -> Result<()> {
        self.refresh_cdp(&lease.profile_id).await?;
        if let Some(endpoint) = self.cdp(&lease.profile_id) {
            crate::browser_cdp::close(&endpoint).await
        } else {
            request_graceful_browser_close(lease.pid)
        }
    }

    pub async fn close(&'static self, profile_id: &str) -> Result<bool> {
        enum CloseAction {
            Child(tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<Result<()>>>),
            Recovered(ProcessLease),
        }

        let action = {
            let mut entries = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("process tracker lock poisoned"))?;
            let Some(entry) = entries.get_mut(profile_id) else {
                return Ok(false);
            };
            if entry.closing {
                anyhow::bail!(crate::errors::conflict(
                    "browser profile is already closing"
                ));
            }
            entry.closing = true;
            match &entry.closer {
                ProcessCloser::Child(closer) => CloseAction::Child(closer.clone()),
                ProcessCloser::Recovered => CloseAction::Recovered(entry.lease.clone()),
            }
        };

        match action {
            CloseAction::Child(closer) => {
                let (reply, completion) = tokio::sync::oneshot::channel();
                if closer.send(reply).await.is_err() {
                    if self.is_running_profile(profile_id) {
                        self.clear_closing(profile_id);
                        anyhow::bail!("browser close monitor stopped unexpectedly");
                    }
                    return Ok(false);
                }
                match completion.await {
                    Ok(Ok(())) => Ok(true),
                    Ok(Err(error)) => {
                        self.clear_closing(profile_id);
                        Err(error)
                    }
                    Err(_) if !self.is_running_profile(profile_id) => {
                        // The process can exit naturally at the same instant the
                        // close request is queued. That is already a clean result.
                        Ok(true)
                    }
                    Err(_) => {
                        self.clear_closing(profile_id);
                        anyhow::bail!("browser close monitor stopped unexpectedly")
                    }
                }
            }
            CloseAction::Recovered(lease) => match self.close_recovered(profile_id, lease).await {
                Ok(stopped) => Ok(stopped),
                Err(error) => {
                    self.clear_closing(profile_id);
                    Err(error)
                }
            },
        }
    }

    pub fn shared() -> &'static Tracker {
        static INSTANCE: std::sync::OnceLock<Tracker> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(Tracker::new)
    }

    fn clear_closing(&self, profile_id: &str) {
        if let Ok(mut entries) = self.inner.lock() {
            if let Some(entry) = entries.get_mut(profile_id) {
                entry.closing = false;
            }
        }
    }

    fn insert_entry(&self, entry: ChildEntry) {
        let lease_profile_id = entry.lease.profile_id.clone();
        let _lease_guard = match self.lease_file.lock() {
            Ok(guard) => guard,
            Err(_) => {
                eprintln!(
                    "[launcher] process lease lock poisoned while tracking {lease_profile_id}"
                );
                return;
            }
        };
        let Ok(mut entries) = self.inner.lock() else {
            eprintln!("[launcher] process tracker lock poisoned while tracking {lease_profile_id}");
            return;
        };
        entries.insert(lease_profile_id.clone(), entry);
        if let Err(error) = self.persist_leases_locked(&entries) {
            // The entry stays authoritative for this launcher process. The
            // persisted lease is retried by later lifecycle transitions.
            eprintln!("[launcher] persist process lease for {lease_profile_id} failed: {error:#}");
        }
    }

    fn persist_leases_locked(&self, entries: &HashMap<String, ChildEntry>) -> Result<()> {
        let consecutive_incomplete_recoveries = *self
            .lease_incomplete_count
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut leases: Vec<ProcessLease> =
            entries.values().map(|entry| entry.lease.clone()).collect();
        let unverified = self
            .unverified
            .lock()
            .map_err(|_| anyhow::anyhow!("process unverified lease lock poisoned"))?;
        leases.extend(unverified.iter().cloned());
        leases.sort_by(|left, right| left.profile_id.cmp(&right.profile_id));
        let serialized = serde_json::to_vec_pretty(&LeaseFile {
            format_version: LEASE_FORMAT_VERSION,
            consecutive_incomplete_recoveries,
            leases,
        })?;
        crate::store::atomic_write(&crate::store::process_leases_path()?, &serialized)
    }

    fn bump_incomplete_recoveries(&self) -> u32 {
        let mut count = self
            .lease_incomplete_count
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *count = count.saturating_add(1);
        *count
    }

    fn reset_incomplete_recoveries(&self) {
        let mut count = self
            .lease_incomplete_count
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *count = 0;
    }

    /// Remove exactly the run that exited. Comparing run IDs ensures a late
    /// monitor from an old process can never erase a newer launch's lease.
    fn remove_run(&self, profile_id: &str, run_id: &str) -> Option<ChildEntry> {
        let _lease_guard = self.lease_file.lock().ok()?;
        let mut entries = self.inner.lock().ok()?;
        if !matches!(
            entries.get(profile_id),
            Some(entry) if entry.lease.run_id == run_id
        ) {
            return None;
        }
        let entry = entries.remove(profile_id)?;
        if let Err(error) = self.persist_leases_locked(&entries) {
            eprintln!("[launcher] remove process lease for {profile_id} failed: {error:#}");
        }
        Some(entry)
    }

    fn finish_run(
        &self,
        profile_id: &str,
        run_id: &str,
        account_runtime: bool,
        delete_temporary: bool,
    ) {
        let _finish_guard = self.finish_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Keep a persistent profile active while its final runtime update is
        // written. This preserves the existing guarantee that a newly opened
        // editor cannot race a stale post-exit profile snapshot.
        let runtime_entry = self.inner.lock().ok().and_then(|entries| {
            entries.get(profile_id).and_then(|entry| {
                (entry.lease.run_id == run_id && !entry.lease.temporary).then_some(entry.started_at)
            })
        });
        if account_runtime {
            if let Some(started_at) = runtime_entry {
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                if let Err(error) = crate::profile::add_runtime(profile_id, elapsed_ms) {
                    eprintln!("[launcher] add_runtime({profile_id}) failed: {error}");
                }
            }
        }

        let Some(entry) = self.remove_run(profile_id, run_id) else {
            return;
        };
        drop(_finish_guard);
        crate::notify_store_changed("profiles");
        if entry.lease.temporary && delete_temporary {
            match crate::profile::delete(profile_id) {
                Ok(()) => eprintln!("[launcher] temporary profile {profile_id} deleted on close"),
                Err(error) => {
                    eprintln!("[launcher] temporary profile {profile_id} cleanup failed: {error}")
                }
            }
        }
    }

    fn spawn_recovery_monitor(&'static self, profile_id: String, run_id: String) {
        tokio::spawn(async move {
            if let Err(error) = self.refresh_cdp(&profile_id).await {
                eprintln!("[launcher] CDP recovery for {profile_id}: {error:#}");
            }
            loop {
                let lease = {
                    let Ok(entries) = self.inner.lock() else {
                        return;
                    };
                    let Some(entry) = entries.get(&profile_id) else {
                        return;
                    };
                    if entry.lease.run_id != run_id {
                        return;
                    }
                    entry.lease.clone()
                };

                match inspect_process(lease.pid) {
                    Ok(Some(process)) if lease_matches_process(&lease, &process) => {
                        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
                    }
                    Ok(None) => {
                        self.finish_run(&profile_id, &run_id, true, true);
                        return;
                    }
                    Ok(Some(_)) => {
                        // Do not delete a temporary profile if identity changed:
                        // a PID-reuse mismatch must never trigger data cleanup.
                        eprintln!(
                            "[launcher] recovered process identity changed for {profile_id}; releasing lease without cleanup"
                        );
                        self.finish_run(&profile_id, &run_id, false, false);
                        return;
                    }
                    Err(error) => {
                        // Keep the recovered lease active on transient access
                        // failures. Releasing it would permit a duplicate
                        // launch or profile mutation while the browser may
                        // still own the user-data directory.
                        eprintln!(
                            "[launcher] cannot verify recovered process for {profile_id}: {error}; retaining lease for retry"
                        );
                        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
                    }
                }
            }
        });
    }

    async fn close_recovered(&self, profile_id: &str, lease: ProcessLease) -> Result<bool> {
        match inspect_process(lease.pid) {
            Ok(Some(process)) if lease_matches_process(&lease, &process) => {}
            Ok(None) => {
                self.finish_run(profile_id, &lease.run_id, true, true);
                return Ok(false);
            }
            Ok(Some(_)) => {
                self.finish_run(profile_id, &lease.run_id, false, false);
                anyhow::bail!(
                    "browser process identity changed before stop; no process was closed"
                );
            }
            Err(error) => {
                anyhow::bail!("cannot safely verify browser process before stop: {error}");
            }
        }

        self.request_close(&lease).await?;
        tokio::time::timeout(Duration::from_secs(20), async { loop {
            match inspect_process(lease.pid) {
                Ok(None) => {
                    self.finish_run(profile_id, &lease.run_id, true, true);
                    return Ok(true);
                }
                Ok(Some(process)) if lease_matches_process(&lease, &process) => {
                    tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
                }
                Ok(Some(_)) => {
                    self.finish_run(profile_id, &lease.run_id, false, false);
                    anyhow::bail!("browser PID changed identity while waiting for stop");
                }
                Err(error) => {
                    anyhow::bail!("cannot verify browser process while waiting for stop: {error}")
                }
            }
        } }).await.context("Browser is still running after the close request. Check its dialogs and try stopping again.")?
    }
}

fn request_graceful_browser_close(pid: u32) -> Result<()> {
    use std::os::windows::process::CommandExt;

    // `taskkill` without `/F` posts WM_CLOSE to the browser's windows. Never
    // add `/F` here: forced termination can discard freshly rotated sessions.
    let output = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .creation_flags(0x08000000)
        .output()
        .context("run taskkill for graceful browser close")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "graceful close request failed for browser pid {pid}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    Ok(())
}

pub fn lock_profile_resources() -> Result<MutexGuard<'static, ()>> {
    let guard = resource_lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("profile resource lock poisoned"))?;
    ensure_lease_recovery_complete()?;
    Ok(guard)
}

fn resource_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug)]
struct LiveProcess {
    executable_path: String,
    started_at_100ns: u64,
    /// Best effort: access may be denied for protected processes. Recovery still
    /// requires the creation timestamp and executable path in that case.
    command_line: Option<String>,
}

fn inspect_process(pid: u32) -> std::result::Result<Option<LiveProcess>, String> {
    windows_process::inspect(pid)
}

fn lease_has_required_fields(lease: &ProcessLease) -> bool {
    !lease.profile_id.is_empty()
        && lease.pid != 0
        && !lease.executable_path.trim().is_empty()
        && !lease.user_data_dir.trim().is_empty()
        && !lease.run_id.trim().is_empty()
        && lease.process_started_at_100ns != 0
}

fn lease_user_data_dir_is_expected(lease: &ProcessLease, user_data_root: &Path) -> bool {
    let expected = stable_path_string(&user_data_root.join(&lease.profile_id));
    same_windows_path(&lease.user_data_dir, &expected)
}

fn lease_matches_process(lease: &ProcessLease, process: &LiveProcess) -> bool {
    lease.process_started_at_100ns != 0
        && process.started_at_100ns == lease.process_started_at_100ns
        && same_windows_path(&lease.executable_path, &process.executable_path)
        && process
            .command_line
            .as_deref()
            .map(|command_line| command_line_uses_user_data_dir(command_line, &lease.user_data_dir))
            .unwrap_or(true)
}

fn stable_path_string(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn instant_from_lease(lease: &ProcessLease) -> Instant {
    let elapsed_ms = unix_now_ms().saturating_sub(lease.launched_at_unix_ms);
    Instant::now()
        .checked_sub(Duration::from_millis(elapsed_ms))
        .unwrap_or_else(Instant::now)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Compare Windows paths case-insensitively and tolerate normal versus extended
/// path prefixes. These records come from Windows APIs and std::process, which
/// can use different slash/prefix spellings for the same executable.
fn same_windows_path(left: &str, right: &str) -> bool {
    normalize_windows_path(left) == normalize_windows_path(right)
}

fn normalize_windows_path(path: &str) -> String {
    let mut normalized = path.trim().trim_matches('"').replace('/', "\\");
    for prefix in ["\\\\?\\", "\\??\\"] {
        if normalized.len() >= prefix.len()
            && normalized[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            normalized = normalized[prefix.len()..].to_string();
            break;
        }
    }
    while normalized.ends_with('\\') && !is_windows_root(&normalized) {
        normalized.pop();
    }
    normalized.to_ascii_lowercase()
}

fn is_windows_root(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() == 3 && bytes[1] == b':' && bytes[2] == b'\\'
}

/// Check the Chromium command line without substring false positives. Windows'
/// parser is kept local and pure so quoted `--user-data-dir=...` paths remain
/// testable independently of live process inspection.
fn command_line_uses_user_data_dir(command_line: &str, expected_user_data_dir: &str) -> bool {
    let args = windows_command_line_arguments(command_line);
    let matches_directory = |value: &str| {
        let path = Path::new(value);
        same_windows_path(value, expected_user_data_dir)
            // The lease stores a canonical path, but CreateProcess receives
            // the launcher path, which can include a substituted drive.
            || (path.is_absolute()
                && std::fs::canonicalize(path).is_ok_and(|resolved| {
                    same_windows_path(&resolved.to_string_lossy(), expected_user_data_dir)
                }))
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some((name, value)) = arg.split_once('=') {
            if name.eq_ignore_ascii_case("--user-data-dir") {
                return matches_directory(value);
            }
        }
        if arg.eq_ignore_ascii_case("--user-data-dir") {
            return args
                .get(index + 1)
                .is_some_and(|value| matches_directory(value));
        }
        index += 1;
    }
    false
}

/// Minimal CommandLineToArgvW-compatible tokenizer for the flag formats we
/// launch. Backslash-before-quote handling is necessary for paths ending in a
/// separator inside a quoted argument.
fn windows_command_line_arguments(command_line: &str) -> Vec<String> {
    let chars: Vec<char> = command_line.chars().collect();
    let mut args = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        if index == chars.len() {
            break;
        }
        let mut argument = String::new();
        let mut quoted = false;
        while index < chars.len() {
            let character = chars[index];
            if !quoted && character.is_whitespace() {
                break;
            }
            if character == '\\' {
                let start = index;
                while index < chars.len() && chars[index] == '\\' {
                    index += 1;
                }
                let slash_count = index - start;
                if index < chars.len() && chars[index] == '"' {
                    for _ in 0..slash_count / 2 {
                        argument.push('\\');
                    }
                    if slash_count % 2 == 0 {
                        quoted = !quoted;
                    } else {
                        argument.push('"');
                    }
                    index += 1;
                } else {
                    for _ in 0..slash_count {
                        argument.push('\\');
                    }
                }
                continue;
            }
            if character == '"' {
                quoted = !quoted;
                index += 1;
                continue;
            }
            argument.push(character);
            index += 1;
        }
        args.push(argument);
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
    }
    args
}

mod windows_process {
    use super::LiveProcess;
    use std::ffi::{c_void, OsString};
    use std::os::windows::ffi::OsStringExt;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_VM_READ: u32 = 0x0010;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const ERROR_NOT_FOUND: i32 = 1168;
    const PROCESS_BASIC_INFORMATION_CLASS: u32 = 0;
    const COMMAND_LINE_OFFSET_IN_PROCESS_PARAMETERS_X64: usize = 0x70;
    const PROCESS_PARAMETERS_POINTER_OFFSET_IN_PEB_X64: usize = 0x20;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FileTime {
        low_date_time: u32,
        high_date_time: u32,
    }

    #[repr(C)]
    struct ProcessBasicInformation {
        reserved1: *mut c_void,
        peb_base_address: *mut c_void,
        reserved2: [*mut c_void; 2],
        unique_process_id: usize,
        reserved3: *mut c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn QueryFullProcessImageNameW(
            process: isize,
            flags: u32,
            executable_name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn GetProcessTimes(
            process: isize,
            creation_time: *mut FileTime,
            exit_time: *mut FileTime,
            kernel_time: *mut FileTime,
            user_time: *mut FileTime,
        ) -> i32;
        fn ReadProcessMemory(
            process: isize,
            base_address: *const c_void,
            buffer: *mut c_void,
            size: usize,
            number_of_bytes_read: *mut usize,
        ) -> i32;
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationProcess(
            process: isize,
            process_information_class: u32,
            process_information: *mut c_void,
            process_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    pub(super) fn inspect(pid: u32) -> std::result::Result<Option<LiveProcess>, String> {
        if pid == 0 {
            return Ok(None);
        }
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process == 0 {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(ERROR_INVALID_PARAMETER) | Some(ERROR_NOT_FOUND)
            ) {
                return Ok(None);
            }
            return Err(format!("open PID {pid}: {error}"));
        }
        let result = (|| unsafe {
            let Some(started_at_100ns) = live_process_creation_time(process)? else {
                return Ok(None);
            };
            let executable_path = match full_image_name(process) {
                Ok(path) => path,
                Err(error) => {
                    if live_process_creation_time(process)?.is_none() {
                        return Ok(None);
                    }
                    return Err(error);
                }
            };
            let command_line = read_command_line(pid);
            Ok(Some(LiveProcess {
                executable_path,
                started_at_100ns,
                command_line,
            }))
        })();
        unsafe {
            CloseHandle(process);
        }
        result
    }

    unsafe fn full_image_name(process: isize) -> std::result::Result<String, String> {
        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        if QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) == 0 {
            return Err(format!(
                "query executable path: {}",
                std::io::Error::last_os_error()
            ));
        }
        buffer.truncate(length as usize);
        Ok(OsString::from_wide(&buffer).to_string_lossy().into_owned())
    }

    unsafe fn live_process_creation_time(
        process: isize,
    ) -> std::result::Result<Option<u64>, String> {
        let mut creation = FileTime {
            low_date_time: 0,
            high_date_time: 0,
        };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        if GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            return Err(format!(
                "query process creation time: {}",
                std::io::Error::last_os_error()
            ));
        }
        if exit.high_date_time != 0 || exit.low_date_time != 0 {
            return Ok(None);
        }
        Ok(Some(
            (u64::from(creation.high_date_time) << 32) | u64::from(creation.low_date_time),
        ))
    }

    /// Direct PEB inspection avoids a WMI/PowerShell subprocess during startup.
    /// Windows does not guarantee command-line visibility across all protected
    /// processes, so failures intentionally return None as a best-effort check.
    unsafe fn read_command_line(pid: u32) -> Option<String> {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
        if process == 0 {
            return None;
        }
        let result = (|| {
            let mut basic: ProcessBasicInformation = std::mem::zeroed();
            if NtQueryInformationProcess(
                process,
                PROCESS_BASIC_INFORMATION_CLASS,
                (&mut basic as *mut ProcessBasicInformation).cast(),
                std::mem::size_of::<ProcessBasicInformation>() as u32,
                std::ptr::null_mut(),
            ) < 0
            {
                return None;
            }
            let process_parameters_address = read_remote_usize(
                process,
                (basic.peb_base_address as usize + PROCESS_PARAMETERS_POINTER_OFFSET_IN_PEB_X64)
                    as *const c_void,
            )?;
            if process_parameters_address == 0 {
                return None;
            }
            let mut parameters = [0u8; COMMAND_LINE_OFFSET_IN_PROCESS_PARAMETERS_X64 + 16];
            read_remote_bytes(
                process,
                process_parameters_address as *const c_void,
                &mut parameters,
            )?;
            let offset = COMMAND_LINE_OFFSET_IN_PROCESS_PARAMETERS_X64;
            let length = u16::from_le_bytes([parameters[offset], parameters[offset + 1]]) as usize;
            if length == 0 || !length.is_multiple_of(2) {
                return None;
            }
            let pointer_offset = offset + 8;
            let buffer_address = u64::from_le_bytes(
                parameters[pointer_offset..pointer_offset + 8]
                    .try_into()
                    .ok()?,
            ) as usize;
            if buffer_address == 0 || length > 65_534 {
                return None;
            }
            let mut raw = vec![0u8; length];
            read_remote_bytes(process, buffer_address as *const c_void, &mut raw)?;
            let words: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|word| u16::from_le_bytes(*word))
                .collect();
            Some(OsString::from_wide(&words).to_string_lossy().into_owned())
        })();
        CloseHandle(process);
        result
    }

    unsafe fn read_remote_usize(process: isize, address: *const c_void) -> Option<usize> {
        let mut value = 0usize;
        read_remote_bytes(
            process,
            address,
            std::slice::from_raw_parts_mut(
                (&mut value as *mut usize).cast::<u8>(),
                std::mem::size_of::<usize>(),
            ),
        )?;
        Some(value)
    }

    unsafe fn read_remote_bytes(
        process: isize,
        address: *const c_void,
        output: &mut [u8],
    ) -> Option<()> {
        let mut bytes_read = 0usize;
        if ReadProcessMemory(
            process,
            address,
            output.as_mut_ptr().cast(),
            output.len(),
            &mut bytes_read,
        ) == 0
            || bytes_read != output.len()
        {
            return None;
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        command_line_uses_user_data_dir, same_windows_path, windows_command_line_arguments,
        LeaseFile, ProcessLease, LEASE_FORMAT_VERSION,
    };

    #[test]
    fn failed_recovery_count_survives_restarts_without_discarding_unverified_leases() {
        let _test = crate::store::TestStore::new();
        let path = crate::store::process_leases_path().unwrap();
        let lease = ProcessLease {
            profile_id: "unverified".into(),
            pid: 0,
            executable_path: "unknown".into(),
            user_data_dir: "unknown".into(),
            run_id: "unknown".into(),
            temporary: false,
            process_started_at_100ns: 0,
            launched_at_unix_ms: 0,
        };
        crate::store::atomic_write(
            &path,
            &serde_json::to_vec(&LeaseFile {
                format_version: LEASE_FORMAT_VERSION,
                consecutive_incomplete_recoveries: 0,
                leases: vec![lease],
            })
            .unwrap(),
        )
        .unwrap();
        for expected in 1..=4 {
            let tracker = Box::leak(Box::new(super::Tracker::new()));
            assert!(tracker.recover().is_err());
            let saved: LeaseFile = crate::store::load_json_with_backup(&path).unwrap();
            assert_eq!(saved.consecutive_incomplete_recoveries, expected);
            assert_eq!(saved.leases.len(), 1);
            assert!(super::lease_recovery_is_incomplete());
        }
        assert!(path
            .with_file_name("process-leases.unresolved.json")
            .exists());
        let mut stopped: LeaseFile = crate::store::load_json_with_backup(&path).unwrap();
        assert!(super::inspect_process(u32::MAX).unwrap().is_none());
        stopped.leases[0].pid = u32::MAX;
        crate::store::atomic_write(&path, &serde_json::to_vec(&stopped).unwrap()).unwrap();
        Box::leak(Box::new(super::Tracker::new()))
            .recover()
            .unwrap();
        assert!(!super::lease_recovery_is_incomplete());
    }

    #[tokio::test]
    #[ignore = "requires SHARDX_TEST_BROWSER pointing to the actual Chromium executable"]
    async fn real_chromium_cdp_survives_recovery_and_closes_headless() {
        use super::*;
        let _test = crate::store::TestStore::new();
        let executable = std::path::PathBuf::from(
            std::env::var("SHARDX_TEST_BROWSER").expect("SHARDX_TEST_BROWSER"),
        );
        for recovered in [false, true] {
            let id = if recovered {
                "recovered-browser"
            } else {
                "tracked-browser"
            };
            let mut profile: crate::profile::StoredProfile =
                serde_json::from_value(serde_json::json!({"_meta":{"id":id},"name":"BrowserTest"}))
                    .unwrap();
            crate::profile::save_raw(&mut profile).unwrap();
            let directory = crate::profile::user_data_dir(id).unwrap();
            let child = tokio::process::Command::new(&executable)
                .args([
                    "--headless=new",
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--remote-debugging-port=0",
                    "--disable-background-networking",
                ])
                .arg(format!("--user-data-dir={}", directory.display()))
                .arg("about:blank")
                .creation_flags(0x08000000)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let pid = child.id().unwrap();
            let endpoint = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    if let Ok(Some(endpoint)) = crate::browser_cdp::discover(&directory).await {
                        break endpoint;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .unwrap();
            let mut child_after_recovery = None;
            if recovered {
                let process = inspect_process(pid).unwrap().unwrap();
                let lease = ProcessLease {
                    profile_id: id.into(),
                    pid,
                    executable_path: stable_path_string(&executable),
                    user_data_dir: stable_path_string(&directory),
                    run_id: uuid::Uuid::new_v4().to_string(),
                    temporary: false,
                    process_started_at_100ns: process.started_at_100ns,
                    launched_at_unix_ms: unix_now_ms(),
                };
                crate::store::atomic_write(
                    &crate::store::process_leases_path().unwrap(),
                    &serde_json::to_vec(&LeaseFile {
                        format_version: LEASE_FORMAT_VERSION,
                        consecutive_incomplete_recoveries: 0,
                        leases: vec![lease],
                    })
                    .unwrap(),
                )
                .unwrap();
                assert_eq!(Tracker::shared().recover().unwrap(), 1);
                child_after_recovery = Some(child);
            } else {
                Tracker::shared().track(id.into(), child, &executable, &directory, false);
            }
            let state = Tracker::shared().launcher_state().unwrap();
            assert_eq!(state.running, [id]);
            assert!(state.starting.is_empty() && state.stopping.is_empty());
            let running = Tracker::shared().running_with_cdp().await;
            assert_eq!(
                running
                    .iter()
                    .find(|r| r.profile_id == id)
                    .unwrap()
                    .cdp
                    .as_ref()
                    .unwrap()
                    .port,
                endpoint.port
            );
            assert!(Tracker::shared().close(id).await.unwrap());
            if let Some(child) = child_after_recovery.as_mut() {
                child.wait().await.unwrap();
            }
            assert!(inspect_process(pid).unwrap().is_none());
            assert!(!Tracker::shared().is_running_profile(id));
            let state = Tracker::shared().launcher_state().unwrap();
            assert!(state.running.is_empty() && state.starting.is_empty() && state.stopping.is_empty());
            let saved: LeaseFile =
                crate::store::load_json_with_backup(&crate::store::process_leases_path().unwrap())
                    .unwrap();
            assert!(saved.leases.is_empty());
            assert!(directory.join("Local State").exists());
        }
    }

    #[tokio::test]
    async fn launch_reservation_blocks_duplicate_starts_until_released() {
        let _test = crate::store::TestStore::new();
        let tracker = super::Tracker::new();
        let reservation = tracker.reserve_launch("profile-1").unwrap();

        assert!(tracker.is_profile_active("profile-1"));
        assert!(tracker.reserve_launch("profile-1").is_err());
        let state = tracker.launcher_state().unwrap();
        assert_eq!(state.starting, ["profile-1"]);
        assert!(state.running.is_empty() && state.stopping.is_empty());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (connected, connection) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            connected.send(()).unwrap();
            // No HTTP reply: cancellation must drop the socket, not wait for a timeout.
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).await.unwrap();
        });
        let request = reservation.wait(async {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!("http://{address}"))
                .send()
                .await?;
            Ok(())
        });
        let cancel = async {
            connection.await.unwrap();
            assert!(tracker.cancel_launch("profile-1").unwrap());
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(request, cancel)
        })
        .await
        .expect("cancellation must interrupt network wait");
        assert_eq!(result.unwrap_err().to_string(), super::LAUNCH_CANCELLED);
        assert!(reservation.commit().is_err());
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(tracker.is_profile_active("profile-1"));
        drop(reservation);
        assert!(!tracker.is_profile_active("profile-1"));
        assert!(tracker.launcher_state().unwrap().starting.is_empty());
        let next = tracker.reserve_launch("profile-1").unwrap();
        next.commit().unwrap();
        assert!(!tracker.cancel_launch("profile-1").unwrap());
        drop(next);
        let pending = tracker.reserve_launch("profile-1").unwrap();
        assert!(tracker.cancel_launch("profile-1").unwrap());
        assert!(pending.wait(async { Ok(()) }).await.is_err());
        assert!(pending.commit().is_err());
    }

    #[test]
    fn windows_path_comparison_accepts_case_prefix_and_slash_variants() {
        assert!(same_windows_path(
            r"\\?\D:\ShardX\runtime\browser.exe",
            r"d:/shardx/runtime/browser.exe"
        ));
        assert!(same_windows_path(r"D:\Profiles\one\", r"d:\profiles\ONE"));
        assert!(!same_windows_path(r"D:\Profiles\one", r"D:\Profiles\two"));
    }

    #[test]
    fn command_line_parser_keeps_quoted_user_data_dir_as_one_argument() {
        let args = windows_command_line_arguments(
            r#""D:\Runtime\browser.exe" "--user-data-dir=D:\Profiles\one two" --no-first-run"#,
        );
        assert_eq!(
            args,
            vec![
                r"D:\Runtime\browser.exe",
                r"--user-data-dir=D:\Profiles\one two",
                "--no-first-run",
            ]
        );
    }

    #[test]
    fn command_line_verification_requires_exact_user_data_dir_flag() {
        assert!(command_line_uses_user_data_dir(
            r#""D:\Runtime\browser.exe" "--user-data-dir=D:\Profiles\one two""#,
            r"d:\profiles\ONE TWO"
        ));
        assert!(command_line_uses_user_data_dir(
            r#"browser.exe --user-data-dir "D:\Profiles\one""#,
            r"D:\Profiles\one"
        ));
        assert!(!command_line_uses_user_data_dir(
            r#"browser.exe --profile-directory="D:\Profiles\one""#,
            r"D:\Profiles\one"
        ));
        assert!(!command_line_uses_user_data_dir(
            r#"browser.exe --user-data-dir=D:\Profiles\other"#,
            r"D:\Profiles\one"
        ));
    }

    #[test]
    fn command_line_verification_resolves_an_absolute_directory_alias() {
        let test = crate::store::TestStore::new();
        let directory = test.root.join("profile one");
        let alias = test.root.join("anchor");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&alias).unwrap();
        let expected = super::stable_path_string(&directory);
        for (name, matches) in [("profile one", true), ("anchor", false)] {
            let path = alias.join("..").join(name);
            let command = format!(r#"browser.exe "--user-data-dir={}""#, path.display());
            assert_eq!(command_line_uses_user_data_dir(&command, &expected), matches);
        }
    }

    #[test]
    fn lease_file_defaults_are_backward_tolerant() {
        let lease = ProcessLease {
            profile_id: "profile-1".into(),
            pid: 42,
            executable_path: r"D:\Runtime\browser.exe".into(),
            user_data_dir: r"D:\Profiles\profile-1".into(),
            run_id: "run-1".into(),
            temporary: false,
            process_started_at_100ns: 123,
            launched_at_unix_ms: 456,
        };
        let file = LeaseFile {
            format_version: LEASE_FORMAT_VERSION,
            consecutive_incomplete_recoveries: 0,
            leases: vec![lease],
        };
        let encoded = serde_json::to_string(&file).unwrap();
        let decoded: LeaseFile = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.leases.len(), 1);
        assert_eq!(decoded.leases[0].run_id, "run-1");
    }

    #[test]
    fn lease_identity_requires_creation_time_executable_and_user_data_dir() {
        let lease = ProcessLease {
            profile_id: "profile-1".into(),
            pid: 42,
            executable_path: r"D:\Runtime\browser.exe".into(),
            user_data_dir: r"D:\Profiles\profile-1".into(),
            run_id: "run-1".into(),
            temporary: false,
            process_started_at_100ns: 123,
            launched_at_unix_ms: 456,
        };
        let matching = super::LiveProcess {
            executable_path: r"d:/runtime/BROWSER.exe".into(),
            started_at_100ns: 123,
            command_line: Some(r#"browser.exe --user-data-dir="D:\Profiles\profile-1""#.into()),
        };
        assert!(super::lease_matches_process(&lease, &matching));

        let reused_pid = super::LiveProcess {
            started_at_100ns: 124,
            ..matching
        };
        assert!(!super::lease_matches_process(&lease, &reused_pid));
    }
}

#[derive(serde::Serialize)]
pub struct LauncherProcessState {
    pub running: Vec<String>,
    pub starting: Vec<String>,
    pub stopping: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RunningProfile {
    pub profile_id: String,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdp: Option<CdpInfo>,
    pub cdp_error: Option<String>,
    /// Milliseconds since the engine was spawned; frontend formats as
    /// "1h 23m" / "12m 30s" / "45s".
    pub uptime_ms: u64,
}
