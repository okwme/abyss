//! Concurrency cap for backend calls (plan section 1).
//!
//! A permit is taken *before* a task is spawned. When a call hits its deadline,
//! its permit is parked rather than returned: the request may still sit in the
//! Telegram library's unbounded queues, and only retiring the client instance
//! (task 4's rebuild) drops those queues. So the library never holds more than
//! [`MAX_CONCURRENT`] of our requests.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Maximum backend calls in flight, including timed-out ones that are parked.
pub const MAX_CONCURRENT: usize = 16;

#[derive(Debug)]
pub(super) struct Admission {
    sem: Arc<Semaphore>,
    parked: Vec<OwnedSemaphorePermit>,
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            sem: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            parked: Vec::with_capacity(MAX_CONCURRENT),
        }
    }
}

impl Admission {
    pub(super) fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.sem).try_acquire_owned().ok()
    }

    /// Keeps a timed-out call's permit out of circulation until [`release_parked`].
    pub(super) fn park(&mut self, permit: OwnedSemaphorePermit) {
        debug_assert!(self.parked.len() < MAX_CONCURRENT);
        self.parked.push(permit);
    }

    pub(super) fn parked(&self) -> usize {
        self.parked.len()
    }

    #[cfg(test)]
    pub(super) fn available(&self) -> usize {
        self.sem.available_permits()
    }

    /// The semaphore itself, so tests can observe permits while the actor runs.
    #[cfg(all(test, feature = "fake"))]
    pub(super) fn probe(&self) -> Arc<Semaphore> {
        Arc::clone(&self.sem)
    }

    /// Recycles parked permits. Call only once the client instance that owned the
    /// timed-out requests has fully retired.
    pub(super) fn release_parked(&mut self) -> usize {
        let n = self.parked.len();
        self.parked.clear();
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_permits_stay_out_until_released() {
        let mut a = Admission::default();
        let permits: Vec<_> = (0..MAX_CONCURRENT)
            .map(|_| a.try_acquire().unwrap())
            .collect();
        assert!(a.try_acquire().is_none(), "17th call must wait");
        for p in permits {
            a.park(p);
        }
        assert_eq!(a.parked(), MAX_CONCURRENT);
        assert!(a.try_acquire().is_none(), "parked permits are not recycled");
        assert_eq!(a.release_parked(), MAX_CONCURRENT);
        assert_eq!(a.available(), MAX_CONCURRENT);
    }
}
