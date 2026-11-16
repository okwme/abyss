//! What a client instance is built from (plan section 2, "Build" step 1).
//!
//! The session part is the account-level [`SessionSnapshot`] (task 5): core
//! only carries it from a retired instance to the next one and never inspects
//! it. A snapshot is a live handle on the critical cell, not a value, so seeds
//! are not comparable; tests use [`SessionSeed::is_stored`] and the snapshot's
//! accessors instead.

pub use crate::session::SessionSnapshot;

use crate::model::ChatId;

/// Maximum peers carried across a rebuild: the open chat windows (at most 8)
/// plus the chats of pending sends (at most 256).
pub const MAX_CARRIED_PEERS: usize = 8 + 256;

/// Where the next instance's session comes from.
#[derive(Clone, Debug)]
pub enum SessionSeed {
    /// Process startup: load the persisted critical state (the Keychain).
    Stored,
    /// In-process rebuild: the live snapshot of the retired instance. Persistent
    /// storage is never re-read during a rebuild.
    Live(SessionSnapshot),
}

impl SessionSeed {
    pub fn is_stored(&self) -> bool {
        matches!(self, SessionSeed::Stored)
    }
}

/// Chats whose peers (access hashes) the next instance must know without a
/// dialogs reload: open windows and unresolved sends. Bounded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CarriedPeers(Vec<ChatId>);

impl CarriedPeers {
    /// Keeps the first [`MAX_CARRIED_PEERS`] distinct chats, in order.
    pub fn new(chats: impl IntoIterator<Item = ChatId>) -> Self {
        let mut out: Vec<ChatId> = Vec::with_capacity(MAX_CARRIED_PEERS);
        for chat in chats {
            if out.len() >= MAX_CARRIED_PEERS {
                break;
            }
            if !out.contains(&chat) {
                out.push(chat);
            }
        }
        Self(out)
    }

    pub fn as_slice(&self) -> &[ChatId] {
        &self.0
    }
}

/// Everything a build needs from core.
#[derive(Clone, Debug)]
pub struct Seed {
    pub session: SessionSeed,
    pub peers: CarriedPeers,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PeerKind;

    #[test]
    fn carried_peers_are_distinct_and_bounded() {
        let chat = |id| ChatId {
            kind: PeerKind::User,
            id,
        };
        let many = (0..1000).map(|i| chat(i % 300));
        let peers = CarriedPeers::new(many);
        assert_eq!(peers.as_slice().len(), MAX_CARRIED_PEERS);
        assert_eq!(peers.as_slice().first(), Some(&chat(0)));
    }
}
