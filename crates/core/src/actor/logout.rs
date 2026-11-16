//! Logout (plan sections 5 and 16) and cache provisioning.
//!
//! Two steps, run like the other lifecycle steps so the actor keeps serving:
//! 1. **Remote:** `Terminate{gen}` (no write for this account is applied any
//!    more), `auth.logOut` with the runner still alive (10 s), then the
//!    instance is retired (quit, join, revoke).
//! 2. **Local:** delete the session item (ordered after every queued write),
//!    then crypto-erase the cache: close it, delete its Keychain key, and only
//!    then unlink the files. A failed key deletion keeps the files.
//!
//! Then the account gets a fresh generation and a signed-out instance is
//! built for the next login. `AUTH_KEY_UNREGISTERED`/`SESSION_REVOKED` run the
//! same steps without the remote call.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Actor;
use super::apply::user_error;
use super::binding::{Rebinding, finish_rebinding};
use super::jobs::CallError;
use super::lifecycle::{LogoutStage, Phase, REBUILD_CAPS, Retired, Step, retire};
use crate::api::{
    AuthState, Event, LocalDeletion, LogoutStatus, RemoteLogout, StorageError, UserError,
};
use crate::backend::{Backend, BackendError, Client};
use crate::cache::{self, CacheError, CacheHandle, CacheKey};
use crate::secret::CacheKeyError;
use crate::session::{AccountGen, PersistHandle};

/// Deadline for `auth.logOut`.
pub(super) const SIGN_OUT_DEADLINE: Duration = Duration::from_secs(10);

/// Where the account's message cache lives, its key while it is open, and
/// the cache while it is quarantined (open, never read; see `binding`).
pub(crate) struct CacheParts {
    pub(crate) dir: PathBuf,
    pub(crate) key: Option<CacheKey>,
    /// Open but not bound to the signed-in account.
    pub(crate) quarantined: Option<CacheHandle>,
    /// A background erase-and-recreate for a login (see `binding`).
    pub(crate) rebinding: Option<Rebinding>,
}

impl CacheParts {
    /// No cache open yet.
    pub(crate) fn closed(dir: PathBuf) -> Self {
        Self {
            dir,
            key: None,
            quarantined: None,
            rebinding: None,
        }
    }
}

impl std::fmt::Debug for CacheParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheParts")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// Whether the logout ends the session on the server first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemotePlan {
    /// Signed in: call `auth.logOut`.
    Attempt,
    /// The server already revoked the session.
    ServerRevoked,
    /// Not signed in: retry the local deletion only.
    Skip,
}

/// The local step's result, plus the next account's cache if one was opened.
pub(super) struct LocalDone {
    pub(super) local: LocalDeletion,
    cache: Option<(CacheHandle, CacheKey)>,
}

impl LocalDone {
    pub(super) fn failed(e: StorageError) -> Self {
        Self {
            local: LocalDeletion::Failed(e),
            cache: None,
        }
    }
}

async fn sign_out<C: Client>(client: Arc<C>, shutdown: &CancellationToken) -> RemoteLogout {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => RemoteLogout::Failed(UserError::ShuttingDown),
        res = tokio::time::timeout(SIGN_OUT_DEADLINE, client.sign_out()) => match res {
            // Already revoked counts: the server no longer accepts the session.
            Ok(Ok(()) | Err(BackendError::Unauthorized)) => RemoteLogout::Revoked,
            Ok(Err(e)) => RemoteLogout::Failed(user_error(CallError::Backend(e))),
            Err(_) => RemoteLogout::TimedOut,
        },
    }
}

async fn delete_local(
    persist: PersistHandle,
    account: Option<AccountGen>,
    open: Option<(CacheHandle, CacheKey)>,
    parts: Option<(PathBuf, Option<Rebinding>)>,
) -> LocalDone {
    let session = match account {
        Some(account) => persist
            .delete_session(account)
            .await
            .map_err(|e| StorageError::from(&e)),
        None => Err(StorageError::Internal),
    };
    let (erased, reopened) = match parts {
        Some((dir, rebinding)) => {
            // A login's re-binding owns the cache until it finishes.
            let open = match finish_rebinding(rebinding).await {
                Some(rebound) => Some(rebound),
                None => open,
            };
            let (cache, key) = open.map_or((None, None), |(c, k)| (Some(c), Some(k)));
            erase_cache(&persist, cache, dir, key).await
        }
        None => (Ok(()), None),
    };
    let local = match (session, erased) {
        (Ok(()), Ok(())) => LocalDeletion::Deleted,
        (Err(e), _) | (_, Err(e)) => LocalDeletion::Failed(e),
    };
    LocalDone {
        local,
        cache: reopened,
    }
}

/// Crypto-erases the cache, then opens a fresh one for the next account.
pub(super) async fn erase_cache(
    persist: &PersistHandle,
    cache: Option<CacheHandle>,
    dir: PathBuf,
    key: Option<CacheKey>,
) -> (Result<(), StorageError>, Option<(CacheHandle, CacheKey)>) {
    let erased = match (cache, key) {
        (Some(handle), Some(key)) => handle
            .erase(key, persist.cache_key_deleter())
            .await
            .map_err(|e| StorageError::from(&e)),
        // Not open (an earlier erase or open failed): key first, then files.
        _ => forget_cache(persist, &dir).await,
    };
    if erased.is_err() {
        return (erased, None);
    }
    (Ok(()), open_cache(persist, dir).await)
}

async fn forget_cache(persist: &PersistHandle, dir: &Path) -> Result<(), StorageError> {
    persist
        .delete_cache_key()
        .await
        .map_err(|e| StorageError::from(&e))?;
    let dir = dir.to_path_buf();
    match tokio::task::spawn_blocking(move || cache::destroy_unopened(&dir)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(StorageError::CacheFiles),
    }
}

/// Provisions the cache key and opens the cache (plan section 16; task 5
/// wiring step 2). A malformed key destroys the unopened cache and replaces
/// the key; a cache the key cannot open (or of an unknown schema) is destroyed
/// and recreated. A Keychain failure runs without a cache and destroys
/// nothing. `None` means "no cache this run".
pub(crate) async fn open_cache(
    persist: &PersistHandle,
    dir: PathBuf,
) -> Option<(CacheHandle, CacheKey)> {
    let key = match persist.load_or_create_cache_key().await {
        Ok(key) => key,
        Err(CacheKeyError::Malformed) => {
            let d = dir.clone();
            tokio::task::spawn_blocking(move || cache::destroy_unopened(&d))
                .await
                .ok()?
                .ok()?;
            persist.create_cache_key().await.ok()?
        }
        // A Keychain failure: the key may still exist, so destroy nothing.
        Err(CacheKeyError::Store(_) | CacheKeyError::Random) => return None,
    };
    tokio::task::spawn_blocking(move || {
        let handle = match CacheHandle::spawn(dir.clone(), &key) {
            Err(CacheError::WrongKey | CacheError::UnknownSchema(_)) => {
                cache::destroy_unopened(&dir).ok()?;
                CacheHandle::spawn(dir, &key).ok()?
            }
            other => other.ok()?,
        };
        Some((handle, key))
    })
    .await
    .ok()
    .flatten()
}

impl<B: Backend> Actor<B> {
    pub(super) async fn on_logout_command(&mut self) {
        if self.life.phase == Phase::LoggingOut(LogoutStage::Unfinished) {
            let remote = self
                .unfinished_logout
                .map_or(RemoteLogout::NotAttempted, |(remote, _)| remote);
            return self.retry_logout(remote).await;
        }
        let plan = if self.auth.signed_in {
            RemotePlan::Attempt
        } else {
            RemotePlan::Skip
        };
        self.start_logout(plan).await;
    }

    /// A call answered `AUTH_KEY_UNREGISTERED`/`SESSION_REVOKED`: the key is
    /// dead, so the local session is torn down whether or not the account
    /// was signed in (a login call can see it too).
    pub(super) async fn session_revoked(&mut self) {
        if matches!(self.life.phase, Phase::LoggingOut(_)) {
            return;
        }
        self.auth.phone_error = Some(UserError::SessionRevoked);
        self.start_logout(RemotePlan::ServerRevoked).await;
    }

    async fn start_logout(&mut self, plan: RemotePlan) {
        match self.life.phase {
            Phase::LoggingOut(_) => return self.emit_auth().await,
            Phase::Retiring(_) => return self.error(UserError::Busy).await,
            // No instance yet; the build is just dropped.
            Phase::Building => {
                if let Some(step) = self.life.step_mut().take() {
                    step.abort();
                    let _ = step.await;
                }
            }
            Phase::Booting | Phase::Live | Phase::Halted | Phase::SignedOut => {}
        }
        let live = self.live.take();
        self.begin_retire();
        self.auth.abort_task();
        self.reset_account_data().await;
        self.set_auth(AuthState::LoggingOut).await;
        let account = self.account.generation().ok();
        let persist = self.persist.clone();
        let shutdown = self.cancel.clone();
        self.life.phase = Phase::LoggingOut(LogoutStage::Remote);
        *self.life.step_mut() = Some(tokio::spawn(async move {
            if let Some(account) = account {
                let _ = persist.terminate(account).await;
            }
            let remote = match (plan, &live) {
                (RemotePlan::Attempt, Some(l)) => sign_out(Arc::clone(&l.client), &shutdown).await,
                (RemotePlan::Attempt, None) => RemoteLogout::Failed(UserError::Network),
                (RemotePlan::ServerRevoked, _) => RemoteLogout::Revoked,
                (RemotePlan::Skip, _) => RemoteLogout::NotAttempted,
            };
            let retired = match live {
                Some(l) => Some(retire(l, REBUILD_CAPS, &shutdown).await),
                None => None,
            };
            Step::SignedOut { remote, retired }
        }));
    }

    /// The remote step finished: the instance is gone. Its send outcomes
    /// belong to the old account and are dropped with it.
    pub(super) fn after_remote_logout(&mut self, remote: RemoteLogout, retired: Option<Retired>) {
        self.note_remote_logout(remote);
        drop(retired);
        self.admission.release_parked();
        self.life.phase = Phase::LoggingOut(LogoutStage::Local);
        let local = self.local_step();
        *self.life.step_mut() = Some(tokio::spawn(async move { Step::Deleted(local.await) }));
    }

    pub(super) fn note_remote_logout(&mut self, remote: RemoteLogout) {
        self.logout_remote = Some(remote);
    }

    /// The local deletion, detached from the actor's state.
    fn local_step(&mut self) -> impl Future<Output = LocalDone> + Send + 'static {
        let persist = self.persist.clone();
        let account = self.account.generation().ok();
        let served = self.cache.take();
        let (open, parts) = match self.cache_parts.as_mut() {
            Some(p) => (
                p.take_open(served),
                Some((p.dir.clone(), p.rebinding.take())),
            ),
            None => (None, None),
        };
        delete_local(persist, account, open, parts)
    }

    /// Shutdown: runs the local step inline, bounded by `deadline`.
    pub(super) async fn delete_local_until(&mut self, deadline: Instant) -> Option<LocalDeletion> {
        let local = self.local_step();
        tokio::time::timeout_at(deadline, local)
            .await
            .ok()
            .map(|d| d.local)
    }

    pub(super) fn logout_status(&self, local: Option<LocalDeletion>) -> LogoutStatus {
        match local {
            Some(local) => LogoutStatus::Done {
                remote: self.logout_remote.unwrap_or(RemoteLogout::NotAttempted),
                local,
            },
            None => LogoutStatus::Incomplete,
        }
    }

    /// The local step finished: start a fresh account generation and build a
    /// signed-out instance for the next login (plan section 5, step 7). A
    /// failed deletion instead stays "Logout incomplete" (no instance, no
    /// login) until a Retry deletes everything.
    pub(super) async fn finish_logout(&mut self, done: LocalDone) {
        let remote = self
            .logout_remote
            .take()
            .unwrap_or(RemoteLogout::NotAttempted);
        // The next account's cache is quarantined until a login binds it.
        if let (Some((handle, key)), Some(parts)) = (done.cache, self.cache_parts.as_mut()) {
            parts.quarantined = Some(handle);
            parts.key = Some(key);
        }
        if let LocalDeletion::Failed(_) = done.local {
            return self.logout_unfinished(remote, done.local).await;
        }
        self.unfinished_logout = None;
        self.account = self.persist.fresh_session();
        self.backend.use_account(self.account.clone());
        self.forget_instance_session();
        self.clear_halt();
        self.auth.reset_signed_out();
        let local = done.local;
        self.set_auth(AuthState::LoggedOut { remote, local }).await;
        self.start_build(Duration::ZERO);
    }

    /// The local deletion failed: the session item or the cache key may
    /// remain. No instance is built and no login is accepted until a Retry
    /// (`Command::Logout`) deletes them; shutdown reports it.
    async fn logout_unfinished(&mut self, remote: RemoteLogout, local: LocalDeletion) {
        tracing::warn!("logout incomplete: local data remains");
        self.unfinished_logout = Some((remote, local));
        self.life.phase = Phase::LoggingOut(LogoutStage::Unfinished);
        self.auth.reset_signed_out();
        self.set_auth(AuthState::LoggedOut { remote, local }).await;
    }

    /// Retry of an incomplete logout: the local step only. The remote result
    /// of the first attempt is the one reported.
    async fn retry_logout(&mut self, remote: RemoteLogout) {
        self.set_auth(AuthState::LoggingOut).await;
        self.note_remote_logout(remote);
        self.life.phase = Phase::LoggingOut(LogoutStage::Local);
        let local = self.local_step();
        *self.life.step_mut() = Some(tokio::spawn(async move { Step::Deleted(local.await) }));
    }

    /// An incomplete logout, for the shutdown report.
    pub(super) fn unfinished_logout_status(&self) -> Option<LogoutStatus> {
        self.unfinished_logout
            .map(|(remote, local)| LogoutStatus::Done { remote, local })
    }

    /// A second migration (plan section 2): the account is torn down like a
    /// logout without the remote call (the instance is already retired), so
    /// the next login, possibly to another account, starts from a fresh
    /// generation, an erased cache and no pending sends.
    pub(super) async fn abandon_account(&mut self) {
        self.auth.phone_error = Some(UserError::AccountMigrated);
        self.start_logout(RemotePlan::Skip).await;
    }

    /// Forgets everything of the old account: chats, windows, pending sends
    /// and read marks. The UI's chat list empties.
    async fn reset_account_data(&mut self) {
        self.dialogs = crate::store::Dialogs::default();
        self.windows = crate::store::Windows::default();
        self.open = None;
        self.viewing = None;
        self.reads.clear();
        self.sends = super::sends::PendingSends::default();
        self.queue.clear();
        self.dialogs_loaded = false;
        self.dialogs_next = None;
        self.emit(Event::DialogsReset(Vec::new())).await;
    }
}
