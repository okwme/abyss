//! Bounded in-memory peer map (plan section 3). The self user is not stored
//! here: it lives in the critical state and is never evicted.
//!
//! Every known peer is in exactly one of three states:
//! - **pinned**: core's required set plus the chats of calls in progress, at
//!   most [`MAX_PINNED_PEERS`]; carried across a rebuild, never evicted;
//! - **held**: the chats of a dialogs page (and its cursor's peer) handed to
//!   core and not yet pinned by core's next `retain_peers`. A handoff holds
//!   at most [`MAX_HANDOFF_PEERS`] ids and at most [`MAX_HANDOFFS`] are held
//!   at once (the oldest is released first). Never evicted, never carried;
//!   a held id leaves its handoff when it is pinned;
//! - **queued**: everything else, at most [`MAX_UNPINNED_PEERS`], the least
//!   recently touched evicted first. Updating a queued peer touches it.
//!
//! So the map holds at most `MAX_PINNED_PEERS + MAX_HANDOFFS ×
//! MAX_HANDOFF_PEERS + MAX_UNPINNED_PEERS` peers, however large a response
//! (up to 4,096 peers) is, and a response's auxiliary peers can never evict
//! the dialogs it hands to core.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};

use grammers_session::types::{PeerId, PeerInfo};

use crate::store::MAX_DIALOGS;

/// Unpinned (queued) peers kept, least recently touched evicted first.
pub const MAX_UNPINNED_PEERS: usize = 2_000;
/// Pinned peers: every loaded dialog plus the carried set (8 open chats and
/// 256 pending sends, plan section 2).
pub const MAX_PINNED_PEERS: usize = MAX_DIALOGS + 8 + 256;
/// Ids one handoff holds: a full dialogs page (100) plus its cursor's peer.
pub const MAX_HANDOFF_PEERS: usize = 100 + 1;
/// Handoffs held at once. Core keeps one dialogs request outstanding; the
/// second covers a page that arrives before core reconciled the previous one.
pub const MAX_HANDOFFS: usize = 2;
/// The queue is compacted (stale positions dropped) beyond this length.
const MAX_FIFO: usize = 2 * MAX_UNPINNED_PEERS;

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

struct Slot {
    info: PeerInfo,
    /// The queue position (a stamp) while queued; `None` while pinned or held.
    queued: Option<u64>,
}

#[derive(Default)]
pub(crate) struct PeerMap {
    slots: HashMap<PeerId, Slot>,
    pinned: HashSet<PeerId>,
    /// Held handoffs, oldest first; each distinct, at most
    /// [`MAX_HANDOFF_PEERS`] ids.
    handoffs: VecDeque<Vec<PeerId>>,
    /// How many handoffs hold each id.
    held: HashMap<PeerId, usize>,
    /// Queue positions, oldest first. A position is live only while its
    /// slot's stamp matches; stale ones are skipped and compacted away.
    fifo: VecDeque<(PeerId, u64)>,
    /// Live positions in `fifo` (queued slots).
    queued: usize,
    stamp: u64,
}

impl PeerMap {
    pub(crate) fn get(&self, id: PeerId) -> Option<PeerInfo> {
        self.slots.get(&id).map(|slot| slot.info.clone())
    }

    /// Caches `info`, merging with what is known (`extend_info`: a `None`
    /// field, as in Telegram's "min" constructors, never clears a known hash).
    /// A queued peer moves to the back of the queue.
    pub(crate) fn insert(&mut self, info: PeerInfo) {
        let id = info.id();
        let queue = match self.slots.entry(id) {
            Entry::Occupied(mut known) => {
                known.get_mut().info.extend_info(&info);
                known.get().queued.is_some()
            }
            Entry::Vacant(slot) => {
                slot.insert(Slot { info, queued: None });
                !self.protected(id)
            }
        };
        if queue {
            self.enqueue(id);
        }
    }

    /// Caches the peers of one response under one lock. `handoff` (a dialogs
    /// page's chats and cursor peer) is held first, so none of `infos` can
    /// evict it before core pins what it keeps.
    pub(crate) fn cache_response(&mut self, handoff: &[PeerId], infos: Vec<PeerInfo>) {
        if !handoff.is_empty() {
            self.hold(handoff);
        }
        for info in infos {
            self.insert(info);
        }
    }

    /// Pins and merges `infos` in addition to the current pins. Peers beyond
    /// [`MAX_PINNED_PEERS`] are cached unpinned. Returns how many are pinned.
    pub(crate) fn pin(&mut self, infos: impl IntoIterator<Item = PeerInfo>) -> usize {
        for info in infos {
            let id = info.id();
            if self.pinned.len() < MAX_PINNED_PEERS || self.pinned.contains(&id) {
                self.pinned.insert(id);
                self.slots
                    .entry(id)
                    .and_modify(|known| {
                        known.info.extend_info(&info);
                    })
                    .or_insert(Slot { info, queued: None });
                self.dequeue(id);
            } else {
                self.insert(info);
            }
        }
        self.pinned.len()
    }

    /// Makes the pinned set exactly the addressable peers among `ids`, in
    /// order, up to [`MAX_PINNED_PEERS`]. A peer this map does not know, or
    /// knows without the access hash it needs, is not pinned: it is counted
    /// in [`Pinned::unknown`]. A pinned id leaves its handoffs. Peers that
    /// lose their pin and are not held are queued, newest last.
    pub(crate) fn set_pins(&mut self, ids: &[PeerId]) -> Pinned {
        let old = std::mem::take(&mut self.pinned);
        let mut unknown = 0_usize;
        for &id in ids {
            if self.pinned.len() >= MAX_PINNED_PEERS {
                break;
            }
            if self.slots.get(&id).is_some_and(|s| addressable(&s.info)) {
                self.pinned.insert(id);
                self.dequeue(id);
            } else {
                unknown = unknown.saturating_add(1);
            }
        }
        self.release_pinned();
        for id in old {
            if !self.protected(id) {
                self.enqueue(id);
            }
        }
        Pinned {
            pinned: self.pinned.len(),
            unknown,
        }
    }

    /// [`Self::set_pins`] after merging `info` (a peer resolved before the
    /// caller took its locks): an eviction in between cannot leave it
    /// unknown.
    pub(crate) fn merge_and_pin(&mut self, info: Option<PeerInfo>, ids: &[PeerId]) -> Pinned {
        if let Some(info) = info {
            self.insert(info);
        }
        self.set_pins(ids)
    }

    /// The pinned peer ids, in no particular order.
    pub(crate) fn pinned_ids(&self) -> Vec<PeerId> {
        self.pinned.iter().copied().collect()
    }

    /// The pinned peers, for carrying across a rebuild.
    pub(crate) fn pinned_infos(&self) -> Vec<PeerInfo> {
        self.pinned
            .iter()
            .filter_map(|id| self.slots.get(id).map(|slot| slot.info.clone()))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    fn protected(&self, id: PeerId) -> bool {
        self.pinned.contains(&id) || self.held.contains_key(&id)
    }

    /// Starts a handoff of `ids` (distinct, at most [`MAX_HANDOFF_PEERS`]),
    /// releasing the oldest handoff beyond [`MAX_HANDOFFS`].
    fn hold(&mut self, ids: &[PeerId]) {
        let mut page = Vec::with_capacity(ids.len().min(MAX_HANDOFF_PEERS));
        for &id in ids {
            if page.len() >= MAX_HANDOFF_PEERS {
                break;
            }
            if !page.contains(&id) {
                page.push(id);
            }
        }
        while self.handoffs.len() >= MAX_HANDOFFS {
            let Some(oldest) = self.handoffs.pop_front() else {
                break;
            };
            self.release(&oldest);
        }
        for &id in &page {
            let count = self.held.entry(id).or_insert(0);
            *count = count.saturating_add(1);
            self.dequeue(id);
        }
        self.handoffs.push_back(page);
    }

    /// Ends one handoff: its ids that nothing else protects are queued.
    fn release(&mut self, page: &[PeerId]) {
        for &id in page {
            self.unhold(id);
            if !self.protected(id) {
                self.enqueue(id);
            }
        }
    }

    fn unhold(&mut self, id: PeerId) {
        if let Entry::Occupied(mut count) = self.held.entry(id) {
            *count.get_mut() = count.get().saturating_sub(1);
            if *count.get() == 0 {
                count.remove();
            }
        }
    }

    /// Pinned ids leave their handoffs: their hold has served its purpose.
    fn release_pinned(&mut self) {
        let mut handoffs = std::mem::take(&mut self.handoffs);
        for page in &mut handoffs {
            let mut ended = Vec::new();
            page.retain(|id| {
                let keep = !self.pinned.contains(id);
                if !keep {
                    ended.push(*id);
                }
                keep
            });
            for id in ended {
                self.unhold(id);
            }
        }
        handoffs.retain(|page| !page.is_empty());
        self.handoffs = handoffs;
    }

    /// Queues (or re-queues, touching) a known peer as the newest.
    fn enqueue(&mut self, id: PeerId) {
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        self.stamp = self.stamp.wrapping_add(1);
        if slot.queued.replace(self.stamp).is_none() {
            self.queued = self.queued.saturating_add(1);
        }
        self.fifo.push_back((id, self.stamp));
        self.evict();
        if self.fifo.len() > MAX_FIFO {
            let Self { fifo, slots, .. } = self;
            fifo.retain(|(id, stamp)| live(slots, *id, *stamp));
        }
    }

    fn dequeue(&mut self, id: PeerId) {
        if let Some(slot) = self.slots.get_mut(&id)
            && slot.queued.take().is_some()
        {
            self.queued = self.queued.saturating_sub(1);
        }
    }

    fn evict(&mut self) {
        while self.queued > MAX_UNPINNED_PEERS {
            let Some((old, stamp)) = self.fifo.pop_front() else {
                break;
            };
            if live(&self.slots, old, stamp) {
                self.slots.remove(&old);
                self.queued = self.queued.saturating_sub(1);
            }
        }
    }
}

/// Whether a queue position is current.
fn live(slots: &HashMap<PeerId, Slot>, id: PeerId, stamp: u64) -> bool {
    slots
        .get(&id)
        .is_some_and(|slot| slot.queued == Some(stamp))
}

#[cfg(test)]
mod tests;
