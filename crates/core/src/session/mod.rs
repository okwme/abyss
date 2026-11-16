//! Session and persistence (plan section 3).
//!
//! - [`AbyssSession`] implements grammers' `Session`. Only critical state
//!   (home DC, DC options with auth keys, the self user) is persisted; peers and
//!   update state live in memory and die with the client instance.
//! - The critical state lives in an account-level cell shared through
//!   [`SessionSnapshot`], which outlives client instances. Each `AbyssSession`
//!   holds a write token; creating the next instance revokes the previous one's.
//! - [`PersistHandle`] is the persistence worker: the only owner of the
//!   [`SecretStore`](crate::secret::SecretStore), ordered and
//!   generation-tagged.

mod cell;
mod critical;
mod peers;
#[cfg(test)]
mod tests;
mod worker;

use std::sync::{Mutex, MutexGuard, PoisonError};

use grammers_session::types::{
    ChannelState, DcOption, PeerId, PeerInfo, PeerRef, UpdateState, UpdatesState,
};
use grammers_session::{BoxFuture, Session};
use tokio::sync::watch;

pub use cell::SessionSnapshot;
pub use peers::{MAX_PINNED_PEERS, MAX_UNPINNED_PEERS};
pub use worker::{
    AccountGen, LoadStatus, LoadedSession, PERSIST_QUEUE, PersistError, PersistHandle,
};

use cell::Cell;
use critical::Refused;
use peers::PeerMap;

/// An encoded critical state of a signed-in account (self user `self_id`, no
/// auth keys), for seeding a store in tests and fake runs.
#[cfg(any(test, feature = "fake"))]
pub fn signed_in_blob(self_id: i64) -> zeroize::Zeroizing<Vec<u8>> {
    let mut state = critical::CriticalState::default();
    state.self_user = Some(critical::SelfUser {
        id: self_id,
        access_hash: 1,
        bot: false,
    });
    critical::encode(&state).unwrap_or_default()
}

/// Error returned to grammers from session calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// A panic poisoned the critical cell.
    Poisoned,
    /// More DC options than `MAX_DC_OPTIONS`.
    TooManyDcs,
    /// The critical state did not fit its encoding bound.
    Encode,
}

impl From<Refused> for SessionError {
    fn from(r: Refused) -> Self {
        match r {
            Refused::TooManyDcs => Self::TooManyDcs,
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Poisoned => f.write_str("session state poisoned"),
            Self::TooManyDcs => f.write_str("too many datacenter options"),
            Self::Encode => f.write_str("session state exceeds its size bound"),
        }
    }
}

impl std::error::Error for SessionError {}

/// grammers `Session` for one client instance (plan sections 2 and 3).
pub struct AbyssSession {
    cell: Cell,
    writer: u64,
    peers: Mutex<PeerMap>,
    updates: Mutex<UpdatesState>,
    baseline: watch::Sender<bool>,
}

impl std::fmt::Debug for AbyssSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbyssSession")
            .field("writer", &self.writer)
            .finish_non_exhaustive()
    }
}

impl AbyssSession {
    /// A fresh instance over the snapshot's critical cell. It takes the write
    /// token (revoking any previous instance's), pins the carried peers, and
    /// starts with empty update state.
    pub fn from_snapshot(snapshot: SessionSnapshot) -> Result<Self, SessionError> {
        let writer = snapshot.cell.take_writer()?;
        let mut peers = PeerMap::default();
        peers.pin(snapshot.carried.iter().cloned());
        Ok(Self {
            cell: snapshot.cell,
            writer,
            peers: Mutex::new(peers),
            updates: Mutex::new(UpdatesState::default()),
            baseline: watch::channel(false).0,
        })
    }

    /// The live critical state plus the pinned peers, for the next instance.
    /// Does not revoke this instance; [`Self::from_snapshot`] or
    /// [`Self::revoke`] does.
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            cell: self.cell.clone(),
            carried: self.peers().pinned_infos().into(),
        }
    }

    /// Revokes this instance's write token: later critical mutations through
    /// it are dropped (plan section 2, rebuild step 4).
    pub fn revoke(&self) -> Result<(), SessionError> {
        self.cell.revoke(self.writer)
    }

    /// See [`SessionSnapshot::barrier`].
    pub async fn barrier(&self) -> Result<(), PersistError> {
        self.cell.barrier().await
    }

    /// Pins peers of loaded dialogs (or carried peers) in addition to the
    /// current pins. Returns the number of pinned peers.
    pub fn pin_peers(&self, infos: impl IntoIterator<Item = PeerInfo>) -> usize {
        self.peers().pin(infos)
    }

    /// Replaces the pinned set, for example after `DialogsReset`.
    pub fn replace_pinned_peers(&self, infos: impl IntoIterator<Item = PeerInfo>) -> usize {
        self.peers().replace_pins(infos)
    }

    /// Becomes `true` on the first `UpdateState::All` for this instance: the
    /// update baseline (plan section 2, bootstrap step 2).
    pub fn baseline_established(&self) -> watch::Receiver<bool> {
        self.baseline.subscribe()
    }

    /// Converts a carried `PeerRef` into peer info suitable for
    /// [`Self::pin_peers`].
    pub fn info_from_ref(peer: PeerRef) -> Option<PeerInfo> {
        let id = peer.id.bare_id()?;
        Some(match peer.id.kind() {
            grammers_session::types::PeerKind::User => PeerInfo::User {
                id,
                auth: Some(peer.auth),
                bot: None,
                is_self: None,
            },
            grammers_session::types::PeerKind::Chat => PeerInfo::Chat { id },
            grammers_session::types::PeerKind::Channel => PeerInfo::Channel {
                id,
                auth: Some(peer.auth),
                kind: None,
            },
        })
    }

    /// Peer and update maps hold no invariant a panic could break halfway, so
    /// a poisoned lock is recovered.
    fn peers(&self) -> MutexGuard<'_, PeerMap> {
        self.peers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn updates(&self) -> MutexGuard<'_, UpdatesState> {
        self.updates.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn mutate(
        &self,
        change: impl FnOnce(&mut critical::CriticalState) -> Result<(), Refused>,
    ) -> Result<(), SessionError> {
        self.cell.mutate(self.writer, change)
    }

    fn lookup(&self, peer: PeerId) -> Result<Option<PeerInfo>, SessionError> {
        // The self-user sentinel and the self user's real id both resolve to
        // the critical self user, which is never evicted.
        let self_info = self.cell.read(critical::CriticalState::self_info)?;
        if let Some(info) = self_info
            && (peer == PeerId::self_user() || info.id() == peer)
        {
            return Ok(Some(info));
        }
        if peer == PeerId::self_user() {
            return Ok(None);
        }
        Ok(self.peers().get(peer))
    }

    fn store_peer(&self, info: PeerInfo) -> Result<(), SessionError> {
        if self.cell.read(|s| s.is_self(&info))? {
            return self.mutate(|s| {
                s.merge_self(&info);
                Ok(())
            });
        }
        self.peers().insert(info);
        Ok(())
    }

    fn apply_update_state(&self, update: UpdateState) {
        let mut state = self.updates();
        match update {
            UpdateState::All(all) => {
                *state = all;
                self.baseline.send_replace(true);
            }
            UpdateState::Primary { pts, date, seq } => {
                state.pts = pts;
                state.date = date;
                state.seq = seq;
            }
            UpdateState::Secondary { qts } => state.qts = qts,
            UpdateState::Channel { id, pts } => {
                if let Some(channel) = state.channels.iter_mut().find(|c| c.id == id) {
                    channel.pts = pts;
                } else if state.channels.len() < MAX_UNPINNED_PEERS + MAX_PINNED_PEERS {
                    state.channels.push(ChannelState { id, pts });
                }
            }
        }
    }
}

impl Session for AbyssSession {
    type Error = SessionError;

    fn home_dc_id(&self) -> Result<i32, SessionError> {
        self.cell.read(|s| s.home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), SessionError>> {
        let result = self.mutate(|s| {
            s.home_dc = dc_id;
            Ok(())
        });
        Box::pin(async move { result })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, SessionError> {
        self.cell.read(|s| s.dc_option(dc_id))
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), SessionError>> {
        let result = self.mutate(|s| s.set_dc_option(dc_option));
        Box::pin(async move { result })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, SessionError>> {
        let result = self.lookup(peer);
        Box::pin(async move { result })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, Result<(), SessionError>> {
        let result = self.store_peer(peer);
        Box::pin(async move { result })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, SessionError>> {
        let state = self.updates().clone();
        Box::pin(async move { Ok(state) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), SessionError>> {
        self.apply_update_state(update);
        Box::pin(async { Ok(()) })
    }
}
