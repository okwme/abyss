//! Bounded in-memory peer map (plan section 3). The self user is not stored
//! here: it lives in the critical state and is never evicted.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};

use grammers_session::types::{PeerId, PeerInfo};

use crate::store::MAX_DIALOGS;

/// Unpinned peers kept, first in first out.
pub const MAX_UNPINNED_PEERS: usize = 2_000;
/// Pinned peers: every loaded dialog plus the carried set (8 open chats and
/// 256 pending sends, plan section 2).
pub const MAX_PINNED_PEERS: usize = MAX_DIALOGS + 8 + 256;

/// The outcome of [`PeerMap::set_pins`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pinned {
    /// Peers now pinned.
    pub pinned: usize,
    /// Requested peers left unpinned: unknown, or without an access hash.
    pub unknown: usize,
}

/// Whether `info` is enough to address the peer: basic groups need no access
/// hash, users and channels do.
fn addressable(info: &PeerInfo) -> bool {
    match info {
        PeerInfo::Chat { .. } => true,
        PeerInfo::User { auth, .. } | PeerInfo::Channel { auth, .. } => auth.is_some(),
    }
}

#[derive(Default)]
pub(crate) struct PeerMap {
    infos: HashMap<PeerId, PeerInfo>,
    pinned: HashSet<PeerId>,
    /// Insertion order of unpinned peers. May hold ids that were pinned (or
    /// re-queued) since; eviction skips pinned ids and tolerates duplicates,
    /// which only makes eviction earlier, never the map larger.
    fifo: VecDeque<PeerId>,
}

impl PeerMap {
    pub(crate) fn get(&self, id: PeerId) -> Option<PeerInfo> {
        self.infos.get(&id).cloned()
    }

    /// Caches `info`, merging with what is known (`extend_info`: a `None`
    /// field, as in Telegram's "min" constructors, never clears a known hash).
    pub(crate) fn insert(&mut self, info: PeerInfo) {
        let id = info.id();
        match self.infos.entry(id) {
            Entry::Occupied(mut known) => {
                known.get_mut().extend_info(&info);
            }
            Entry::Vacant(slot) => {
                slot.insert(info);
                if !self.pinned.contains(&id) {
                    self.fifo.push_back(id);
                    self.evict();
                }
            }
        }
    }

    /// Pins and merges `infos` in addition to the current pins. Peers beyond
    /// [`MAX_PINNED_PEERS`] are cached unpinned. Returns how many are pinned.
    pub(crate) fn pin(&mut self, infos: impl IntoIterator<Item = PeerInfo>) -> usize {
        for info in infos {
            let id = info.id();
            if self.pinned.len() < MAX_PINNED_PEERS || self.pinned.contains(&id) {
                self.pinned.insert(id);
                self.infos
                    .entry(id)
                    .and_modify(|known| {
                        known.extend_info(&info);
                    })
                    .or_insert(info);
            } else {
                self.insert(info);
            }
        }
        self.pinned.len()
    }

    /// Makes the pinned set exactly the addressable peers among `ids`, in
    /// order, up to [`MAX_PINNED_PEERS`]. A peer this map does not know, or
    /// knows without the access hash it needs, is not pinned: it is counted
    /// in [`Pinned::unknown`]. Peers that lose their pin become unpinned,
    /// newest last.
    pub(crate) fn set_pins(&mut self, ids: &[PeerId]) -> Pinned {
        let old = std::mem::take(&mut self.pinned);
        let mut unknown = 0_usize;
        for &id in ids {
            if self.pinned.len() >= MAX_PINNED_PEERS {
                break;
            }
            if self.infos.get(&id).is_some_and(addressable) {
                self.pinned.insert(id);
            } else {
                unknown = unknown.saturating_add(1);
            }
        }
        for id in old {
            if !self.pinned.contains(&id) {
                self.fifo.push_back(id);
            }
        }
        self.evict();
        Pinned {
            pinned: self.pinned.len(),
            unknown,
        }
    }

    /// The pinned peer ids, in no particular order.
    pub(crate) fn pinned_ids(&self) -> Vec<PeerId> {
        self.pinned.iter().copied().collect()
    }

    /// The pinned peers, for carrying across a rebuild.
    pub(crate) fn pinned_infos(&self) -> Vec<PeerInfo> {
        self.pinned
            .iter()
            .filter_map(|id| self.infos.get(id).cloned())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.infos.len()
    }

    fn evict(&mut self) {
        while self.fifo.len() > MAX_UNPINNED_PEERS {
            let Some(old) = self.fifo.pop_front() else {
                break;
            };
            if !self.pinned.contains(&old) {
                self.infos.remove(&old);
            }
        }
    }
}
