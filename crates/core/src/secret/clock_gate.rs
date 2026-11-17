//! Keeps a paused test clock still while the persistence thread works
//! (review round 31, tests only).
//!
//! The persistence worker is an OS thread, which a paused tokio clock cannot
//! see: an actor waiting on it looks idle, and the runtime auto-advances to
//! the next timer, so hundreds of virtual seconds (a call deadline, the
//! shutdown deadline, a test's wait budget) can pass while the store answers
//! in a few milliseconds. While a request is awaited ([`ClockGate::busy`]) a
//! blocking task runs, and tokio does not auto-advance while one does. A test
//! hold ([`super::FaultControl::hold`]) is meant to let time pass, so it
//! lifts the gate until [`super::FaultControl::release`].

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

#[derive(Debug, Default)]
struct State {
    /// Requests awaited on the store thread.
    busy: u32,
    /// Store operations blocked on a test hold.
    held: u32,
    /// Bumped by a release: holds that began before it are over.
    epoch: u64,
    /// The blocking task that keeps the clock still is running.
    waiter: bool,
}

#[derive(Debug, Default)]
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

/// See the module docs. Clones share the gate.
#[derive(Clone, Debug, Default)]
pub struct ClockGate(Arc<Inner>);

/// A request awaited on the store thread; dropping it ends it.
#[derive(Debug)]
pub struct Busy(ClockGate);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        s.busy = s.busy.saturating_sub(1);
        drop(s);
        self.0.0.changed.notify_all();
    }
}

impl ClockGate {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A request is about to be awaited (async code, on the runtime).
    pub fn busy(&self) -> Busy {
        self.lock().busy += 1;
        self.ensure_waiter();
        Busy(self.clone())
    }

    /// Starts the blocking task that holds the clock, unless one runs, no
    /// request is awaited, or a test hold lets time pass.
    fn ensure_waiter(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut s = self.lock();
        if s.waiter || s.busy == 0 || s.held > 0 {
            return;
        }
        s.waiter = true;
        drop(s);
        let gate = self.clone();
        drop(runtime.spawn_blocking(move || gate.hold_clock()));
    }

    fn hold_clock(&self) {
        let mut s = self.lock();
        while s.busy > 0 && s.held == 0 {
            s = self
                .0
                .changed
                .wait(s)
                .unwrap_or_else(PoisonError::into_inner);
        }
        s.waiter = false;
    }

    /// A store operation blocks on a test hold (store thread): time may
    /// pass. Returns the epoch for [`Self::hold_ended`].
    pub(super) fn hold_began(&self) -> u64 {
        let mut s = self.lock();
        s.held += 1;
        let epoch = s.epoch;
        drop(s);
        self.0.changed.notify_all();
        epoch
    }

    /// A hold ended without a release (its bound passed).
    pub(super) fn hold_ended(&self, epoch: u64) {
        let mut s = self.lock();
        if s.epoch == epoch {
            s.held = s.held.saturating_sub(1);
        }
    }

    /// The test released every hold (async code): the clock is held again
    /// while the released operations finish.
    pub(super) fn released(&self) {
        let mut s = self.lock();
        s.held = 0;
        s.epoch = s.epoch.wrapping_add(1);
        drop(s);
        self.ensure_waiter();
    }
}
