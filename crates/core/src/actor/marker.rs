//! The durable logout-pending marker (review rounds 15, A, and 19).
//!
//! A logout records the marker (`SecretItem::LogoutPending`) before any of
//! its side effects and clears it only after both the session item and the
//! cache are gone. A marker that cannot be written stops the logout before
//! it starts: nothing is terminated, ended or deleted, and the account stays
//! as it was ("Couldn't start logout", Retry). Startup reads it before the
//! session or the cache:
//! - **Present:** a logout was interrupted (a crash, a failed deletion, a
//!   shutdown). The session is not loaded, the cache stays quarantined, no
//!   client is built, and the local deletion runs again. A failure stays
//!   "Logout incomplete" with Retry, across restarts, until it succeeds.
//! - **Unreadable:** unknown, which is not pending. Startup is blocked
//!   (nothing loaded, served, built or deleted); Retry reads the marker
//!   again and goes on as if it had been read at startup.
//! - **Absent:** a normal startup.
//!
//! The marker's one byte is the remote result so far, so a resumed logout
//! reports what the first attempt saw. An unknown result is reported as a
//! failure, so the UI points at Active Sessions. Rewrites of that byte are
//! best effort and never stop a logout: the Keychain updates an item in
//! place (`secret::keychain`), so a failed rewrite leaves the earlier
//! marker, and every path into the local deletion starts from a marker that
//! was written (`LogoutGate`) or read (startup, `retry_startup`).

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::Actor;
use super::auth::AuthMsg;
use super::lifecycle::{LogoutStage, Phase, Step};
use super::logout::RemotePlan;
use crate::api::{AuthState, LogoutTrigger, RemoteLogout, StorageError, UserError};
use crate::backend::Backend;
use crate::secret::StoreError;
use crate::session::PersistHandle;

/// What startup found in the marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingLogout {
    /// A logout to finish; the remote result it reached.
    Resume(RemoteLogout),
    /// The marker could not be read: whether a logout is pending is
    /// unknown.
    Unreadable(StorageError),
}

/// Reads the marker at startup, before the session item.
pub(crate) async fn read_pending_logout(persist: &PersistHandle) -> Option<PendingLogout> {
    match persist.load_logout_marker().await {
        Ok(None) => None,
        Ok(Some(blob)) => Some(PendingLogout::Resume(decode(&blob))),
        Err(e) => {
            tracing::warn!("logout marker unreadable; startup blocked");
            Some(PendingLogout::Unreadable(StorageError::from(&e)))
        }
    }
}

/// The marker's byte for a remote result (`None`: not known yet).
fn encode(remote: Option<RemoteLogout>) -> Zeroizing<Vec<u8>> {
    let byte = match remote {
        None => 0,
        Some(RemoteLogout::Revoked) => 1,
        Some(RemoteLogout::Failed(_)) => 2,
        Some(RemoteLogout::TimedOut) => 3,
        Some(RemoteLogout::NotAttempted) => 4,
    };
    Zeroizing::new(vec![byte])
}

fn decode(blob: &[u8]) -> RemoteLogout {
    match blob.first() {
        Some(1) => RemoteLogout::Revoked,
        Some(3) => RemoteLogout::TimedOut,
        Some(4) => RemoteLogout::NotAttempted,
        // Failed, or not known: the session may still be live on the server.
        _ => RemoteLogout::Failed(UserError::Network),
    }
}

/// Records (or refreshes) the marker; runs ahead of a logout's side effects
/// on the persistence worker's ordered queue.
pub(super) async fn mark_logout_pending(
    persist: &PersistHandle,
    remote: Option<RemoteLogout>,
) -> Result<(), StoreError> {
    let res = persist.store_logout_marker(encode(remote)).await;
    if res.is_err() {
        tracing::warn!("logout marker not written");
    }
    res
}

/// Writes the marker unless shutdown starts first (`None`).
pub(super) async fn mark_until(
    persist: &PersistHandle,
    remote: Option<RemoteLogout>,
    shutdown: &CancellationToken,
) -> Option<Result<(), StoreError>> {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => None,
        res = mark_logout_pending(persist, remote) => Some(res),
    }
}

/// A logout asked for whose marker is not durable yet.
#[derive(Clone, Debug)]
pub(super) struct Marking {
    pub(super) plan: RemotePlan,
    pub(super) trigger: LogoutTrigger,
    /// The state "Stay signed in" goes back to.
    pub(super) resume: AuthState,
}

impl Marking {
    /// The server ending the session outranks the user's request: there is
    /// no session left to end remotely, and no staying signed in.
    fn escalate(&mut self, plan: RemotePlan, trigger: LogoutTrigger) {
        if plan == RemotePlan::ServerRevoked {
            self.plan = plan;
        }
        if trigger == LogoutTrigger::Server {
            self.trigger = trigger;
        }
    }
}

/// The marker as the precondition of every logout side effect (review
/// round 19): a logout is `marking` while its marker is written, and
/// `blocked` once the write failed. Neither has changed anything yet.
/// Login states reported meanwhile (the startup check's `SignedIn`, say)
/// become the state "Stay signed in" goes back to (`hold`).
#[derive(Debug, Default)]
pub(super) struct LogoutGate {
    marking: Option<Marking>,
    blocked: Option<(Marking, StorageError)>,
}

impl LogoutGate {
    /// Folds a new request into a logout that waits on its marker; `false`
    /// if there is none.
    pub(super) fn escalate(&mut self, plan: RemotePlan, trigger: LogoutTrigger) -> bool {
        let blocked = self.blocked.as_mut().map(|(m, _)| m);
        match self.marking.as_mut().or(blocked) {
            Some(m) => {
                m.escalate(plan, trigger);
                true
            }
            None => false,
        }
    }

    pub(super) fn is_marking(&self) -> bool {
        self.marking.is_some()
    }

    pub(super) fn take_blocked(&mut self) -> Option<(Marking, StorageError)> {
        self.blocked.take()
    }

    pub(super) fn put_blocked(&mut self, marking: Marking, error: StorageError) {
        self.blocked = Some((marking, error));
    }

    /// "Stay signed in": the latest state of a user's logout that could not
    /// start. `None` for the server's (the account cannot continue).
    pub(super) fn stay_signed_in(&mut self) -> Option<AuthState> {
        if self.blocked.as_ref()?.0.trigger != LogoutTrigger::User {
            return None;
        }
        self.blocked.take().map(|(m, _)| m.resume)
    }

    /// The state to show instead of `state`. While a logout waits on its
    /// marker, `state` becomes what "Stay signed in" goes back to; once the
    /// write failed, "Couldn't start logout" stays up until Retry or Back.
    pub(super) fn hold(&mut self, state: AuthState) -> AuthState {
        if matches!(state, AuthState::LogoutNotStarted { .. }) {
            return state;
        }
        if let Some(m) = self.marking.as_mut() {
            m.resume = state.clone();
            return state;
        }
        match self.blocked.as_mut() {
            Some((m, error)) => {
                m.resume = state;
                AuthState::LogoutNotStarted {
                    error: *error,
                    trigger: m.trigger,
                }
            }
            None => state,
        }
    }
}

impl<B: Backend> Actor<B> {
    /// Writes the marker in the background (the Keychain may prompt); the
    /// logout starts only when the write succeeded (`on_logout_marked`).
    pub(super) fn begin_marking(&mut self, marking: Marking) {
        self.logout_gate.marking = Some(marking);
        let persist = self.persist.clone();
        let tx = self.auth.sender();
        let shutdown = self.cancel.clone();
        tokio::spawn(async move {
            // Shutdown first: nothing was started, and a marker that did
            // land resumes the logout at the next startup.
            if let Some(res) = mark_until(&persist, None, &shutdown).await {
                let res = res.map_err(|e| StorageError::from(&e));
                // A closed channel means the actor is gone.
                let _ = tx.send(AuthMsg::LogoutMarked(res)).await;
            }
        });
    }

    /// The marker write finished. Durable: the teardown starts. Failed:
    /// nothing was done, so the account stays as it was ("Couldn't start
    /// logout", Retry).
    pub(super) async fn on_logout_marked(&mut self, res: Result<(), StorageError>) {
        let Some(marking) = self.logout_gate.marking.take() else {
            return;
        };
        match res {
            Ok(()) => self.tear_down(marking.plan).await,
            Err(error) => {
                tracing::warn!("logout not started: its marker was not written");
                let trigger = marking.trigger;
                self.logout_gate.put_blocked(marking, error);
                self.set_auth(AuthState::LogoutNotStarted { error, trigger })
                    .await;
            }
        }
    }

    /// Startup with the marker present or unreadable: no session was
    /// loaded and no client is built.
    pub(super) async fn resume_logout(&mut self, pending: PendingLogout) {
        match pending {
            PendingLogout::Resume(remote) => {
                tracing::info!("resuming an interrupted logout");
                self.set_auth(AuthState::LoggingOut).await;
                self.note_remote_logout(remote);
                self.life.phase = Phase::LoggingOut(LogoutStage::Local);
                let local = self.local_step();
                *self.life.step_mut() =
                    Some(tokio::spawn(async move { Step::Deleted(local.await) }));
            }
            PendingLogout::Unreadable(error) => {
                // Unknown is not pending: nothing is deleted on it.
                self.auth.reset_signed_out();
                self.life.phase = Phase::LoggingOut(LogoutStage::Unknown);
                self.set_auth(AuthState::StartupBlocked { error }).await;
            }
        }
    }

    /// `Command::RetryStartup` (or a Logout) while startup is blocked: reads
    /// the marker again. Absent: the normal startup. Present: the logout's
    /// local deletion. Unreadable: still blocked. Read inline: in this state
    /// the actor serves nothing else, and shutdown interrupts the read.
    pub(super) async fn retry_startup(&mut self) {
        if self.life.phase != Phase::LoggingOut(LogoutStage::Unknown) {
            return self.emit_auth().await;
        }
        let read = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return,
            read = read_pending_logout(&self.persist) => read,
        };
        match read {
            None => self.resume_startup().await,
            Some(pending) => self.resume_logout(pending).await,
        }
    }

    /// The marker is absent after all: start as `core::prepare` would have,
    /// loading the session and binding the cache to it.
    async fn resume_startup(&mut self) {
        tracing::info!("logout marker absent on retry; starting");
        let (account, identity) = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return,
            loaded = crate::core::load_account(&self.persist, &self.events) => loaded,
        };
        self.account = account;
        self.backend.use_account(self.account.clone());
        self.forget_instance_session();
        self.auth.signed_in = self.account.is_signed_in().unwrap_or(false);
        self.reprovision_cache(identity).await;
        self.set_auth(AuthState::Connecting).await;
        self.load_cached_chats();
        self.resolve_credentials().await;
        self.start_build(Duration::ZERO);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips_the_remote_result() {
        for remote in [
            RemoteLogout::Revoked,
            RemoteLogout::TimedOut,
            RemoteLogout::NotAttempted,
        ] {
            assert_eq!(decode(&encode(Some(remote))), remote);
        }
        let unknown = RemoteLogout::Failed(UserError::Network);
        assert_eq!(decode(&encode(None)), unknown);
        assert_eq!(decode(&[]), unknown);
    }
}
