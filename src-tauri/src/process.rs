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
    Mutex, MutexGuard,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Child;

const LEASE_FORMAT_VERSION: u32 = 1;
/// Consecutive failed recovery attempts after which unresolved leases are
/// archived out of the active set, so a persistently failing process check
/// (endpoint-security product, protected-process PID reuse) cannot keep the
/// launcher locked forever.
const LEASE_SELF_HEAL_THRESHOLD: u32 = 3;
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
        anyhow::bail!(
            "browser process lease recovery is incomplete; restart the launcher — if the process check keeps failing, unresolved leases are archived automatically after {LEASE_SELF_HEAL_THRESHOLD} consecutive attempts"
        );
    }
    Ok(())
}

pub struct Tracker {
    inner: Mutex<HashMap<String, ChildEntry>>,
    launching: Mutex<HashSet<String>>,
    /// Leases that could not be inspected during startup. They must survive
    /// later lifecycle writes so the next launcher start remains fail-closed.
    unverified: Mutex<Vec<ProcessLease>>,
    /// Serializes read/modify/write lease-file operations with tracker changes.
    lease_file: Mutex<()>,
    /// Consecutive incomplete recoveries, mirrored into the lease file so the
    /// count survives restarts (see LEASE_SELF_HEAL_THRESHOLD).
    lease_incomplete_count: Mutex<u32>,
}

/// Keeps a profile reserved while launch preflight is in progress. Dropping
/// the guard after an error releases the reservation; successful tracking also
/// clears it once the process lease becomes authoritative.
pub struct LaunchReservation<'a> {
    tracker: &'a Tracker,
    profile_id: String,
}

impl Drop for LaunchReservation<'_> {
    fn drop(&mut self) {
        if let Ok(mut launching) = self.tracker.launching.lock() {
            launching.remove(&self.profile_id);
        }
    }
}

struct ChildEntry {
    pid: u32,
    closer: ProcessCloser,
    /// Set once DevToolsActivePort is read; None for UI launches or recovered runs.
    cdp: Option<CdpInfo>,
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
    /// Tracked across restarts so the fail-closed lock can self-heal after
    /// `LEASE_SELF_HEAL_THRESHOLD` failed attempts.
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
            launching: Mutex::new(HashSet::new()),
            unverified: Mutex::new(Vec::new()),
            lease_file: Mutex::new(()),
            lease_incomplete_count: Mutex::new(0),
        }
    }

    /// Restore leases before profile mutation or temporary-profile cleanup. A
    /// lease is accepted only if the PID still identifies the same executable
    /// and process creation time; when Windows exposes the command line, its
    /// --user-data-dir must also match the persisted directory.
    pub fn recover(&'static self) -> Result<usize> {
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

        // Persist and expose every verified live lease before reporting an
        // incomplete inspection. This preserves the mutation/launch lock for
        // browsers whose identity was proven even if another lease needs a
        // later recovery attempt.
        {
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
                entries.insert(
                    lease.profile_id.clone(),
                    ChildEntry {
                        pid: lease.pid,
                        closer: ProcessCloser::Recovered,
                        cdp: None,
                        started_at: instant_from_lease(lease),
                        lease: lease.clone(),
                        closing: false,
                    },
                );
            }
            *unverified = unresolved;
            drop(unverified);
            if let Err(error) = self.persist_leases_locked(&entries) {
                eprintln!("[launcher] persist recovered process leases failed: {error:#}");
            }
        }

        for lease in &recovered {
            self.spawn_recovery_monitor(lease.profile_id.clone(), lease.run_id.clone());
        }

        if inspection_failed {
            let failures = self.bump_incomplete_recoveries();
            if failures < LEASE_SELF_HEAL_THRESHOLD {
                set_lease_recovery_incomplete(true);
                anyhow::bail!("one or more persisted process leases could not be verified");
            }
            // Self-heal: the process check has now failed on this many
            // consecutive starts, so the fail-closed lock would otherwise be
            // permanent. Archive the unverifiable leases out of the active
            // set (audit trail survives in a sibling file); a genuinely live
            // browser remains guarded by the engine's own user-data
            // singleton lock.
            eprintln!(
                "[launcher] {failures} consecutive incomplete lease recoveries; archiving unresolved leases"
            );
            let unresolved: Vec<ProcessLease> = {
                let mut unverified = self
                    .unverified
                    .lock()
                    .map_err(|_| anyhow::anyhow!("process unverified lease lock poisoned"))?;
                std::mem::take(&mut *unverified)
            };
            let archived_at_unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let payload = serde_json::to_vec_pretty(&serde_json::json!({
                "format_version": LEASE_FORMAT_VERSION,
                "archived_at_unix_ms": archived_at_unix_ms,
                "leases": unresolved,
            }))?;
            let archive_path = crate::store::process_leases_path()?
                .with_file_name("process-leases.unresolved.json");
            crate::store::atomic_write(&archive_path, &payload)?;
            self.reset_incomplete_recoveries();
            {
                let _lease_guard = self
                    .lease_file
                    .lock()
                    .map_err(|_| anyhow::anyhow!("process lease lock poisoned"))?;
                let entries = self
                    .inner
                    .lock()
                    .map_err(|_| anyhow::anyhow!("process tracker lock poisoned"))?;
                if let Err(error) = self.persist_leases_locked(&entries) {
                    eprintln!(
                        "[launcher] persist process leases after self-heal failed: {error:#}"
                    );
                }
            }
            set_lease_recovery_incomplete(false);
            return Ok(recovered.len());
        }

        self.reset_incomplete_recoveries();
        set_lease_recovery_incomplete(false);
        Ok(recovered.len())
    }

    /// Atomically reserve one profile for launch. This blocks both duplicate
    /// starts and mutations during the potentially slow launch preflight.
    pub fn reserve_launch(&self, profile_id: &str) -> Result<LaunchReservation<'_>> {
        // Serialize launch reservation against proxy edits/deletes. Whichever
        // operation starts first completes its atomic state transition first.
        let _resource_guard = lock_profile_resources()?;
        if self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("process tracker lock poisoned"))?
            .contains_key(profile_id)
        {
            anyhow::bail!("This browser profile is already running");
        }

        let mut launching = self
            .launching
            .lock()
            .map_err(|_| anyhow::anyhow!("launch reservation lock poisoned"))?;
        if !launching.insert(profile_id.to_string()) {
            anyhow::bail!("This browser profile is already starting");
        }

        Ok(LaunchReservation {
            tracker: self,
            profile_id: profile_id.to_string(),
        })
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

        self.insert_entry(ChildEntry {
            pid,
            closer: ProcessCloser::Child(tx),
            cdp: None,
            started_at: Instant::now(),
            lease,
            closing: false,
        });
        if let Ok(mut launching) = self.launching.lock() {
            launching.remove(&profile_id);
        }

        // A launcher stop request posts WM_CLOSE and then waits without a
        // deadline. Chromium alone decides when shutdown is complete, so its
        // Cookie, OAuth and Local State writes are never cut off by this app.
        tokio::spawn(async move {
            let close_completion = tokio::select! {
                wait_result = child.wait() => {
                    if let Err(error) = wait_result {
                        eprintln!("[launcher] wait for browser profile {profile_id} failed: {error}");
                    }
                    None
                }
                request = rx.recv() => {
                    rx.close();
                    match request {
                        Some(reply) => {
                            let close_result = child
                                .id()
                                .map(request_graceful_browser_close)
                                .unwrap_or(Ok(()));
                            if let Err(error) = close_result {
                                if matches!(child.try_wait(), Ok(Some(_))) {
                                    // The process exited between select() and
                                    // taskkill. It already closed cleanly.
                                    Some((reply, Ok(())))
                                } else {
                                    let _ = reply.send(Err(error));
                                    if let Err(wait_error) = child.wait().await {
                                        eprintln!("[launcher] wait for browser profile {profile_id} after close failure failed: {wait_error}");
                                    }
                                    None
                                }
                            } else {
                                let wait_result = child
                                    .wait()
                                    .await
                                    .map(|_| ())
                                    .with_context(|| format!("wait for browser profile {profile_id} to close"));
                                Some((reply, wait_result))
                            }
                        }
                        None => {
                            if let Err(error) = child.wait().await {
                                eprintln!("[launcher] wait for browser profile {profile_id} failed: {error}");
                            }
                            None
                        }
                    }
                }
            };
            // Removing the tracker entry also removes the persisted lease. A
            // stale lease can never outlive a normally observed child exit.
            Self::shared().finish_run(&profile_id, &run_id, true, true);
            if let Some((reply, result)) = close_completion {
                let _ = reply.send(result);
            }
        });

        pid
    }

    /// Attach CDP to a tracked profile; no-op if the profile already exited.
    pub fn set_cdp(&self, profile_id: &str, cdp: CdpInfo) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(e) = g.get_mut(profile_id) {
                e.cdp = Some(cdp);
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
                uptime_ms: e.started_at.elapsed().as_millis() as u64,
            })
            .collect()
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
            .map(|profiles| profiles.contains(profile_id))
            .unwrap_or(false)
    }

    pub fn active_profile_ids(&self) -> Vec<String> {
        let mut active: HashSet<String> = self
            .inner
            .lock()
            .map(|entries| entries.keys().cloned().collect())
            .unwrap_or_default();
        if let Ok(launching) = self.launching.lock() {
            active.extend(launching.iter().cloned());
        }
        active.into_iter().collect()
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
                anyhow::bail!("browser profile is already closing");
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
        *count += 1;
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
            loop {
                let lease = {
                    let Ok(entries) = Self::shared().inner.lock() else {
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
                        Self::shared().finish_run(&profile_id, &run_id, true, true);
                        return;
                    }
                    Ok(Some(_)) => {
                        // Do not delete a temporary profile if identity changed:
                        // a PID-reuse mismatch must never trigger data cleanup.
                        eprintln!(
                            "[launcher] recovered process identity changed for {profile_id}; releasing lease without cleanup"
                        );
                        Self::shared().finish_run(&profile_id, &run_id, false, false);
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

        request_graceful_browser_close(lease.pid)?;
        loop {
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
        }
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
    ensure_lease_recovery_complete()?;
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("profile resource lock poisoned"))
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
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some((name, value)) = arg.split_once('=') {
            if name.eq_ignore_ascii_case("--user-data-dir") {
                return same_windows_path(value, expected_user_data_dir);
            }
        }
        if arg.eq_ignore_ascii_case("--user-data-dir") {
            return args
                .get(index + 1)
                .is_some_and(|value| same_windows_path(value, expected_user_data_dir));
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
        let result = unsafe {
            let executable_path = full_image_name(process)?;
            let started_at_100ns = process_creation_time(process)?;
            let command_line = read_command_line(pid);
            Ok(LiveProcess {
                executable_path,
                started_at_100ns,
                command_line,
            })
        };
        unsafe {
            CloseHandle(process);
        }
        result.map(Some)
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

    unsafe fn process_creation_time(process: isize) -> std::result::Result<u64, String> {
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
        Ok((u64::from(creation.high_date_time) << 32) | u64::from(creation.low_date_time))
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
    fn launch_reservation_blocks_duplicate_starts_until_released() {
        let tracker = super::Tracker::new();
        let reservation = tracker.reserve_launch("profile-1").unwrap();

        assert!(tracker.is_profile_active("profile-1"));
        assert!(tracker.reserve_launch("profile-1").is_err());

        drop(reservation);
        assert!(!tracker.is_profile_active("profile-1"));
        assert!(tracker.reserve_launch("profile-1").is_ok());
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct RunningProfile {
    pub profile_id: String,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdp: Option<CdpInfo>,
    /// Milliseconds since the engine was spawned; frontend formats as
    /// "1h 23m" / "12m 30s" / "45s".
    pub uptime_ms: u64,
}
