// Tracker for launched ShardX child processes; keyed by profile_id.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;
use tokio::process::Child;

pub struct Tracker {
    inner: Mutex<HashMap<String, ChildEntry>>,
    launching: Mutex<HashSet<String>>,
}

/// Keeps a profile reserved while launch preflight is in progress. Dropping
/// the guard after an error releases the reservation; successful tracking also
/// clears it once the child process becomes authoritative.
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
    closer: tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<Result<()>>>,
    /// Set once DevToolsActivePort is read; None for UI launches.
    cdp: Option<CdpInfo>,
    /// Process start; serialised as elapsed ms in RunningProfile.
    started_at: Instant,
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
        }
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

    /// Take a spawned child + monitor it; entry removed only after a real exit.
    pub fn track(self: &'static Self, profile_id: String, mut child: Child, temporary: bool) -> u32 {
        let pid = child.id().unwrap_or(0);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        {
            let mut g = self.inner.lock().unwrap();
            g.insert(
                profile_id.clone(),
                ChildEntry { pid, closer: tx, cdp: None, started_at: Instant::now() },
            );
        }
        if let Ok(mut launching) = self.launching.lock() {
            launching.remove(&profile_id);
        }

        // A launcher stop request posts WM_CLOSE and then waits without a
        // deadline. Chromium alone decides when shutdown is complete, so its
        // Cookie, OAuth and Local State writes are never cut off by this app.
        let started_at = Instant::now();
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
            // Bump the persisted total runtime; non-temporary only (temp
            // profiles get deleted below so their counter is moot). Keep the
            // tracker entry authoritative until this read/modify/write is
            // finished, preventing a newly-opened editor from being overwritten
            // by the stale post-exit profile snapshot.
            if !temporary {
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                if let Err(e) = crate::profile::add_runtime(&profile_id, elapsed_ms) {
                    eprintln!("[launcher] add_runtime({profile_id}) failed: {e}");
                }
            }
            if let Ok(mut g) = Self::shared().inner.lock() {
                g.remove(&profile_id);
            }
            // Tear down temporary profile (config + udd) on close.
            if temporary {
                match crate::profile::delete(&profile_id) {
                    Ok(()) => eprintln!("[launcher] temporary profile {profile_id} deleted on close"),
                    Err(e) => eprintln!("[launcher] temporary profile {profile_id} cleanup failed: {e}"),
                }
            }
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

    pub async fn close(&self, profile_id: &str) -> Result<bool> {
        let closer = {
            let g = self.inner.lock().unwrap();
            g.get(profile_id).map(|e| e.closer.clone())
        };
        if let Some(closer) = closer {
            let (reply, completion) = tokio::sync::oneshot::channel();
            if closer.send(reply).await.is_err() {
                if self.is_running_profile(profile_id) {
                    anyhow::bail!("browser profile is already closing");
                }
                return Ok(false);
            }
            match completion.await {
                Ok(result) => result?,
                Err(_) if !self.is_running_profile(profile_id) => {
                    // The process can exit naturally at the same instant the
                    // close request is queued. That is already a clean result.
                }
                Err(_) => anyhow::bail!("browser close monitor stopped unexpectedly"),
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn shared() -> &'static Tracker {
        static INSTANCE: std::sync::OnceLock<Tracker> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(Tracker::new)
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
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("profile resource lock poisoned"))
}

#[cfg(test)]
mod tests {
    use super::Tracker;

    #[test]
    fn launch_reservation_blocks_duplicate_starts_until_released() {
        let tracker = Tracker::new();
        let reservation = tracker.reserve_launch("profile-1").unwrap();

        assert!(tracker.is_profile_active("profile-1"));
        assert!(tracker.reserve_launch("profile-1").is_err());

        drop(reservation);
        assert!(!tracker.is_profile_active("profile-1"));
        assert!(tracker.reserve_launch("profile-1").is_ok());
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
