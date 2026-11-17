//! Acknowledging the loads the UI asks for (`LoadDialogs`, `LoadOlder`,
//! `LoadNewer`): each ends in its answer (`DialogsPaging`, `History`) or in
//! `Event::LoadEnded`, so the UI never waits on a request core dropped.
//! A history load asked while another load of the same chat runs is
//! deferred, not dropped, and runs once that load is released.
//!
//! The load in flight that owes the UI its answer is tracked explicitly
//! ([`Owed`], task 10f): a failure or panic ends a load the UI waits on
//! only when it is that load, so a load of an old view (or a background
//! load) never acknowledges the current view's request. Only the open chat
//! loads more, so at most one load is owed at a time, and it is forgotten
//! whenever the open chat's view changes.

use super::Actor;
use super::jobs::Load;
use crate::api::{Event, HistoryDir, LoadTarget, UserError};
use crate::backend::Backend;
use crate::model::ChatId;

/// Deferred history loads: one per direction, of the open chat only.
pub(super) const MAX_DEFERRED: usize = 2;

/// How many times one user `LoadOlder` starts over because its page no
/// longer continued the window (`apply::on_older`) before it ends with
/// `LoadEnded { error: Busy }` and the UI's bounded backoff takes over.
/// Each restart is one more page request, so this bounds what one load
/// can ask for while live messages keep trimming the window (task 10f).
pub(super) const MAX_OLDER_RESTARTS: u8 = 3;

/// A `LoadOlder`/`LoadNewer` waiting for its chat's running load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Deferred {
    pub(super) chat: ChatId,
    pub(super) dir: HistoryDir,
    /// How often this user load already started over (only `Older`).
    pub(super) restarts: u8,
}

impl Deferred {
    /// A load the UI just asked for.
    pub(super) fn fresh(chat: ChatId, dir: HistoryDir) -> Self {
        Deferred {
            chat,
            dir,
            restarts: 0,
        }
    }
}

/// The history load in flight whose outcome answers the UI's request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Owed {
    pub(super) load: Load,
    /// How often the UI's request already started over.
    pub(super) restarts: u8,
}

impl<B: Backend> Actor<B> {
    /// Tells the UI a load it asked for ended without its answer.
    pub(super) async fn load_ended(&self, target: LoadTarget, error: Option<UserError>) {
        self.emit(Event::LoadEnded { target, error }).await;
    }

    /// [`Self::load_ended`] for a history load.
    pub(super) async fn history_ended(
        &self,
        chat: ChatId,
        dir: HistoryDir,
        error: Option<UserError>,
    ) {
        if matches!(dir, HistoryDir::Older | HistoryDir::Newer) {
            self.load_ended(LoadTarget::History { chat, dir }, error)
                .await;
        }
    }

    /// Remembers a `LoadOlder`/`LoadNewer` that arrived while another load
    /// of its chat runs. Only the open chat loads more, so a deferred load of
    /// another chat is obsolete and goes (its chat is no longer shown).
    pub(super) fn defer_load(&mut self, load: Deferred) {
        self.deferred.retain(|d| d.chat == load.chat);
        if let Some(d) = self.deferred.iter_mut().find(|d| d.dir == load.dir) {
            d.restarts = d.restarts.max(load.restarts);
        } else if self.deferred.len() < MAX_DEFERRED {
            self.deferred.push(load);
        }
        debug_assert!(self.deferred.len() <= MAX_DEFERRED);
    }

    /// Starts deferred loads whose chat has no load running any more.
    pub(super) async fn run_deferred_loads(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        let busy = &self.history_busy;
        let (ready, waiting): (Vec<_>, Vec<_>) = self
            .deferred
            .drain(..)
            .partition(|d| !busy.contains_key(&d.chat));
        self.deferred = waiting;
        for load in ready {
            self.load_more(load).await;
        }
    }

    /// `load` now owes the UI the answer to a request that already started
    /// over `restarts` times.
    pub(super) fn owe(&mut self, load: Load, restarts: u8) {
        debug_assert_eq!(Some(load.chat), self.open, "only the open chat loads more");
        self.owed = Some(Owed { load, restarts });
    }

    /// `load` completed or was dropped: takes what it owed the UI, if it is
    /// the owed load. A load of an old view never is, since the owed load
    /// is forgotten when the view changes ([`Self::forget_owed`]).
    pub(super) fn take_owed(&mut self, load: Load) -> Option<Owed> {
        self.owed.take_if(|o| o.load == load)
    }

    /// `chat`'s view changes (opened, closed, replaced or invalidated): a
    /// load of the old view owes the UI nothing, as the UI forgets its
    /// pending loads with the old view.
    pub(super) fn forget_owed(&mut self, chat: ChatId) {
        self.owed.take_if(|o| o.load.chat == chat);
    }

    /// Invariant, checked once per actor loop turn: an owed load belongs
    /// to the open chat's current view.
    pub(super) fn owed_is_current(&self) -> bool {
        self.owed.is_none_or(|o| {
            Some(o.load.chat) == self.open
                && self
                    .windows
                    .get(o.load.chat)
                    .is_some_and(|w| w.view_gen() == o.load.view)
        })
    }

    /// The owed `LoadOlder` `owed` got a page that no longer continues the
    /// window. It starts over from the window's current edge, at most
    /// [`MAX_OLDER_RESTARTS`] times; then it ends with `Busy`.
    pub(super) async fn restart_older(&mut self, owed: Owed) {
        let chat = owed.load.chat;
        if owed.restarts < MAX_OLDER_RESTARTS {
            self.defer_load(Deferred {
                chat,
                dir: HistoryDir::Older,
                restarts: owed.restarts.saturating_add(1),
            });
        } else {
            tracing::debug!("older load gave up after repeated restarts");
            self.history_ended(chat, HistoryDir::Older, Some(UserError::Busy))
                .await;
        }
    }
}
