//! The seam between core and a Telegram implementation.
//!
//! A [`Backend`] is a factory of client *instances* (plan section 2): each build
//! yields a [`Client`] for calls, an [`UpdateStream`], a runner future that core
//! supervises, and an [`InstanceControl`] used to retire it. Core owns the
//! lifecycle; a backend never rebuilds itself.
//!
//! The traits use `impl Future + Send` returns rather than `dyn` objects: the actor
//! is generic over one backend chosen at startup, so there is no need for object
//! safety, and static dispatch avoids boxing every call. `BackendChoice` maps the
//! runtime choice onto the concrete type in `start`.
//!
//! There is deliberately no media or download method: Stage 1 never fetches media,
//! and a backend cannot be asked to.

#[cfg(test)]
mod contract;
#[cfg(feature = "fake")]
pub mod fake;
pub mod grammers;
mod seed;

use std::future::Future;
use std::pin::Pin;

pub use seed::{CarriedPeers, MAX_CARRIED_PEERS, Seed, SessionSeed, SessionSnapshot};

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

#[derive(Clone, Debug)]
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
    /// An authenticated, non-file request was answered with a `303` migration.
    /// Stage 1 does not move authorizations between DCs (plan section 2, risk 6).
    Migrated,
    /// Sends only: the server accepted the message, but no id mapping is known
    /// (`RANDOM_ID_DUPLICATE` on a resend, or a response without
    /// `updateMessageID`). Never retried (plan section 9).
    SentUnconfirmed,
}

/// One item from an instance's update stream.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "`Baseline` occurs once per stream; boxing would allocate per update"
)]
pub enum StreamItem {
    /// The update baseline (`updates.getState`) is recorded. Yielded at most once
    /// per stream. Snapshots taken after it cannot miss an update the stream
    /// will deliver.
    Baseline,
    Update(Update),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// May clear up; core retries on the same stream with backoff.
    Transient,
    /// The stream is unusable; core rebuilds the instance.
    Fatal,
}

/// Calls bound to one client instance.
pub trait Client: Send + Sync + 'static {
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
}

/// An instance's live updates. The first `next` call must start establishing
/// the baseline before it waits for updates (grammers sends `updates.getState`).
pub trait UpdateStream: Send + 'static {
    fn next(&mut self) -> impl Future<Output = Result<StreamItem, StreamError>> + Send;
}

/// Retirement hooks for one instance.
pub trait InstanceControl: Send + Sync + 'static {
    /// Asks the runner to stop (grammers: `pool.handle.quit()`). Must not block.
    fn quit(&self);

    /// Called once the runner has completed or been cancelled: revokes the
    /// session's write token, so a late write from this instance is dropped, and
    /// returns the live critical snapshot for the next build.
    fn retire(self) -> SessionSnapshot;
}

/// The connection driver of an instance. Its completion, for any reason other
/// than `quit`, means the instance is dead.
pub type Runner = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A freshly built client instance.
pub struct Instance<B: Backend + ?Sized> {
    pub client: B::Client,
    pub updates: B::Updates,
    pub runner: Runner,
    pub control: B::Control,
}

impl<B: Backend + ?Sized> std::fmt::Debug for Instance<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance").finish_non_exhaustive()
    }
}

/// Builds client instances.
pub trait Backend: Send + Sync + 'static {
    type Client: Client;
    type Updates: UpdateStream;
    type Control: InstanceControl;

    /// Builds a fresh instance from `seed`. Spawns nothing: core spawns the
    /// runner and the update consumer.
    fn build(
        &self,
        seed: Seed,
    ) -> impl Future<Output = Result<Instance<Self>, BackendError>> + Send;
}
