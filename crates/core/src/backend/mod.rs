//! The seam between core and a Telegram implementation.
//!
//! The trait uses `impl Future + Send` returns rather than `dyn` objects: the actor
//! is generic over one backend chosen at startup, so there is no need for object
//! safety, and static dispatch avoids boxing every call. `BackendChoice` maps the
//! runtime choice onto the concrete type in `start`.
//!
//! There is deliberately no media or download method: Stage 1 never fetches media,
//! and a backend cannot be asked to.

#[cfg(feature = "fake")]
pub mod fake;

use std::future::Future;

use crate::model::{ChatId, ChatSummary, Message, MsgId};

/// Opaque position in the dialogs list, produced by the backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogsCursor {
    pub(crate) offset_date: i32,
    pub(crate) offset_id: i32,
    pub(crate) offset_peer: Option<ChatId>,
    /// Index for backends that page by position (the fake).
    pub(crate) index: u32,
}

#[derive(Debug)]
pub struct DialogsPage {
    pub chats: Vec<ChatSummary>,
    /// `None` when this was the last page.
    pub next: Option<DialogsCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryQuery {
    /// The newest messages.
    Tail,
    /// Messages with id below the given one.
    Before(MsgId),
    /// Messages with id above the given one.
    After(MsgId),
}

#[derive(Debug)]
pub struct HistoryPage {
    /// Ascending by id.
    pub messages: Vec<Message>,
    /// More history exists in the queried direction.
    pub has_more: bool,
}

#[derive(Debug)]
pub enum Update {
    NewMessage(Message),
    Edited(Message),
    /// `chat` is `None` for private chats and basic groups (account-unique ids).
    Deleted {
        chat: Option<ChatId>,
        ids: Vec<MsgId>,
    },
    /// Read on another device.
    ReadInbox {
        chat: ChatId,
        still_unread: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The server rejected the request; retrying with the same input won't help.
    Rejected,
    FloodWait {
        seconds: u32,
    },
    /// Network or transport failure; the outcome of a write is unknown.
    Transient,
    /// The authorization is gone.
    Unauthorized,
}

pub trait Backend: Send + Sync + 'static {
    fn dialogs(
        &self,
        cursor: Option<DialogsCursor>,
        limit: u32,
    ) -> impl Future<Output = Result<DialogsPage, BackendError>> + Send;

    fn history(
        &self,
        chat: ChatId,
        query: HistoryQuery,
        limit: u32,
    ) -> impl Future<Output = Result<HistoryPage, BackendError>> + Send;

    /// Sends text with a caller-chosen `random_id`, which must be reused for every
    /// retry of the same logical send so the server can deduplicate.
    fn send_text(
        &self,
        chat: ChatId,
        text: String,
        random_id: i64,
    ) -> impl Future<Output = Result<MsgId, BackendError>> + Send;

    /// Marks history read up to and including `max_id`.
    fn read_history(
        &self,
        chat: ChatId,
        max_id: MsgId,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Waits for the next live update.
    fn next_update(&self) -> impl Future<Output = Result<Update, BackendError>> + Send;
}
