//! Which peers an instance keeps pinned in its session (plan section 2,
//! rebuild step 5: the retired snapshot carries only pinned peers).
//!
//! A rebuild must not lose the access hash of any chat core still shows or
//! uses. Core keeps every loaded dialog page across a first-page refresh
//! (`store::Dialogs::merge_first_page`), and it may address any open chat or
//! pending send after a rebuild, so the pinned set is the union of:
//! - the chats carried into this instance (`Seed::peers`), for its lifetime;
//! - recently used chats (history, sends, read marks), the last
//!   [`MAX_CARRIED_PEERS`]: open windows and pending sends;
//! - chats seen in dialog pages, the last [`MAX_DIALOGS`] seen, which covers
//!   every dialog core can hold.
//!
//! The union is bounded, and the session additionally caps it at
//! `MAX_PINNED_PEERS`; peers past that cap are cached unpinned. The priority
//! order above decides which ones those are.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::backend::MAX_CARRIED_PEERS;
use crate::model::ChatId;
use crate::store::MAX_DIALOGS;

/// A bounded least-recently-used set of chats.
#[derive(Debug)]
pub(crate) struct Recency {
    stamps: HashMap<ChatId, u64>,
    order: BTreeMap<u64, ChatId>,
    next: u64,
    cap: usize,
}

impl Recency {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            stamps: HashMap::with_capacity(cap),
            order: BTreeMap::new(),
            next: 0,
            cap,
        }
    }

    /// Marks `chat` as the most recent. Returns whether the set of chats
    /// changed (an insertion, possibly with an eviction).
    pub(crate) fn touch(&mut self, chat: ChatId) -> bool {
        let stamp = self.next;
        self.next = self.next.saturating_add(1);
        if let Some(old) = self.stamps.insert(chat, stamp) {
            self.order.remove(&old);
            self.order.insert(stamp, chat);
            return false;
        }
        self.order.insert(stamp, chat);
        while self.stamps.len() > self.cap {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.stamps.remove(&oldest);
        }
        true
    }

    pub(crate) fn len(&self) -> usize {
        self.stamps.len()
    }

    /// Most recent first.
    pub(crate) fn newest_first(&self) -> impl Iterator<Item = ChatId> + '_ {
        self.order.values().rev().copied()
    }
}

/// The chats an instance keeps pinned.
#[derive(Debug)]
pub(crate) struct Pins {
    carried: Vec<ChatId>,
    active: Recency,
    dialogs: Recency,
}

impl Pins {
    /// `carried` are the build's carried chats; `inherited` the peers the
    /// retired instance had pinned (its dialogs, open chats and sends), which
    /// stay pinned until newer dialogs push them out.
    pub(crate) fn new(carried: &[ChatId], inherited: impl IntoIterator<Item = ChatId>) -> Self {
        let carried: Vec<ChatId> = carried.iter().copied().take(MAX_CARRIED_PEERS).collect();
        let mut dialogs = Recency::new(MAX_DIALOGS);
        for chat in inherited.into_iter().take(crate::session::MAX_PINNED_PEERS) {
            if !carried.contains(&chat) {
                dialogs.touch(chat);
            }
        }
        Self {
            carried,
            active: Recency::new(MAX_CARRIED_PEERS),
            dialogs,
        }
    }

    /// A chat was addressed (history, send, read mark).
    pub(crate) fn use_chat(&mut self, chat: ChatId) -> bool {
        self.active.touch(chat)
    }

    /// Chats listed by a dialogs page, in page order.
    pub(crate) fn list(&mut self, chats: impl DoubleEndedIterator<Item = ChatId>) -> bool {
        // Backwards, so the page's first chat ends up the most recent.
        let mut changed = false;
        for chat in chats.rev() {
            changed |= self.dialogs.touch(chat);
        }
        changed
    }

    /// Every chat to pin, distinct, in priority order. Bounded by the sum of
    /// the three caps.
    pub(crate) fn all(&self) -> Vec<ChatId> {
        let cap = self.carried.len() + self.active.len() + self.dialogs.len();
        let mut seen = HashSet::with_capacity(cap);
        let mut out = Vec::with_capacity(cap);
        let ordered = self
            .carried
            .iter()
            .copied()
            .chain(self.active.newest_first())
            .chain(self.dialogs.newest_first());
        for chat in ordered {
            if seen.insert(chat) {
                out.push(chat);
            }
        }
        out
    }
}
