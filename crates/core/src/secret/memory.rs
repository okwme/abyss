//! In-memory and fault-injecting stores for tests and the fake backend.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use zeroize::Zeroizing;

use super::{SecretItem, SecretStore, StoreError};

/// Longest a held operation waits for [`FaultControl::release`]. Bounds every
/// test that forgets to release.
const MAX_HOLD: Duration = Duration::from_secs(10);

/// Thread-safe in-memory store. Clones share storage, so a test can keep one
/// clone to inspect what the persistence worker wrote.
#[derive(Clone, Default)]
pub struct MemoryStore {
    items: Arc<Mutex<BTreeMap<SecretItem, Zeroizing<Vec<u8>>>>>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MemoryStore(<redacted>)")
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether an item exists (test inspection without copying the secret).
    pub fn contains(&self, item: SecretItem) -> bool {
        self.lock().contains_key(&item)
    }

    /// Every operation is a single map call, so a poisoned map is still consistent.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<SecretItem, Zeroizing<Vec<u8>>>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, item: SecretItem) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        Ok(self.lock().get(&item).map(|v| Zeroizing::new(v.to_vec())))
    }

    fn set(&self, item: SecretItem, secret: &[u8]) -> Result<(), StoreError> {
        self.lock().insert(item, Zeroizing::new(secret.to_vec()));
        Ok(())
    }

    fn delete(&self, item: SecretItem) -> Result<(), StoreError> {
        self.lock().remove(&item);
        Ok(())
    }
}

/// An operation kind `FaultyStore` can fail or count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreOp {
    Get,
    Set,
    Delete,
}

impl StoreOp {
    const fn bit(self) -> u8 {
        match self {
            Self::Get => 1,
            Self::Set => 2,
            Self::Delete => 4,
        }
    }
}

#[derive(Default)]
struct Faults {
    delay_ms: AtomicU64,
    failing: AtomicU8,
    /// Deletions of these items fail (one bit per item).
    failing_deletes: AtomicU8,
    calls: AtomicU64,
    held: Mutex<bool>,
    released: Condvar,
}

/// Test handle that steers a [`FaultyStore`] after it moved into the worker.
#[derive(Clone, Default)]
pub struct FaultControl(Arc<Faults>);

impl std::fmt::Debug for FaultControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultControl")
            .field("calls", &self.calls())
            .finish_non_exhaustive()
    }
}

impl FaultControl {
    /// Every operation sleeps this long before running (saturates at `u64::MAX` ms).
    pub fn set_delay(&self, delay: Duration) {
        let ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        self.0.delay_ms.store(ms, Ordering::SeqCst);
    }

    /// Makes `op` return [`StoreError::Injected`] until [`Self::heal`].
    pub fn fail(&self, op: StoreOp) {
        self.0.failing.fetch_or(op.bit(), Ordering::SeqCst);
    }

    pub fn heal(&self, op: StoreOp) {
        self.0.failing.fetch_and(!op.bit(), Ordering::SeqCst);
    }

    /// Makes deleting `item` return [`StoreError::Injected`], leaving other
    /// items' deletions alone.
    pub fn fail_delete_of(&self, item: SecretItem) {
        self.0
            .failing_deletes
            .fetch_or(item_bit(item), Ordering::SeqCst);
    }

    fn delete_fails(&self, item: SecretItem) -> bool {
        self.0.failing_deletes.load(Ordering::SeqCst) & item_bit(item) != 0
    }

    /// Blocks every subsequent operation until [`Self::release`] (at most 10 s).
    pub fn hold(&self) {
        if let Ok(mut held) = self.0.held.lock() {
            *held = true;
        }
    }

    pub fn release(&self) {
        if let Ok(mut held) = self.0.held.lock() {
            *held = false;
        }
        self.0.released.notify_all();
    }

    /// Number of operations started so far.
    pub fn calls(&self) -> u64 {
        self.0.calls.load(Ordering::SeqCst)
    }

    fn before(&self, op: StoreOp) -> Result<(), StoreError> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(held) = self.0.held.lock() {
            // A timeout or poisoned lock just ends the hold; the bound matters
            // more than the reason.
            let _ = self
                .0
                .released
                .wait_timeout_while(held, MAX_HOLD, |held| *held);
        }
        let ms = self.0.delay_ms.load(Ordering::SeqCst);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
        if self.0.failing.load(Ordering::SeqCst) & op.bit() == 0 {
            Ok(())
        } else {
            Err(StoreError::Injected)
        }
    }
}

/// Wraps a store and injects delays, holds and failures, steered by a
/// [`FaultControl`].
#[derive(Debug)]
pub struct FaultyStore<S> {
    inner: S,
    control: FaultControl,
}

impl<S: SecretStore> FaultyStore<S> {
    pub fn new(inner: S) -> (Self, FaultControl) {
        let control = FaultControl::default();
        (
            Self {
                inner,
                control: control.clone(),
            },
            control,
        )
    }
}

impl<S: SecretStore> SecretStore for FaultyStore<S> {
    fn get(&self, item: SecretItem) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        self.control.before(StoreOp::Get)?;
        self.inner.get(item)
    }

    fn set(&self, item: SecretItem, secret: &[u8]) -> Result<(), StoreError> {
        self.control.before(StoreOp::Set)?;
        self.inner.set(item, secret)
    }

    fn delete(&self, item: SecretItem) -> Result<(), StoreError> {
        self.control.before(StoreOp::Delete)?;
        if self.control.delete_fails(item) {
            return Err(StoreError::Injected);
        }
        self.inner.delete(item)
    }
}

const fn item_bit(item: SecretItem) -> u8 {
    match item {
        SecretItem::Session => 1,
        SecretItem::CacheKey => 2,
    }
}
