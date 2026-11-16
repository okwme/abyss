//! The four Stage 1 calls over raw TL requests (plan sections 7–9).
//!
//! Raw requests rather than grammers' iterators: `iter_dialogs` builds
//! `Dialog`s, which panic on unknown peers (`peer/dialog.rs:49-66`),
//! `mark_as_read` hardcodes `max_id: 0`, and `send_message` draws a fresh
//! `random_id` per call. Responses go through the pure mappers in `map`.

use std::sync::Arc;

use grammers_client::InvocationError;
use grammers_client::session::Session as _;
use grammers_client::session::types::PeerInfo;
use grammers_client::tl;

use super::errors::{backend_error, kind};
use super::invoke::{Invoke, RetryClass, call};
use super::map::{self, Directory};
use crate::backend::{BackendError, DialogsCursor, DialogsPage, HistoryPage, HistoryQuery};
use crate::model::{ChatId, MsgId};
use crate::session::AbyssSession;

/// Telegram's page size limit for dialogs and history.
pub(crate) const MAX_PAGE: u32 = 100;

fn page_limit(limit: u32) -> i32 {
    i32::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1)
}

fn fail(e: &InvocationError) -> BackendError {
    backend_error(kind(e))
}

/// The `messages.sendMessage` request for one attempt of a logical send.
/// Every attempt of the same send must carry the same `random_id`.
pub(crate) fn send_request(
    peer: tl::enums::InputPeer,
    text: String,
    random_id: i64,
) -> tl::functions::messages::SendMessage {
    tl::functions::messages::SendMessage {
        no_webpage: true,
        silent: false,
        background: false,
        clear_draft: false,
        noforwards: false,
        update_stickersets_order: false,
        invert_media: false,
        allow_paid_floodskip: false,
        peer,
        reply_to: None,
        message: text,
        random_id,
        reply_markup: None,
        entities: None,
        schedule_date: None,
        schedule_repeat_period: None,
        send_as: None,
        quick_reply_shortcut: None,
        effect: None,
        allow_paid_stars: None,
        suggested_post: None,
        rich_message: None,
    }
}

/// `messages.getHistory` arguments for a query: `(offset_id, add_offset, min_id)`.
pub(crate) fn history_offsets(query: HistoryQuery, limit: i32) -> (i32, i32, i32) {
    match query {
        HistoryQuery::Tail => (0, 0, 0),
        HistoryQuery::Before(id) => (id.0, 0, 0),
        // Newer than `id`: page backwards from it, and let `min_id` cut it off.
        HistoryQuery::After(id) => (id.0, limit.saturating_neg(), id.0),
    }
}

/// Calls of one instance over an [`Invoke`]r, with peers resolved through the
/// instance's session.
pub(crate) struct Calls<I> {
    invoker: I,
    session: Arc<AbyssSession>,
    /// Chats carried across the rebuild that built this instance; they stay
    /// pinned when the first dialogs page replaces the pins.
    carried: Arc<[ChatId]>,
}

impl<I> std::fmt::Debug for Calls<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Calls")
            .field("carried", &self.carried.len())
            .finish_non_exhaustive()
    }
}

impl<I: Invoke> Calls<I> {
    pub(crate) fn new(invoker: I, session: Arc<AbyssSession>, carried: Arc<[ChatId]>) -> Self {
        Self {
            invoker,
            session,
            carried,
        }
    }

    async fn info(&self, chat: ChatId) -> Option<PeerInfo> {
        let id = map::peer_id(chat)?;
        self.session.peer(id).await.ok().flatten()
    }

    /// The `InputPeer` for `chat`. A chat the session has no access hash for
    /// cannot be addressed: that is a definite rejection.
    async fn resolve(&self, chat: ChatId) -> Result<tl::enums::InputPeer, BackendError> {
        let info = self.info(chat).await;
        map::input_peer(chat, info.as_ref()).ok_or(BackendError::Rejected)
    }

    /// Caches every addressable peer of a response (grammers' `auto_cache_peers`
    /// does the same for the responses it handles itself).
    async fn remember(&self, dir: &Directory) {
        for info in dir.infos() {
            let _ = self.session.cache_peer(info).await;
        }
    }

    /// The first page replaces the pinned set (as `DialogsReset` replaces the
    /// list), keeping the carried chats pinned.
    async fn replace_pins(&self, pins: Vec<PeerInfo>) {
        let mut all = pins;
        for &chat in self.carried.iter() {
            if let Some(info) = self.info(chat).await {
                all.push(info);
            }
        }
        self.session.replace_pinned_peers(all);
    }

    pub(crate) async fn dialogs(
        &self,
        cursor: Option<DialogsCursor>,
        limit: u32,
    ) -> Result<DialogsPage, BackendError> {
        let limit = page_limit(limit);
        let first_page = cursor.is_none();
        let (offset_date, offset_id, offset_peer) = match cursor {
            None => (0, 0, tl::enums::InputPeer::Empty),
            Some(c) => {
                let peer = match c.offset_peer {
                    Some(chat) => self.resolve(chat).await.ok(),
                    None => None,
                };
                (
                    c.offset_date,
                    c.offset_id,
                    peer.unwrap_or(tl::enums::InputPeer::Empty),
                )
            }
        };
        let request = tl::functions::messages::GetDialogs {
            exclude_pinned: false,
            folder_id: None,
            offset_date,
            offset_id,
            offset_peer,
            limit,
            hash: 0,
        };
        let resp = call(&self.invoker, RetryClass::Read, &request)
            .await
            .map_err(|e| fail(&e))?;
        let parts = map::dialogs_parts(&resp);
        let dir = Directory::from_lists(parts.users, parts.chats);
        self.remember(&dir).await;
        let mapped = map::dialogs(&parts, &dir, usize::try_from(limit).unwrap_or(0));
        if mapped.excluded > 0 {
            tracing::debug!(excluded = mapped.excluded, "broadcast channels left out");
        }
        if first_page {
            self.replace_pins(mapped.pins).await;
        } else {
            self.session.pin_peers(mapped.pins);
        }
        Ok(DialogsPage {
            chats: mapped.chats,
            next: mapped.next,
        })
    }

    pub(crate) async fn history(
        &self,
        chat: ChatId,
        query: HistoryQuery,
        limit: u32,
    ) -> Result<HistoryPage, BackendError> {
        let peer = self.resolve(chat).await?;
        let limit = page_limit(limit);
        let (offset_id, add_offset, min_id) = history_offsets(query, limit);
        let request = tl::functions::messages::GetHistory {
            peer,
            offset_id,
            offset_date: 0,
            add_offset,
            limit,
            max_id: 0,
            min_id,
            hash: 0,
        };
        let resp = call(&self.invoker, RetryClass::Read, &request)
            .await
            .map_err(|e| fail(&e))?;
        let parts = map::messages_parts(&resp);
        let dir = Directory::from_lists(parts.users, parts.chats);
        self.remember(&dir).await;
        let limit = usize::try_from(limit).unwrap_or(0);
        Ok(map::history(&parts, &dir, chat, query, limit))
    }

    /// One attempt of a logical send. Never retried here: core retries with
    /// the same `random_id` (plan section 9).
    pub(crate) async fn send_text(
        &self,
        chat: ChatId,
        text: String,
        random_id: i64,
    ) -> Result<MsgId, BackendError> {
        let peer = self.resolve(chat).await?;
        let request = send_request(peer, text, random_id);
        let resp = call(&self.invoker, RetryClass::Never, &request)
            .await
            .map_err(|e| fail(&e))?;
        // Accepted, but without `updateMessageID` for our id: not an error to
        // retry, and not an id to show (plan section 9, `SentUnconfirmed`).
        map::sent_id(&resp, random_id).ok_or(BackendError::SentUnconfirmed)
    }

    /// Raw `readHistory` with the highest displayed id (grammers'
    /// `mark_as_read` sends `max_id: 0`).
    pub(crate) async fn read_history(
        &self,
        chat: ChatId,
        max_id: MsgId,
    ) -> Result<(), BackendError> {
        let peer = self.resolve(chat).await?;
        let res = if let Some(channel) = map::input_channel(&peer) {
            let request = tl::functions::channels::ReadHistory {
                channel,
                max_id: max_id.0,
            };
            call(&self.invoker, RetryClass::Read, &request)
                .await
                .map(drop)
        } else {
            let request = tl::functions::messages::ReadHistory {
                peer,
                max_id: max_id.0,
            };
            call(&self.invoker, RetryClass::Read, &request)
                .await
                .map(drop)
        };
        res.map_err(|e| fail(&e))
    }
}
