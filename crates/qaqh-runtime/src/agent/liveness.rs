//! Worker liveness — the shared idle-unload signal between a Loop actor
//! (producer) and the daemon registry (consumer).
//!
//! One `Arc<WorkerLiveness>` is created per spawned worker at registry-spawn
//! time and cloned into both sides:
//!
//! - **Loop side** (`loop_core::safe_dispatch`): marks `busy` around each
//!   command dispatch, `touch()`es the activity clock, and mirrors
//!   `turn.is_suspended()` into `suspend_pending` (an unresolved ask /
//!   permission / plan review must never be unloaded — the resume path would
//!   seal it as an orphan interaction).
//! - **Registry side** (`AgentRegistry::unload_idle_sessions`): decides
//!   unloadability without touching the worker thread — atomics only, no
//!   locks shared with the actor.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default)]
pub struct WorkerLiveness {
    /// Epoch seconds of the last completed dispatch.
    last_activity: AtomicU64,
    /// A command dispatch is currently executing (turn running / tools running).
    busy: AtomicBool,
    /// A user interaction is suspended mid-turn (ask / permission / plan).
    /// While set, the session must not be idle-unloaded.
    suspend_pending: AtomicBool,
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl WorkerLiveness {
    pub fn new() -> Self {
        Self {
            last_activity: AtomicU64::new(now_epoch()),
            busy: AtomicBool::new(false),
            suspend_pending: AtomicBool::new(false),
        }
    }

    /// Refresh the activity clock (called when a dispatch completes).
    pub fn touch(&self) {
        self.last_activity.store(now_epoch(), Ordering::Release);
    }

    pub fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::Release);
    }

    pub fn set_suspend_pending(&self, pending: bool) {
        self.suspend_pending.store(pending, Ordering::Release);
    }

    /// Seconds since the last completed dispatch.
    pub fn idle_secs(&self) -> u64 {
        now_epoch().saturating_sub(self.last_activity.load(Ordering::Acquire))
    }

    /// Whether the worker may be idle-unloaded right now.
    pub fn unloadable(&self) -> bool {
        !self.busy.load(Ordering::Acquire) && !self.suspend_pending.load(Ordering::Acquire)
    }

    /// Test/ops hook: rewind the activity clock so the worker immediately
    /// qualifies as idle for `secs` more seconds.
    #[doc(hidden)]
    pub fn rewind_last_activity(&self, secs: u64) {
        let past = now_epoch().saturating_sub(secs);
        self.last_activity.store(past, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_worker_is_not_idle_and_unloadable() {
        let l = WorkerLiveness::new();
        assert_eq!(l.idle_secs(), 0);
        assert!(l.unloadable(), "idle & not busy & not suspended");
    }

    #[test]
    fn busy_or_suspended_blocks_unload() {
        let l = WorkerLiveness::new();
        l.set_busy(true);
        assert!(!l.unloadable());
        l.set_busy(false);
        l.set_suspend_pending(true);
        assert!(!l.unloadable(), "pending ask must block unload");
        l.set_suspend_pending(false);
        assert!(l.unloadable());
    }

    #[test]
    fn touch_resets_idle_clock() {
        let l = WorkerLiveness::new();
        l.last_activity
            .store(now_epoch().saturating_sub(3600), Ordering::Release);
        assert!(l.idle_secs() >= 3600);
        l.touch();
        assert!(l.idle_secs() <= 1);
    }
}
