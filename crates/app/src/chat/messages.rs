//! The open chat's message window as the UI holds it: a mirror of core's
//! sliding window (plan sections 8 and 16), plus the row ops that keep a
//! GPUI `ListState` in step with it, so prepending keeps the scroll anchor.

use std::collections::VecDeque;
use std::ops::Range;

use abyss_core::api::HistoryDir;
use abyss_core::model::{ChatId, Message, MsgId};
use abyss_core::store::{MAX_MESSAGES_PER_CHAT, ViewGen};

/// Rows from either end of the window at which the next page is requested.
pub(crate) const LOAD_MARGIN: usize = 10;

/// A change to the list rows, recorded in application order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowOp {
    /// `ListState::splice(range, count)`: rows replaced by `count` new ones.
    /// The row after them may change its day separator, so callers remeasure
    /// it too.
    Splice { range: Range<usize>, count: usize },
    /// `ListState::remeasure_items(ix..ix + 1)`: same row, new content.
    Remeasure(usize),
}

/// What a change did to the rows.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    /// Stale or for another view: nothing changed.
    Ignored,
    /// A new view: the rows were replaced.
    Reset,
    Changed(Vec<RowOp>),
}

/// A history event's payload.
#[derive(Debug)]
pub(crate) struct Batch {
    pub(crate) view: ViewGen,
    pub(crate) dir: HistoryDir,
    pub(crate) messages: Vec<Message>,
    pub(crate) removed: Vec<MsgId>,
    pub(crate) has_more: bool,
}

/// Row ops with adjacent inserts and removals coalesced.
#[derive(Debug, Default)]
struct Splices(Vec<RowOp>);

impl Splices {
    fn insert(&mut self, at: usize) {
        if let Some(RowOp::Splice { range, count }) = self.0.last_mut()
            && range.start == range.end
            && at == range.start + *count
        {
            *count += 1;
            return;
        }
        self.0.push(RowOp::Splice {
            range: at..at,
            count: 1,
        });
    }

    fn remove(&mut self, at: usize) {
        if let Some(RowOp::Splice { range, count: 0 }) = self.0.last_mut()
            && at == range.start
        {
            range.end += 1;
            return;
        }
        self.0.push(RowOp::Splice {
            range: at..at + 1,
            count: 0,
        });
    }

    fn replace(&mut self, at: usize) {
        self.0.push(RowOp::Remeasure(at));
    }
}

/// Which end of the window to trim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Oldest,
    Newest,
}

/// History loads asked of core and not answered yet.
#[derive(Clone, Copy, Debug, Default)]
struct Pending {
    older: bool,
    newer: bool,
}

#[derive(Debug)]
pub(crate) struct MessageWindow {
    chat: ChatId,
    /// Core's view this mirrors; `None` until the first tail arrives.
    view: Option<ViewGen>,
    /// Ascending by id, unique, at most [`MAX_MESSAGES_PER_CHAT`].
    messages: VecDeque<Message>,
    has_older: bool,
    has_newer: bool,
    pending: Pending,
}

impl MessageWindow {
    pub(crate) fn new(chat: ChatId) -> Self {
        Self {
            chat,
            view: None,
            messages: VecDeque::new(),
            has_older: true,
            has_newer: false,
            pending: Pending::default(),
        }
    }

    pub(crate) fn chat(&self) -> ChatId {
        self.chat
    }

    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    pub(crate) fn get(&self, ix: usize) -> Option<&Message> {
        self.messages.get(ix)
    }

    pub(crate) fn contains(&self, id: MsgId) -> bool {
        self.find(id).is_ok()
    }

    pub(crate) fn by_id(&self, id: MsgId) -> Option<&Message> {
        self.messages.get(self.find(id).ok()?)
    }

    #[cfg(test)]
    pub(crate) fn newest(&self) -> Option<MsgId> {
        self.messages.back().map(|m| m.id)
    }

    /// Core's view this window mirrors, once a tail arrived.
    pub(crate) fn view(&self) -> Option<ViewGen> {
        self.view
    }

    /// Whether core has answered with this chat's history at least once.
    pub(crate) fn is_loaded(&self) -> bool {
        self.view.is_some()
    }

    pub(crate) fn has_older(&self) -> bool {
        self.has_older
    }

    /// Detached from the live tail (scrolled far back).
    pub(crate) fn has_newer(&self) -> bool {
        self.has_newer
    }

    fn find(&self, id: MsgId) -> Result<usize, usize> {
        self.messages.binary_search_by_key(&id, |m| m.id)
    }

    /// Applies a `History` event. A newer view replaces the window; an older
    /// one is stale. Until the first tail arrives only a tail is accepted, so
    /// late events from a previous opening of the chat cannot seed it.
    pub(crate) fn apply(&mut self, b: Batch) -> Applied {
        match self.view {
            None if b.dir != HistoryDir::Tail => return Applied::Ignored,
            Some(cur) if b.view < cur => return Applied::Ignored,
            _ => {}
        }
        let reset = self.view != Some(b.view);
        if reset {
            self.messages.clear();
            self.view = Some(b.view);
            self.has_older = true;
            self.has_newer = false;
        }
        // Any answer ends a load: core runs one history load per chat at a
        // time and drops requests while one runs, so a stuck flag would stall
        // scrolling. A duplicate request is harmless.
        self.pending.older = false;
        self.pending.newer = false;
        let mut splices = Splices::default();
        let trimmed = !b.removed.is_empty();
        self.remove_ids(&b.removed, &mut splices);
        for m in b.messages {
            self.upsert(m, &mut splices);
        }
        match b.dir {
            HistoryDir::Tail => {
                self.has_older = b.has_more;
                self.has_newer = false;
            }
            HistoryDir::Older => {
                self.has_older = b.has_more;
                self.has_newer |= trimmed;
            }
            HistoryDir::Newer => {
                self.has_newer = b.has_more;
                self.has_older |= trimmed;
            }
        }
        let end = if b.dir == HistoryDir::Older {
            End::Newest
        } else {
            End::Oldest
        };
        self.trim(end, &mut splices);
        if reset {
            Applied::Reset
        } else {
            Applied::Changed(splices.0)
        }
    }

    fn upsert(&mut self, m: Message, splices: &mut Splices) {
        match self.find(m.id) {
            Ok(at) => {
                if let Some(slot) = self.messages.get_mut(at) {
                    *slot = m;
                    splices.replace(at);
                }
            }
            Err(at) => {
                self.messages.insert(at, m);
                splices.insert(at);
            }
        }
    }

    fn remove_ids(&mut self, ids: &[MsgId], splices: &mut Splices) {
        for id in ids {
            if let Ok(at) = self.find(*id) {
                self.messages.remove(at);
                splices.remove(at);
            }
        }
    }

    /// Defensive bound: core already trims to the same cap.
    fn trim(&mut self, end: End, splices: &mut Splices) {
        while self.messages.len() > MAX_MESSAGES_PER_CHAT {
            match end {
                End::Oldest => {
                    self.messages.pop_front();
                    self.has_older = true;
                    splices.remove(0);
                }
                End::Newest => {
                    self.messages.pop_back();
                    self.has_newer = true;
                    splices.remove(self.messages.len());
                }
            }
        }
    }

    /// A live edit of a message in the window.
    pub(crate) fn edit(&mut self, m: Message) -> Applied {
        if m.thread.chat != self.chat {
            return Applied::Ignored;
        }
        match self.find(m.id) {
            Ok(at) => {
                let mut splices = Splices::default();
                self.upsert(m, &mut splices);
                debug_assert_eq!(splices.0.first(), Some(&RowOp::Remeasure(at)));
                Applied::Changed(splices.0)
            }
            Err(_) => Applied::Ignored,
        }
    }

    pub(crate) fn delete(&mut self, ids: &[MsgId]) -> Applied {
        let mut splices = Splices::default();
        self.remove_ids(ids, &mut splices);
        Applied::Changed(splices.0)
    }

    /// Core cleared the window (rebuild); the next tail carries a new view.
    pub(crate) fn invalidate(&mut self) -> Applied {
        self.messages.clear();
        self.has_older = true;
        self.has_newer = false;
        self.pending.older = false;
        self.pending.newer = false;
        Applied::Reset
    }

    /// A load failed: allow the next scroll to ask again.
    pub(crate) fn clear_pending(&mut self) {
        self.pending.older = false;
        self.pending.newer = false;
    }

    /// Whether to ask for older history with row `first_visible` on screen.
    pub(crate) fn wants_older(&self, first_visible: usize) -> bool {
        self.is_loaded() && self.has_older && !self.pending.older && first_visible < LOAD_MARGIN
    }

    /// Whether to ask for newer history with message row `last_visible` on
    /// screen (only while detached from the live tail).
    pub(crate) fn wants_newer(&self, last_visible: usize) -> bool {
        self.is_loaded()
            && self.has_newer
            && !self.pending.newer
            && last_visible.saturating_add(LOAD_MARGIN) >= self.messages.len()
    }

    /// Core admitted a load in `dir`; no other until it is answered.
    pub(crate) fn requested(&mut self, dir: HistoryDir) {
        match dir {
            HistoryDir::Older => self.pending.older = true,
            HistoryDir::Newer => self.pending.newer = true,
            HistoryDir::Tail => {}
        }
    }
}

#[cfg(test)]
mod tests;
