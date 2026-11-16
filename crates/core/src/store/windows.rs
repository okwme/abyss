//! Bounded, sliding message windows for recently opened chats.
//!
//! A window holds a contiguous run of a chat's history. It slides: loading older
//! history past the cap drops the newest messages (the window detaches from the
//! live tail), and loading newer history drops the oldest. Dropped messages are
//! re-read from the disk cache or the server when the user scrolls back.

use std::collections::{HashMap, VecDeque};

use crate::backend::HistoryPage;
use crate::model::{ChatId, Message, MsgId, group_by_chat};

/// Maximum messages retained per chat window.
pub const MAX_MESSAGES_PER_CHAT: usize = 2000;
/// Maximum number of chat windows retained (LRU).
pub const MAX_WINDOWS: usize = 8;
/// Maximum retained bytes across all windows.
pub const MAX_STORE_BYTES: usize = 64 * 1024 * 1024;

/// Identifies one incarnation of a chat window. Completions tagged with an older
/// generation are stale and must be dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ViewGen(pub u64);

/// The completion belongs to a view that was closed, evicted or replaced.
#[derive(Debug, PartialEq, Eq)]
pub struct Stale;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Oldest,
    Newest,
}

/// Contiguous messages of one chat, ascending by id, without duplicates.
#[derive(Debug)]
pub struct Window {
    view_gen: ViewGen,
    messages: VecDeque<Message>,
    bytes: usize,
    /// Older history exists outside the window.
    has_older: bool,
    /// Newer history exists outside the window: the window does not reach the
    /// live tail, so live messages are not appended to it.
    has_newer: bool,
    /// The chat's disk cache is known to be contiguous with the live tail
    /// (set once a server tail was written through), so older and newer pages
    /// may be served from it. Lives and dies with the window, so it is bounded
    /// by [`MAX_WINDOWS`]; cleared on reopen and invalidation.
    cache_contiguous: bool,
}

impl Window {
    fn new(view_gen: ViewGen) -> Self {
        Self {
            view_gen,
            messages: VecDeque::new(),
            bytes: 0,
            has_older: true,
            has_newer: false,
            cache_contiguous: false,
        }
    }

    pub fn cache_contiguous(&self) -> bool {
        self.cache_contiguous
    }

    fn reset(&mut self, view_gen: ViewGen) {
        *self = Self::new(view_gen);
    }

    pub fn view_gen(&self) -> ViewGen {
        self.view_gen
    }

    pub fn messages(&self) -> &VecDeque<Message> {
        &self.messages
    }

    pub fn has_older(&self) -> bool {
        self.has_older
    }

    pub fn has_newer(&self) -> bool {
        self.has_newer
    }

    fn oldest(&self) -> Option<MsgId> {
        self.messages.front().map(|m| m.id)
    }

    fn newest(&self) -> Option<MsgId> {
        self.messages.back().map(|m| m.id)
    }

    /// The message with `id`, if it is in the window.
    pub fn get(&self, id: MsgId) -> Option<&Message> {
        self.messages.get(self.find(id).ok()?)
    }

    fn find(&self, id: MsgId) -> Result<usize, usize> {
        self.messages.binary_search_by_key(&id, |m| m.id)
    }

    /// Removes one message from `end`, marking that direction as having more.
    fn pop(&mut self, end: End) -> Option<Message> {
        let m = match end {
            End::Oldest => {
                self.has_older = true;
                self.messages.pop_front()?
            }
            End::Newest => {
                self.has_newer = true;
                self.messages.pop_back()?
            }
        };
        self.bytes = self.bytes.saturating_sub(m.retained_bytes());
        Some(m)
    }
}

/// Outcome of a mutation, for UI events.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub inserted: Vec<MsgId>,
    pub removed: Vec<MsgId>,
}

/// LRU set of chat windows with a global byte budget and a message-id index for
/// chats whose ids are account-unique (private chats and basic groups).
#[derive(Debug, Default)]
pub struct Windows {
    by_chat: HashMap<ChatId, Window>,
    /// Most recently used last. Always holds exactly the keys of `by_chat`.
    lru: Vec<ChatId>,
    next_gen: u64,
    bytes: usize,
    account_index: HashMap<MsgId, ChatId>,
    /// Bumped whenever the set or recency order of windowed chats changes.
    rev: u64,
}

impl Windows {
    /// Changes whenever [`Windows::chats`] may have changed.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn get(&self, chat: ChatId) -> Option<&Window> {
        self.by_chat.get(&chat)
    }

    pub fn total_bytes(&self) -> usize {
        self.bytes
    }

    fn bump_gen(&mut self) -> ViewGen {
        self.next_gen += 1;
        ViewGen(self.next_gen)
    }

    /// Opens (or re-opens) a chat window, marking it most recently used. Returns the
    /// generation that history completions for this view must carry.
    pub fn open(&mut self, chat: ChatId) -> ViewGen {
        let view_gen = self.bump_gen();
        let w = self
            .by_chat
            .entry(chat)
            .or_insert_with(|| Window::new(view_gen));
        w.view_gen = view_gen;
        // Re-established by the tail this opening fetches.
        w.cache_contiguous = false;
        self.lru.retain(|c| *c != chat);
        self.lru.push(chat);
        self.rev = self.rev.wrapping_add(1);
        while self.lru.len() > MAX_WINDOWS {
            let evicted = self.lru.remove(0);
            self.drop_window(evicted);
        }
        debug_assert_eq!(self.lru.len(), self.by_chat.len());
        view_gen
    }

    /// Closes the view: pending completions become stale; messages stay cached.
    pub fn close(&mut self, chat: ChatId) {
        let view_gen = self.bump_gen();
        if let Some(w) = self.by_chat.get_mut(&chat) {
            w.view_gen = view_gen;
            w.cache_contiguous = false;
        }
    }

    /// Marks `chat`'s disk cache as contiguous with the live tail (see
    /// [`Window::cache_contiguous`]). No-op without a window.
    pub fn set_cache_contiguous(&mut self, chat: ChatId) {
        if let Some(w) = self.by_chat.get_mut(&chat) {
            w.cache_contiguous = true;
        }
    }

    /// Whether `chat` has a window whose disk cache is contiguous.
    pub fn is_cache_contiguous(&self, chat: ChatId) -> bool {
        self.by_chat
            .get(&chat)
            .is_some_and(Window::cache_contiguous)
    }

    /// Clears all messages of `chat` in place (keeping its LRU position) and
    /// returns the new generation if it was cached.
    pub fn invalidate(&mut self, chat: ChatId) -> Option<ViewGen> {
        let view_gen = self.bump_gen();
        let w = self.by_chat.get_mut(&chat)?;
        let old = std::mem::take(&mut w.messages);
        self.bytes = self.bytes.saturating_sub(w.bytes);
        w.reset(view_gen);
        for m in &old {
            self.unindex(chat, m.id);
        }
        Some(view_gen)
    }

    /// Chats with a window, most recently used last.
    pub fn chats(&self) -> impl Iterator<Item = ChatId> + '_ {
        self.lru.iter().copied()
    }

    /// Clears every window (client rebuild), in recency order.
    pub fn invalidate_all(&mut self) -> Vec<(ChatId, ViewGen)> {
        let chats = self.lru.clone();
        chats
            .into_iter()
            .filter_map(|c| self.invalidate(c).map(|g| (c, g)))
            .collect()
    }

    fn drop_window(&mut self, chat: ChatId) {
        self.rev = self.rev.wrapping_add(1);
        if let Some(w) = self.by_chat.remove(&chat) {
            self.bytes = self.bytes.saturating_sub(w.bytes);
            for m in &w.messages {
                self.unindex(chat, m.id);
            }
        }
        self.lru.retain(|c| *c != chat);
    }

    fn index(&mut self, chat: ChatId, id: MsgId) {
        if chat.kind.has_account_unique_ids() {
            self.account_index.insert(id, chat);
        }
    }

    fn unindex(&mut self, chat: ChatId, id: MsgId) {
        if chat.kind.has_account_unique_ids() && self.account_index.get(&id) == Some(&chat) {
            self.account_index.remove(&id);
        }
    }

    /// Chat that holds account-unique message `id`, if cached.
    pub fn chat_of(&self, id: MsgId) -> Option<ChatId> {
        self.account_index.get(&id).copied()
    }

    /// Inserts `msg` at its sorted position; caller guarantees it is absent.
    fn insert_sorted(&mut self, chat: ChatId, pos: usize, msg: Message, change: &mut Change) {
        let Some(w) = self.by_chat.get_mut(&chat) else {
            return;
        };
        let (id, added) = (msg.id, msg.retained_bytes());
        w.messages.insert(pos, msg);
        w.bytes += added;
        self.bytes += added;
        self.index(chat, id);
        change.inserted.push(id);
    }

    /// Pops from `end` until the window is within the per-chat cap.
    fn trim(&mut self, chat: ChatId, end: End, change: &mut Change) {
        while self
            .by_chat
            .get(&chat)
            .is_some_and(|w| w.messages.len() > MAX_MESSAGES_PER_CHAT)
            && self.pop_one(chat, end, change)
        {}
    }

    fn pop_one(&mut self, chat: ChatId, end: End, change: &mut Change) -> bool {
        let Some(old) = self.by_chat.get_mut(&chat).and_then(|w| w.pop(end)) else {
            return false;
        };
        self.bytes = self.bytes.saturating_sub(old.retained_bytes());
        self.unindex(chat, old.id);
        change.removed.push(old.id);
        true
    }

    /// Adds a page of older history (any order). Messages not older than the
    /// current oldest are ignored. Past the cap, the newest messages are dropped
    /// and the window detaches from the live tail.
    pub fn prepend(
        &mut self,
        chat: ChatId,
        view_gen: ViewGen,
        page: HistoryPage,
    ) -> Result<Change, Stale> {
        let w = self.current(chat, view_gen)?;
        let bound = w.oldest();
        w.has_older = page.has_more;
        let older = sorted_unique(page.messages, |id| bound.is_none_or(|o| id < o));
        let mut change = Change::default();
        for m in older.into_iter().rev() {
            self.insert_sorted(chat, 0, m, &mut change);
        }
        self.trim(chat, End::Newest, &mut change);
        self.enforce_byte_budget(chat, End::Newest, &mut change);
        Ok(change)
    }

    /// Adds a page of older history without ever dropping a message: of the
    /// page's messages older than the window, only the newest ones (adjacent
    /// to the window, so it stays contiguous) that fit under the per-chat cap
    /// and the byte budget are inserted. For pages core fetches on its own,
    /// which must never move the end the user is looking at. When some did
    /// not fit, older history is still marked as existing.
    pub fn prepend_fitting(
        &mut self,
        chat: ChatId,
        view_gen: ViewGen,
        page: HistoryPage,
    ) -> Result<Change, Stale> {
        let w = self.current(chat, view_gen)?;
        let bound = w.oldest();
        let room = MAX_MESSAGES_PER_CHAT.saturating_sub(w.messages.len());
        let more = page.has_more;
        let older = sorted_unique(page.messages, |id| bound.is_none_or(|o| id < o));
        let mut change = Change::default();
        let mut fitted = true;
        for m in older.into_iter().rev() {
            let bytes = self.bytes.saturating_add(m.retained_bytes());
            if change.inserted.len() >= room || bytes > MAX_STORE_BYTES {
                fitted = false;
                break;
            }
            self.insert_sorted(chat, 0, m, &mut change);
        }
        if let Some(w) = self.by_chat.get_mut(&chat) {
            w.has_older = more || !fitted;
            debug_assert!(w.messages.len() <= MAX_MESSAGES_PER_CHAT);
        }
        Ok(change)
    }

    /// Adds a page of newer history when scrolling back down a detached window.
    /// Past the cap, the oldest messages are dropped. `has_more = false` means
    /// the window has reached the live tail again.
    pub fn append_newer(
        &mut self,
        chat: ChatId,
        view_gen: ViewGen,
        page: HistoryPage,
    ) -> Result<Change, Stale> {
        let w = self.current(chat, view_gen)?;
        let bound = w.newest();
        w.has_newer = page.has_more;
        let newer = sorted_unique(page.messages, |id| bound.is_none_or(|n| id > n));
        let mut change = Change::default();
        for m in newer {
            let pos = self.by_chat.get(&chat).map_or(0, |w| w.messages.len());
            self.insert_sorted(chat, pos, m, &mut change);
        }
        self.trim(chat, End::Oldest, &mut change);
        self.enforce_byte_budget(chat, End::Oldest, &mut change);
        Ok(change)
    }

    fn current(&mut self, chat: ChatId, view_gen: ViewGen) -> Result<&mut Window, Stale> {
        self.by_chat
            .get_mut(&chat)
            .filter(|w| w.view_gen == view_gen)
            .ok_or(Stale)
    }

    /// Applies a live message. Replaces it if cached. Otherwise inserts it only if
    /// the window reaches the live tail and the message is not below loaded
    /// history; past the cap, the oldest messages are dropped.
    pub fn upsert(&mut self, msg: Message) -> Change {
        let chat = msg.thread.chat;
        let mut change = Change::default();
        let Some(w) = self.by_chat.get(&chat) else {
            return change;
        };
        let pos = match w.find(msg.id) {
            Ok(_) => {
                self.replace(msg);
                return change;
            }
            Err(_) if w.has_newer => return change,
            Err(0) if !w.messages.is_empty() && w.has_older => return change,
            Err(pos) => pos,
        };
        self.insert_sorted(chat, pos, msg, &mut change);
        self.trim(chat, End::Oldest, &mut change);
        self.enforce_byte_budget(chat, End::Oldest, &mut change);
        change
    }

    /// Replaces a cached message (edit). Never inserts: an edit of a message outside
    /// the window must not create a hole in history. Returns whether it applied.
    pub fn replace(&mut self, msg: Message) -> bool {
        let chat = msg.thread.chat;
        let Some(w) = self.by_chat.get_mut(&chat) else {
            return false;
        };
        let Ok(pos) = w.find(msg.id) else {
            return false;
        };
        let added = msg.retained_bytes();
        let Some(slot) = w.messages.get_mut(pos) else {
            return false;
        };
        let old = std::mem::replace(slot, msg).retained_bytes();
        w.bytes = w.bytes.saturating_sub(old) + added;
        self.bytes = self.bytes.saturating_sub(old) + added;
        self.enforce_byte_budget(chat, End::Oldest, &mut Change::default());
        true
    }

    /// Removes messages from a chat window. Returns the ids actually removed.
    pub fn delete(&mut self, chat: ChatId, ids: &[MsgId]) -> Vec<MsgId> {
        let Some(w) = self.by_chat.get_mut(&chat) else {
            return Vec::new();
        };
        let mut removed = Vec::new();
        for id in ids {
            if let Ok(pos) = w.find(*id)
                && let Some(m) = w.messages.remove(pos)
            {
                w.bytes = w.bytes.saturating_sub(m.retained_bytes());
                self.bytes = self.bytes.saturating_sub(m.retained_bytes());
                removed.push(m.id);
            }
        }
        for id in &removed {
            self.unindex(chat, *id);
        }
        removed
    }

    /// Deletion without a chat (private/basic groups): resolved through the index.
    pub fn delete_unscoped(&mut self, ids: &[MsgId]) -> Vec<(ChatId, Vec<MsgId>)> {
        group_by_chat(ids.iter().filter_map(|id| Some((self.chat_of(*id)?, *id))))
            .into_iter()
            .map(|(chat, ids)| (chat, self.delete(chat, &ids)))
            .filter(|(_, removed)| !removed.is_empty())
            .collect()
    }

    /// Evicts least recently used windows other than `keep`; if `keep` alone is
    /// over budget, trims it from `end` (the side the user is not looking at).
    fn enforce_byte_budget(&mut self, keep: ChatId, end: End, change: &mut Change) {
        while self.bytes > MAX_STORE_BYTES {
            if let Some(victim) = self.lru.iter().copied().find(|c| *c != keep) {
                self.drop_window(victim);
                continue;
            }
            if !self.pop_one(keep, end, change) {
                return;
            }
        }
    }
}

/// Sorts `page` by id, drops duplicates and every message whose id fails `keep`.
fn sorted_unique(mut page: Vec<Message>, keep: impl Fn(MsgId) -> bool) -> Vec<Message> {
    page.retain(|m| keep(m.id));
    page.sort_unstable_by_key(|m| m.id);
    page.dedup_by_key(|m| m.id);
    page
}

#[cfg(test)]
mod tests;
