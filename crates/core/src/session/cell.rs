//! The account-level critical cell (plan section 3) and its snapshot handle.

use std::sync::{Arc, Mutex, MutexGuard};

use grammers_session::types::PeerInfo;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use super::SessionError;
use super::critical::{CriticalState, Refused, encode};
use super::worker::{AccountGen, PersistError, PersistHandle, PersistMsg};

/// Write-token value that no session holds.
const NO_WRITER: u64 = 0;

pub(crate) struct CellInner {
    pub(crate) state: CriticalState,
    /// Bumped only when the encoded state changes.
    rev: u64,
    /// Encoding of `state` at `rev`.
    last_blob: Zeroizing<Vec<u8>>,
    account: AccountGen,
    /// The token of the one session instance allowed to mutate.
    writer: u64,
    last_writer: u64,
    /// A revision whose `Write` could not be queued, until a later one is.
    unqueued: Option<u64>,
    tx: mpsc::Sender<PersistMsg>,
}

/// Shared, lock-protected critical state. Outlives client instances.
#[derive(Clone)]
pub(crate) struct Cell(Arc<Mutex<CellInner>>);

impl Cell {
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, CellInner>, SessionError> {
        self.0.lock().map_err(|_| SessionError::Poisoned)
    }

    /// Issues a new write token, revoking the previous holder's.
    pub(crate) fn take_writer(&self) -> Result<u64, SessionError> {
        let mut inner = self.lock()?;
        inner.last_writer = inner.last_writer.saturating_add(1);
        inner.writer = inner.last_writer;
        Ok(inner.writer)
    }

    pub(crate) fn revoke(&self, writer: u64) -> Result<(), SessionError> {
        let mut inner = self.lock()?;
        if inner.writer == writer {
            inner.writer = NO_WRITER;
        }
        Ok(())
    }

    /// Applies a critical mutation for `writer`. Under the lock: apply,
    /// encode, compare with the last encoding, and only on change bump `rev`
    /// and queue a `Write`. A revoked writer's mutation is dropped.
    pub(crate) fn mutate(
        &self,
        writer: u64,
        change: impl FnOnce(&mut CriticalState) -> Result<(), Refused>,
    ) -> Result<(), SessionError> {
        let mut inner = self.lock()?;
        if writer == NO_WRITER || inner.writer != writer {
            return Ok(());
        }
        change(&mut inner.state)?;
        let blob = encode(&inner.state).map_err(|_| SessionError::Encode)?;
        if *blob == *inner.last_blob {
            return Ok(());
        }
        inner.rev = inner.rev.saturating_add(1);
        let copy = Zeroizing::new(blob.to_vec());
        inner.last_blob = blob;
        let msg = PersistMsg::Write {
            account: inner.account,
            rev: inner.rev,
            blob: copy,
        };
        // Never block under the lock (or on the runtime): a full queue is
        // recorded and reported by the next barrier instead.
        inner.unqueued = match inner.tx.try_send(msg) {
            Ok(()) => None,
            Err(_) => Some(inner.rev),
        };
        Ok(())
    }

    pub(crate) fn read<T>(&self, f: impl FnOnce(&CriticalState) -> T) -> Result<T, SessionError> {
        Ok(f(&self.lock()?.state))
    }

    pub(crate) async fn barrier(&self) -> Result<(), PersistError> {
        let (account, rev, tx) = {
            let inner = self.lock().map_err(|_| PersistError::Poisoned)?;
            if inner.unqueued.is_some() {
                return Err(PersistError::NotQueued);
            }
            (inner.account, inner.rev, inner.tx.clone())
        };
        let (reply, rx) = oneshot::channel();
        tx.send(PersistMsg::Barrier {
            account,
            rev,
            reply,
        })
        .await
        .map_err(|_| PersistError::Stopped)?;
        rx.await.map_err(|_| PersistError::Stopped)?
    }
}

/// The live critical state of an account generation plus the peers carried
/// across a rebuild (plan section 2, rebuild step 5).
///
/// Cheap to clone (two `Arc`s). Rebuilds pass it from the retired
/// `AbyssSession` ([`super::AbyssSession::snapshot`]) to the next
/// ([`super::AbyssSession::from_snapshot`]); the Keychain is never re-read.
#[derive(Clone)]
pub struct SessionSnapshot {
    pub(crate) cell: Cell,
    pub(crate) carried: Arc<[PeerInfo]>,
}

impl std::fmt::Debug for SessionSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("SessionSnapshot");
        if let Ok(inner) = self.cell.lock() {
            d.field("account", &inner.account).field("rev", &inner.rev);
        }
        d.field("carried", &self.carried.len())
            .finish_non_exhaustive()
    }
}

impl SessionSnapshot {
    pub(crate) fn new(
        state: CriticalState,
        blob: Zeroizing<Vec<u8>>,
        account: AccountGen,
        persist: &PersistHandle,
    ) -> Self {
        Self::with_sender(state, blob, account, persist.sender())
    }

    pub(crate) fn with_sender(
        state: CriticalState,
        blob: Zeroizing<Vec<u8>>,
        account: AccountGen,
        tx: mpsc::Sender<PersistMsg>,
    ) -> Self {
        let inner = CellInner {
            state,
            rev: 0,
            last_blob: blob,
            account,
            writer: NO_WRITER,
            last_writer: NO_WRITER,
            unqueued: None,
            tx,
        };
        Self {
            cell: Cell(Arc::new(Mutex::new(inner))),
            carried: Arc::from([]),
        }
    }

    /// The persistence generation, for `terminate` and `delete_session`.
    pub fn generation(&self) -> Result<AccountGen, SessionError> {
        Ok(self.cell.lock()?.account)
    }

    /// The current critical revision (0: as loaded, or nothing saved yet).
    pub fn rev(&self) -> Result<u64, SessionError> {
        Ok(self.cell.lock()?.rev)
    }

    /// Whether the critical state names a self user (a completed login).
    pub fn is_signed_in(&self) -> Result<bool, SessionError> {
        self.cell.read(|s| s.self_user.is_some())
    }

    /// Waits until the store holds the current revision and returns the
    /// storage result: `Ok` only if the newest write covering it succeeded.
    /// Resolves at once with the last outcome when nothing changed.
    pub async fn barrier(&self) -> Result<(), PersistError> {
        self.cell.barrier().await
    }
}
