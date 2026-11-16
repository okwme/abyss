//! Acknowledging the loads the UI asks for (`LoadDialogs`, `LoadOlder`,
//! `LoadNewer`): each ends in its answer (`DialogsPaging`, `History`) or in
//! `Event::LoadEnded`, so the UI never waits on a request core dropped.
//! A history load asked while another load of the same chat runs is
//! deferred, not dropped, and runs once that load is released.

use super::Actor;
use crate::api::{Event, HistoryDir, LoadTarget, UserError};
use crate::backend::Backend;
use crate::model::ChatId;

/// Deferred history loads: one per direction, of the open chat only.
pub(super) const MAX_DEFERRED: usize = 2;

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
    /// of `chat` runs. Only the open chat loads more, so a deferred load of
    /// another chat is obsolete and goes (its chat is no longer shown).
    pub(super) fn defer_load(&mut self, chat: ChatId, dir: HistoryDir) {
        self.deferred.retain(|(c, _)| *c == chat);
        if !self.deferred.contains(&(chat, dir)) && self.deferred.len() < MAX_DEFERRED {
            self.deferred.push((chat, dir));
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
            .partition(|(chat, _)| !busy.contains_key(chat));
        self.deferred = waiting;
        for (chat, dir) in ready {
            self.load_more(chat, dir).await;
        }
    }
}
