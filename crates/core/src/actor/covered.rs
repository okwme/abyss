//! Evidence that server history covers a `SentUnconfirmed` send (plan
//! section 9). Such a send has no known id, and is never matched by text.
//! What core does know:
//!
//! - a lower bound: the send's id is above every id of its chat core knew
//!   when the user sent it (its `floor`), since the server numbers later
//!   messages higher;
//! - an upper bound: a tail requested after the server accepted the send
//!   reaches at least to it (or it was deleted).
//!
//! So server history requested after the acceptance, applied to the chat's
//! window and contiguous from just above `floor` up to such a tail, holds
//! the send's message (unless it was deleted). Core tracks that range per
//! chat (a [`Cover`]), reports the sends it covers (`SendsCovered`), and
//! when the tail alone does not reach back far enough (more than a page of
//! newer messages), fetches older pages into the window, at most
//! [`MAX_COVER_FETCHES`] per tail.

use std::collections::HashMap;

use super::Actor;
use super::jobs::ReqId;
use crate::api::{Event, HistoryDir};
use crate::backend::Backend;
use crate::model::{ChatId, MsgId};
use crate::store::ViewGen;

/// Older pages core fetches on its own, per covering tail, to reach a send.
pub(super) const MAX_COVER_FETCHES: u8 = 4;

/// Server history applied to `view`, contiguous from `lo` up to the tail
/// requested as `req` (and everything newer that arrived since).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Cover {
    view: ViewGen,
    lo: MsgId,
    req: ReqId,
    fetches: u8,
}

/// Covers of chats with a `SentUnconfirmed` send awaiting one; an entry
/// goes when its chat has none left.
#[derive(Debug, Default)]
pub(super) struct Covers(HashMap<ChatId, Cover>);

impl<B: Backend> Actor<B> {
    /// A server tail requested as `req` was applied to `view` of `chat`,
    /// authoritative from `lo` up (`None`: it covered nothing).
    pub(super) async fn tail_covers(
        &mut self,
        chat: ChatId,
        view: ViewGen,
        req: ReqId,
        lo: Option<MsgId>,
    ) {
        let sends = &self.sends;
        self.covers.0.retain(|c, _| sends.awaits_cover(*c));
        let Some(lo) = lo.filter(|_| self.sends.awaits_cover(chat)) else {
            self.covers.0.remove(&chat);
            return;
        };
        let cover = Cover {
            view,
            lo,
            req,
            fetches: 0,
        };
        self.covers.0.insert(chat, cover);
        self.settle_cover(chat).await;
    }

    /// A server page of history older than `edge` was applied to `view`,
    /// authoritative from `lo` (`None`: it covered nothing). It extends the
    /// chat's cover when it continues it (`edge` is inside it).
    pub(super) async fn older_covers(
        &mut self,
        chat: ChatId,
        view: ViewGen,
        edge: MsgId,
        lo: Option<MsgId>,
    ) {
        let Some(lo) = lo else {
            return;
        };
        let Some(cover) = self
            .covers
            .0
            .get_mut(&chat)
            .filter(|c| c.view == view && edge >= c.lo)
        else {
            return;
        };
        cover.lo = cover.lo.min(lo);
        self.settle_cover(chat).await;
    }

    /// Reports the sends `chat`'s cover reaches, then fetches further back
    /// if some still wait for it.
    async fn settle_cover(&mut self, chat: ChatId) {
        let Some(cover) = self.covers.0.get(&chat).copied() else {
            return;
        };
        let sends = self.sends.covered_by(chat, cover.lo, cover.req);
        if !sends.is_empty() {
            tracing::debug!(count = sends.len(), "unconfirmed sends covered");
            self.emit(Event::SendsCovered { chat, sends }).await;
        }
        if !self.sends.awaits_cover(chat) {
            self.covers.0.remove(&chat);
            return;
        }
        self.fetch_cover(chat, cover).await;
    }

    /// Loads the page older than the window when the window starts inside
    /// the cover (so the page continues it), the chat is open and idle, and
    /// the fetch budget is not spent.
    async fn fetch_cover(&mut self, chat: ChatId, cover: Cover) {
        if cover.fetches >= MAX_COVER_FETCHES
            || self.open != Some(chat)
            || self.history_busy.contains_key(&chat)
        {
            return;
        }
        let Some(edge) = self
            .windows
            .get(chat)
            .filter(|w| w.view_gen() == cover.view && w.has_older())
            .and_then(|w| w.messages().front())
            .map(|m| m.id)
            .filter(|oldest| *oldest >= cover.lo)
        else {
            return;
        };
        if let Some(c) = self.covers.0.get_mut(&chat) {
            c.fetches = c.fetches.saturating_add(1);
        }
        let load = self.begin_load(chat, cover.view);
        self.fetch_more(load, HistoryDir::Older, edge).await;
    }
}
