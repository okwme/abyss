//! What a client instance is built from (plan section 2, "Build" step 1).
//!
//! The session part is the account-level [`SessionSnapshot`] (task 5): core
//! only carries it from a retired instance to the next one and never inspects
//! it. A snapshot is a live handle on the critical cell, not a value, so seeds
//! are not comparable; tests use [`SessionSeed::is_stored`] and the snapshot's
//! accessors instead.

pub use crate::session::SessionSnapshot;

/// Chats core may address at once outside the dialog list: the open chat
/// windows (at most 8) plus the chats of pending sends (at most 256).
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

/// Everything a build needs from core.
///
/// The peers the next instance must know come with the session: the retired
/// instance's pinned set (core's last `Client::retain_peers` plus calls in
/// progress) is carried in the snapshot.
#[derive(Clone, Debug)]
pub struct Seed {
    pub session: SessionSeed,
}
