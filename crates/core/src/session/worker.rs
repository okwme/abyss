//! Persistence worker (plan section 3): the single owner of the
//! [`SecretStore`], on a dedicated OS thread (`abyss-persist`).
//!
//! Messages are processed strictly in order, one at a time, and every store
//! call blocks only that thread, never an async runtime thread. Ordering is
//! what makes logout safe: `Delete` runs after every write queued before it,
//! and once a generation is terminated nothing written for it is ever applied.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use super::cell::SessionSnapshot;
use super::critical::{CriticalState, Decoded, decode, encode};
use crate::cache::{CacheKey, DeleteKey, KeyError};
use crate::secret::{
    CacheKeyError, SecretItem, SecretStore, StoreError, create_cache_key, delete_cache_key,
    load_or_create_cache_key,
};

/// Queue bound. Writes happen only when critical bytes change (a handful per
/// login), and every other message has a waiting caller, so this is never
/// reached in practice; a full queue is reported, never blocked on, by writers.
pub const PERSIST_QUEUE: usize = 64;

/// Persistence generation: one per signed-in account lifetime (plan section 3).
/// Starts at login or at startup with a loaded item, ends at logout. Separate
/// from `ClientGen`: rebuilds keep it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountGen(u64);

/// Generation of detached snapshots; real generations start at 1.
#[cfg(any(test, feature = "fake"))]
const DETACHED: AccountGen = AccountGen(0);

/// Why a persistence request did not confirm durability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistError {
    /// The store failed the write or delete.
    Store(StoreError),
    /// The generation was terminated (logout); its state is no longer saved.
    Terminated,
    /// The write for this revision never reached the store (queue full).
    NotQueued,
    /// The worker thread is gone.
    Stopped,
    /// A blocking call was made from an async runtime thread.
    InAsyncContext,
    /// The session lock was poisoned by a panic.
    Poisoned,
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "session store: {e}"),
            Self::Terminated => f.write_str("session generation was terminated"),
            Self::NotQueued => f.write_str("session write was not queued"),
            Self::Stopped => f.write_str("persistence worker stopped"),
            Self::InAsyncContext => f.write_str("blocking persistence call on an async thread"),
            Self::Poisoned => f.write_str("session lock poisoned"),
        }
    }
}

impl std::error::Error for PersistError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            _ => None,
        }
    }
}

type Reply<T> = oneshot::Sender<T>;

pub(crate) enum PersistMsg {
    Write {
        account: AccountGen,
        rev: u64,
        blob: Zeroizing<Vec<u8>>,
    },
    Barrier {
        account: AccountGen,
        rev: u64,
        reply: Reply<Result<(), PersistError>>,
    },
    Terminate {
        account: AccountGen,
    },
    Delete {
        account: AccountGen,
        reply: Reply<Result<(), PersistError>>,
    },
    LoadSession {
        reply: Reply<Result<Option<Zeroizing<Vec<u8>>>, StoreError>>,
    },
    CacheKey {
        mode: KeyMode,
        reply: Reply<Result<CacheKey, CacheKeyError>>,
    },
    /// Replies on a std channel: the caller is the cache worker thread.
    DeleteCacheKey {
        reply: std::sync::mpsc::SyncSender<Result<(), StoreError>>,
    },
    /// Async variant, for retrying a logout whose cache erase failed.
    ForgetCacheKey {
        reply: Reply<Result<(), StoreError>>,
    },
    /// A plain item: the API credentials (plan section 17.1) or the
    /// logout-pending marker (review round 15, A). Ordered like every other
    /// message, so a delete queued after a write wins.
    LoadItem {
        item: SecretItem,
        reply: Reply<Result<Option<Zeroizing<Vec<u8>>>, StoreError>>,
    },
    StoreItem {
        item: SecretItem,
        blob: Zeroizing<Vec<u8>>,
        reply: Reply<Result<(), StoreError>>,
    },
    DeleteItem {
        item: SecretItem,
        reply: Reply<Result<(), StoreError>>,
    },
}

impl std::fmt::Debug for PersistMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PersistMsg")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyMode {
    LoadOrCreate,
    Replace,
}

/// What `load_session` found in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStatus {
    /// No item: first run or after logout.
    Missing,
    /// A valid item; the snapshot holds it at `rev` 0, already durable.
    Loaded,
    /// An item of another format version. Treated as logged out; the item is
    /// overwritten by the next login or removed by the next logout.
    UnknownVersion(u16),
    /// An unreadable item. Treated as logged out, as above.
    Corrupt,
}

/// The startup session state.
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub snapshot: SessionSnapshot,
    pub status: LoadStatus,
}

/// Async handle to the persistence worker. Cloneable; the thread exits once
/// every handle (and every session cell, which holds a sender) is dropped.
#[derive(Clone, Debug)]
pub struct PersistHandle {
    tx: mpsc::Sender<PersistMsg>,
    next_gen: Arc<AtomicU64>,
    /// Tests: keeps a paused clock still while a request is awaited
    /// (`secret::ClockGate`).
    #[cfg(test)]
    gate: Option<crate::secret::ClockGate>,
}

#[cfg(any(test, feature = "fake"))]
impl SessionSnapshot {
    /// A logged-out session state with no persistence worker behind it, for the
    /// fake backend and tests. Mutations still advance `rev`, but nothing is
    /// ever stored: every barrier reports [`PersistError::NotQueued`] (after a
    /// change) or [`PersistError::Stopped`].
    pub fn detached() -> Self {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let state = CriticalState::default();
        let blob = encode(&state).unwrap_or_default();
        Self::with_sender(state, blob, DETACHED, tx)
    }
}

impl PersistHandle {
    /// Starts the worker thread, which takes sole ownership of `store`.
    pub fn spawn(store: impl SecretStore) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel(PERSIST_QUEUE);
        #[cfg(test)]
        let gate = store.clock_gate();
        let worker = Worker {
            store: Box::new(store),
            terminated: None,
            current: None,
        };
        std::thread::Builder::new()
            .name("abyss-persist".into())
            .spawn(move || worker.run(rx))?;
        Ok(Self {
            tx,
            next_gen: Arc::new(AtomicU64::new(1)),
            #[cfg(test)]
            gate,
        })
    }

    fn new_gen(&self) -> AccountGen {
        AccountGen(self.next_gen.fetch_add(1, Ordering::Relaxed))
    }

    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> PersistMsg) -> Option<T> {
        let (reply, rx) = oneshot::channel();
        #[cfg(test)]
        let _busy = self.gate.as_ref().map(crate::secret::ClockGate::busy);
        self.tx.send(make(reply)).await.ok()?;
        rx.await.ok()
    }

    /// Tests: see the `gate` field.
    #[cfg(test)]
    pub(crate) fn clock_gate(&self) -> Option<crate::secret::ClockGate> {
        self.gate.clone()
    }

    /// Reads the session item (process startup only; rebuilds reuse the live
    /// snapshot). Unknown versions and corrupt items count as logged out.
    pub async fn load_session(&self) -> Result<LoadedSession, StoreError> {
        let stored = self
            .ask(|reply| PersistMsg::LoadSession { reply })
            .await
            .ok_or(StoreError::Stopped)??;
        let (state, blob, status) = match stored.as_ref().map(|b| decode(b.as_slice())) {
            None => (None, None, LoadStatus::Missing),
            Some(Decoded::State(state)) => (Some(state), stored, LoadStatus::Loaded),
            Some(Decoded::UnknownVersion(v)) => (None, None, LoadStatus::UnknownVersion(v)),
            Some(Decoded::Corrupt) => (None, None, LoadStatus::Corrupt),
        };
        let snapshot = match (state, blob) {
            (Some(state), Some(blob)) => SessionSnapshot::new(state, blob, self.new_gen(), self),
            _ => self.fresh_session(),
        };
        Ok(LoadedSession { snapshot, status })
    }

    /// A logged-out session state under a new generation, for the first login
    /// or the login after a logout.
    pub fn fresh_session(&self) -> SessionSnapshot {
        let state = CriticalState::default();
        // Encoding the bounded default state cannot fail; an empty baseline
        // would only make the first mutation write once more.
        let blob = encode(&state).unwrap_or_default();
        SessionSnapshot::new(state, blob, self.new_gen(), self)
    }

    /// Stops all later writes for `account` (logout step 2).
    pub async fn terminate(&self, account: AccountGen) -> Result<(), PersistError> {
        self.tx
            .send(PersistMsg::Terminate { account })
            .await
            .map_err(|_| PersistError::Stopped)
    }

    /// Terminates `account` and deletes the session item after every queued write
    /// (logout step 5). Deleting a missing item succeeds, so a retry is safe.
    pub async fn delete_session(&self, account: AccountGen) -> Result<(), PersistError> {
        self.ask(|reply| PersistMsg::Delete { account, reply })
            .await
            .unwrap_or(Err(PersistError::Stopped))
    }

    /// Loads the cache key, creating it on first run (plan section 16).
    pub async fn load_or_create_cache_key(&self) -> Result<CacheKey, CacheKeyError> {
        self.cache_key(KeyMode::LoadOrCreate).await
    }

    /// Replaces the cache key, after a `Malformed` key's cache was destroyed.
    pub async fn create_cache_key(&self) -> Result<CacheKey, CacheKeyError> {
        self.cache_key(KeyMode::Replace).await
    }

    async fn cache_key(&self, mode: KeyMode) -> Result<CacheKey, CacheKeyError> {
        self.ask(|reply| PersistMsg::CacheKey { mode, reply })
            .await
            .unwrap_or(Err(CacheKeyError::Store(StoreError::Stopped)))
    }

    /// Deletes the cache key from an async context (a logout retry after the
    /// cache worker is gone). Succeeds when no key exists.
    pub async fn delete_cache_key(&self) -> Result<(), StoreError> {
        self.ask(|reply| PersistMsg::ForgetCacheKey { reply })
            .await
            .unwrap_or(Err(StoreError::Stopped))
    }

    /// A `DeleteKey` callback for `CacheHandle::erase`. It runs on the cache
    /// worker thread and blocks it until this worker has deleted the key.
    /// Called on an async runtime thread it fails instead of blocking.
    pub fn cache_key_deleter(&self) -> DeleteKey {
        let tx = self.tx.clone();
        Box::new(move || -> Result<(), KeyError> {
            if tokio::runtime::Handle::try_current().is_ok() {
                return Err(KeyError::from(PersistError::InAsyncContext));
            }
            let (reply, rx) = std::sync::mpsc::sync_channel(1);
            tx.blocking_send(PersistMsg::DeleteCacheKey { reply })
                .map_err(|_| KeyError::from(PersistError::Stopped))?;
            let result = rx
                .recv()
                .map_err(|_| KeyError::from(PersistError::Stopped))?;
            result.map_err(|e| KeyError::from(PersistError::Store(e)))
        })
    }

    /// Reads the API credentials item. `Ok(None)`: none stored; `Err`: the
    /// Keychain could not be read, which is not the same.
    pub async fn load_api_credentials(&self) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        self.load_item(SecretItem::ApiCredentials).await
    }

    /// Creates or replaces the API credentials item.
    pub async fn store_api_credentials(&self, blob: Zeroizing<Vec<u8>>) -> Result<(), StoreError> {
        self.store_item(SecretItem::ApiCredentials, blob).await
    }

    /// Deletes the API credentials item; succeeds when none exists.
    pub async fn delete_api_credentials(&self) -> Result<(), StoreError> {
        self.delete_item(SecretItem::ApiCredentials).await
    }

    /// Reads the logout-pending marker. `Ok(None)`: no logout is pending;
    /// `Err`: the store could not be read, which startup treats as pending.
    pub async fn load_logout_marker(&self) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        self.load_item(SecretItem::LogoutPending).await
    }

    /// Records (or updates) the logout-pending marker.
    pub async fn store_logout_marker(&self, blob: Zeroizing<Vec<u8>>) -> Result<(), StoreError> {
        self.store_item(SecretItem::LogoutPending, blob).await
    }

    /// Clears the logout-pending marker; succeeds when none exists.
    pub async fn delete_logout_marker(&self) -> Result<(), StoreError> {
        self.delete_item(SecretItem::LogoutPending).await
    }

    async fn load_item(&self, item: SecretItem) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        self.ask(|reply| PersistMsg::LoadItem { item, reply })
            .await
            .unwrap_or(Err(StoreError::Stopped))
    }

    async fn store_item(
        &self,
        item: SecretItem,
        blob: Zeroizing<Vec<u8>>,
    ) -> Result<(), StoreError> {
        self.ask(|reply| PersistMsg::StoreItem { item, blob, reply })
            .await
            .unwrap_or(Err(StoreError::Stopped))
    }

    async fn delete_item(&self, item: SecretItem) -> Result<(), StoreError> {
        self.ask(|reply| PersistMsg::DeleteItem { item, reply })
            .await
            .unwrap_or(Err(StoreError::Stopped))
    }

    pub(crate) fn sender(&self) -> mpsc::Sender<PersistMsg> {
        self.tx.clone()
    }
}

/// Write bookkeeping for the newest generation seen.
struct GenState {
    account: AccountGen,
    /// Highest revision applied to the store (0: the loaded or empty state).
    written: u64,
    /// Outcome of that write.
    outcome: Result<(), PersistError>,
}

struct Worker {
    store: Box<dyn SecretStore>,
    /// Every generation up to and including this one is terminated.
    terminated: Option<AccountGen>,
    current: Option<GenState>,
}

impl Worker {
    fn run(mut self, mut rx: mpsc::Receiver<PersistMsg>) {
        while let Some(msg) = rx.blocking_recv() {
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: PersistMsg) {
        match msg {
            PersistMsg::Write { account, rev, blob } => self.write(account, rev, &blob),
            PersistMsg::Barrier {
                account,
                rev,
                reply,
            } => drop(reply.send(self.barrier(account, rev))),
            PersistMsg::Terminate { account } => self.terminate(account),
            PersistMsg::Delete { account, reply } => {
                self.terminate(account);
                let result = self
                    .store
                    .delete(SecretItem::Session)
                    .map_err(PersistError::Store);
                drop(reply.send(result));
            }
            PersistMsg::LoadSession { reply } => {
                drop(reply.send(self.store.get(SecretItem::Session)));
            }
            PersistMsg::CacheKey { mode, reply } => {
                let result = match mode {
                    KeyMode::LoadOrCreate => load_or_create_cache_key(&*self.store),
                    KeyMode::Replace => create_cache_key(&*self.store),
                };
                drop(reply.send(result));
            }
            PersistMsg::DeleteCacheKey { reply } => {
                drop(reply.send(delete_cache_key(&*self.store)));
            }
            PersistMsg::ForgetCacheKey { reply } => {
                drop(reply.send(delete_cache_key(&*self.store)));
            }
            PersistMsg::LoadItem { item, reply } => {
                drop(reply.send(self.store.get(item)));
            }
            PersistMsg::StoreItem { item, blob, reply } => {
                drop(reply.send(self.store.set(item, &blob)));
            }
            PersistMsg::DeleteItem { item, reply } => {
                drop(reply.send(self.store.delete(item)));
            }
        }
    }

    fn is_terminated(&self, account: AccountGen) -> bool {
        self.terminated.is_some_and(|t| account <= t)
    }

    fn terminate(&mut self, account: AccountGen) {
        self.terminated = Some(self.terminated.map_or(account, |t| t.max(account)));
    }

    /// The bookkeeping for `account`, starting it if it is newer than the current
    /// one. `None` for an older generation.
    fn gen_state(&mut self, account: AccountGen) -> Option<&mut GenState> {
        let current = self.current.get_or_insert(GenState {
            account,
            written: 0,
            outcome: Ok(()),
        });
        if current.account < account {
            *current = GenState {
                account,
                written: 0,
                outcome: Ok(()),
            };
        }
        (current.account == account).then_some(current)
    }

    fn write(&mut self, account: AccountGen, rev: u64, blob: &[u8]) {
        if self.is_terminated(account) {
            return;
        }
        let Some(state) = self.gen_state(account) else {
            return;
        };
        if rev <= state.written {
            return;
        }
        state.written = rev;
        let result = self.store.set(SecretItem::Session, blob);
        if let Some(state) = self.current.as_mut() {
            state.outcome = result.map_err(PersistError::Store);
        }
    }

    /// The outcome of the newest write covering `rev`: every write is a full
    /// state, so a later write covers earlier revisions.
    fn barrier(&mut self, account: AccountGen, rev: u64) -> Result<(), PersistError> {
        if self.is_terminated(account) {
            return Err(PersistError::Terminated);
        }
        let Some(state) = self.gen_state(account) else {
            return Err(PersistError::Terminated);
        };
        if rev <= state.written {
            state.outcome.clone()
        } else {
            // Messages are ordered and a Barrier is sent after the Write for
            // its revision, so that Write was dropped or never queued.
            Err(PersistError::NotQueued)
        }
    }
}
