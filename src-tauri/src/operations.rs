//! Short-lived reservations share the profile resource lock with launch/save.
//! File compression and runtime downloads never hold that global mutex.
use anyhow::{ensure, Result};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct State {
    profiles: HashSet<String>,
    runtime: bool,
    jobs: usize,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

pub struct Reservation {
    profiles: Vec<String>,
    runtime: bool,
}

pub struct Activity;
impl Drop for Activity {
    fn drop(&mut self) {
        state().lock().unwrap_or_else(|e| e.into_inner()).jobs -= 1;
    }
}
pub fn activity() -> Activity {
    state().lock().unwrap_or_else(|e| e.into_inner()).jobs += 1;
    Activity
}

#[derive(serde::Serialize)]
pub struct Status {
    pub profiles: Vec<String>,
    pub runtime: bool,
    pub jobs: usize,
}
pub fn status() -> Status {
    let state = state().lock().unwrap_or_else(|e| e.into_inner());
    Status {
        profiles: state.profiles.iter().cloned().collect(),
        runtime: state.runtime,
        jobs: state.jobs,
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
        for id in &self.profiles {
            state.profiles.remove(id);
        }
        if self.runtime {
            state.runtime = false;
        }
    }
}

pub fn ensure_runtime_available() -> Result<()> {
    ensure!(
        !state()
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state lock poisoned"))?
            .runtime,
        crate::errors::conflict("Runtime update is in progress. Wait for it to finish.")
    );
    Ok(())
}

pub fn ensure_profile_available(id: &str) -> Result<()> {
    ensure_runtime_available()?;
    ensure!(
        !busy_profiles().iter().any(|busy| busy == id),
        crate::errors::conflict("Profile backup is in progress. Wait for it to finish.")
    );
    Ok(())
}

pub fn busy_profiles() -> Vec<String> {
    state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .profiles
        .iter()
        .cloned()
        .collect()
}

pub fn is_busy() -> bool {
    let state = state().lock().unwrap_or_else(|e| e.into_inner());
    state.runtime || !state.profiles.is_empty() || state.jobs > 0
}

pub fn reserve_profiles(ids: &[String]) -> Result<Reservation> {
    let _guard = crate::process::lock_profile_resources()?;
    crate::profile::recover_interrupted_operations_locked()?;
    for id in ids {
        crate::profile::ensure_stopped(id)?;
    }
    state()
        .lock()
        .map_err(|_| anyhow::anyhow!("operation state lock poisoned"))?
        .profiles
        .extend(ids.iter().cloned());
    Ok(Reservation {
        profiles: ids.to_vec(),
        runtime: false,
    })
}

pub fn reserve_runtime() -> Result<Reservation> {
    let _guard = crate::process::lock_profile_resources()?;
    crate::profile::recover_interrupted_operations_locked()?;
    ensure!(
        crate::process::Tracker::shared()
            .active_profile_ids()
            .is_empty(),
        crate::errors::conflict(
            "Stop all running or starting browsers before updating the runtime"
        )
    );
    let mut state = state()
        .lock()
        .map_err(|_| anyhow::anyhow!("operation state lock poisoned"))?;
    ensure!(
        !state.runtime && state.profiles.is_empty(),
        crate::errors::conflict(
            "Another storage or runtime operation is in progress. Wait for it to finish."
        )
    );
    state.runtime = true;
    Ok(Reservation {
        profiles: Vec::new(),
        runtime: true,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn backup_and_runtime_reservations_block_conflicting_writers_and_launches() {
        let _test = crate::store::TestStore::new();
        let backup = super::reserve_profiles(&["busy-profile".into()]).unwrap();
        assert!(crate::profile::ensure_stopped("busy-profile").is_err());
        assert!(crate::process::Tracker::shared()
            .reserve_launch("busy-profile")
            .is_err());
        assert!(super::reserve_runtime().is_err());
        assert!(crate::profile::ensure_stopped("other-profile").is_ok());
        drop(backup);
        let runtime = super::reserve_runtime().unwrap();
        assert!(crate::process::Tracker::shared()
            .reserve_launch("other-profile")
            .is_err());
        assert!(super::reserve_profiles(&["other-profile".into()]).is_err());
        drop(runtime);
        assert!(crate::process::Tracker::shared()
            .reserve_launch("other-profile")
            .is_ok());
    }
}
