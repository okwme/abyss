//! The durable logout-pending marker (review round 15, A).
//!
//! A logout records the marker (`SecretItem::LogoutPending`) before any of
//! its side effects and clears it only after both the session item and the
//! cache are gone. Startup reads it before the session or the cache:
//! - **Present:** a logout was interrupted (a crash, a failed deletion, a
//!   shutdown). The session is not loaded, the cache stays quarantined, no
//!   client is built, and the local deletion runs again. A failure stays
//!   "Logout incomplete" with Retry, across restarts, until it succeeds.
//! - **Unreadable:** fail closed. Startup is blocked in "Logout incomplete"
//!   (nothing loaded, served or built); Retry runs the local deletion.
//! - **Absent:** a normal startup.
//!
//! The marker's one byte is the remote result so far, so a resumed logout
//! reports what the first attempt saw. An unknown result is reported as a
//! failure, so the UI points at Active Sessions.

use zeroize::Zeroizing;

use super::Actor;
use super::lifecycle::{LogoutStage, Phase, Step};
use crate::api::{AuthState, LocalDeletion, RemoteLogout, StorageError, UserError};
use crate::backend::Backend;
use crate::secret::StoreError;
use crate::session::PersistHandle;

/// What startup found in the marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingLogout {
    /// A logout to finish; the remote result it reached.
    Resume(RemoteLogout),
    /// The marker could not be read.
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

impl<B: Backend> Actor<B> {
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
            PendingLogout::Unreadable(e) => {
                let local = LocalDeletion::Failed(e);
                self.logout_unfinished(RemoteLogout::NotAttempted, local)
                    .await;
            }
        }
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
