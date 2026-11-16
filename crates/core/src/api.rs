//! Commands the UI sends to core and events core sends back.

use crate::model::{ChatId, ChatSummary, LocalId, Message, MsgId};
use crate::store::ViewGen;

pub use crate::auth_types::{
    ApiHash, ApiHashError, AuthState, CredentialError, LocalDeletion, LogoutStatus,
    PASSWORD_CAPACITY, PasswordBuf, PasswordTooLong, RemoteLogout, SaveStatus, StorageError,
    mask_phone,
};

/// Generation of the client instance. Bumps on every rebuild, login and logout;
/// completions from an older generation are dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientGen(pub u64);

/// `Debug` redacts the phone number, the login code and the message text.
pub enum Command {
    SubmitPhone(String),
    SubmitCode(String),
    /// Consumed: the buffer is wiped when core drops it.
    SubmitPassword(PasswordBuf),
    /// Requests a new login code for the same number (`Code` state).
    ResendCode,
    /// Refetches the 2FA parameters after the password token was consumed
    /// (`PasswordTokenUnavailable`).
    RetryPasswordParams,
    /// Loads the first dialogs page, or the next page once one is loaded.
    LoadDialogs,
    OpenChat(ChatId),
    CloseChat(ChatId),
    /// Scrolls the open window up (older history).
    LoadOlder(ChatId),
    /// Scrolls a detached window back down (newer history).
    LoadNewer(ChatId),
    SendText {
        chat: ChatId,
        text: String,
        local_id: LocalId,
    },
    RetrySend(LocalId),
    /// Drops a failed or unknown send from the pending-send table. A send that
    /// is in flight cannot be discarded (`Busy`).
    DiscardSend(LocalId),
    /// The main window became active or inactive. Activation triggers a light
    /// refresh; while active, one also runs every 5 minutes (plan section 2).
    Window(WindowState),
    MarkRead {
        chat: ChatId,
        max_id: MsgId,
    },
    /// The chat whose live tail is on screen: the open chat, scrolled to its
    /// newest message. `None` when scrolled up or no chat is shown. Together
    /// with `Window(Active)`, this alone suppresses the unread increment for
    /// incoming messages; until the app sends it, every incoming message
    /// counts as unread and is cleared by `MarkRead`.
    ViewingTail(Option<ChatId>),
    /// Signs out: remote `auth.logOut`, then local deletion of the session
    /// item and the cache (plan sections 5, 16). While signed out, it retries
    /// the local deletion only.
    Logout,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubmitPhone(_) => f.write_str("SubmitPhone(<redacted>)"),
            Self::SubmitCode(_) => f.write_str("SubmitCode(<redacted>)"),
            Self::SubmitPassword(_) => f.write_str("SubmitPassword(<redacted>)"),
            Self::ResendCode => f.write_str("ResendCode"),
            Self::RetryPasswordParams => f.write_str("RetryPasswordParams"),
            Self::LoadDialogs => f.write_str("LoadDialogs"),
            Self::OpenChat(c) => f.debug_tuple("OpenChat").field(c).finish(),
            Self::CloseChat(c) => f.debug_tuple("CloseChat").field(c).finish(),
            Self::LoadOlder(c) => f.debug_tuple("LoadOlder").field(c).finish(),
            Self::LoadNewer(c) => f.debug_tuple("LoadNewer").field(c).finish(),
            Self::SendText { chat, local_id, .. } => f
                .debug_struct("SendText")
                .field("chat", chat)
                .field("local_id", local_id)
                .finish_non_exhaustive(),
            Self::RetrySend(id) => f.debug_tuple("RetrySend").field(id).finish(),
            Self::DiscardSend(id) => f.debug_tuple("DiscardSend").field(id).finish(),
            Self::Window(w) => f.debug_tuple("Window").field(w).finish(),
            Self::MarkRead { chat, max_id } => f
                .debug_struct("MarkRead")
                .field("chat", chat)
                .field("max_id", max_id)
                .finish(),
            Self::ViewingTail(c) => f.debug_tuple("ViewingTail").field(c).finish(),
            Self::Logout => f.write_str("Logout"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowState {
    Active,
    Inactive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Online,
    /// Retiring the client instance and building a new one. `attempt` counts
    /// rebuilds since the last 5 minutes of stable operation, starting at 1.
    Rebuilding {
        attempt: u32,
    },
    /// No usable instance: a build failed and the next one waits for backoff, or
    /// the account needs a new sign-in (repeated `AccountMigrated`).
    Offline,
}

/// Lifecycle of one logical send (plan section 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendState {
    Sending,
    Sent {
        id: MsgId,
    },
    /// The server accepted it but no id mapping was observed.
    SentUnconfirmed,
    /// Outcome unknown after retries; same `random_id` is reused on retry.
    Unknown {
        attempts: u8,
    },
    Failed,
    /// The server asked to wait (`FLOOD_WAIT`): not sent. Core resends it with
    /// the same `random_id` once `seconds` (counted from this event) have
    /// passed (at least one second); it can be discarded meanwhile. Automatic
    /// resends are bounded: after repeated flood waits, or past a total retry
    /// window, the send becomes `Failed` (with `UserError::FloodWait`) and
    /// waits for Retry or Discard.
    FloodWait {
        seconds: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserError {
    /// A queue or table is full, or a conflicting operation is running.
    Busy,
    Network,
    FloodWait {
        seconds: u32,
    },
    /// The server rejected the request.
    Rejected,
    NotFound,
    ShuttingDown,
    /// The server moved the account to another data center. Core rebuilds once;
    /// if it recurs, the user has to sign in again (plan section 2, risk 6).
    AccountMigrated,
    /// Not available yet in this build.
    Unsupported,
    /// Telegram rejected the phone number (`PHONE_NUMBER_INVALID`).
    PhoneInvalid,
    /// Telegram banned the phone number (`PHONE_NUMBER_BANNED`).
    PhoneBanned,
    /// The login code is wrong or expired; ask for a new one.
    InvalidCode,
    /// The 2FA password is wrong.
    InvalidPassword,
    /// The number has no Telegram account; create it in an official app.
    SignUpRequired,
    /// The server no longer accepts this session (signed out elsewhere or
    /// revoked). The local session was removed.
    SessionRevoked,
    /// The API credentials were rejected (plan section 17.2).
    Credentials(CredentialError),
    /// Device-local storage (Keychain, cache) failed.
    Storage(StorageError),
    /// A bug: a contained panic or broken invariant.
    Internal,
}

/// Which part of a window a history event extends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryDir {
    /// The newest messages. An event with a new `view` replaces the window; one
    /// with the current view (cached history shown first) extends it.
    Tail,
    Older,
    /// Newer history, including live messages (`has_more = false`).
    Newer,
}

#[derive(Debug)]
pub enum Event {
    Auth(AuthState),
    Connection(ConnState),
    /// The whole chat list, replacing the UI's copy. A reloaded first page
    /// (bootstrap, refresh) is merged into the loaded list in core, so later
    /// pages survive it.
    DialogsReset(Vec<ChatSummary>),
    /// Chats a later page added or changed: the UI updates listed chats in
    /// place and appends new ones. A page that reorders the list (after a
    /// refresh, a newly found chat is placed in server order and may evict a
    /// provisional one) is sent as `DialogsReset` instead.
    DialogsAppended(Vec<ChatSummary>),
    /// Whether more dialogs pages can be loaded (`LoadDialogs`), after every
    /// applied dialogs page. A refreshed first page restarts pagination, so
    /// this can turn `true` again after a `false`.
    DialogsPaging {
        has_more: bool,
    },
    /// A chat's summary changed (new message, unread count). A new message
    /// moves an unpinned chat to the top of the unpinned section exactly when
    /// `last_id` grew; read-state changes keep `last_id`.
    ChatBumped(ChatSummary),
    History {
        chat: ChatId,
        view: ViewGen,
        dir: HistoryDir,
        /// Messages added to the window, ascending.
        messages: Vec<Message>,
        /// Messages that slid out of the window at the opposite end.
        removed: Vec<MsgId>,
        /// More history exists in `dir` (for `Tail`: older history).
        has_more: bool,
    },
    ChatInvalidated(ChatId),
    /// The server's tail of `chat` was applied to `view` (open, bootstrap,
    /// gap or light refresh), after the `History`/`Deleted`/`Edited` events
    /// it caused. Everything in the window up to `newest` is now the server's
    /// truth: echoes of sends at or below `newest` are in it (or were
    /// deleted), and `SentUnconfirmed` echoes of this chat can be dropped.
    Reconciled {
        chat: ChatId,
        view: ViewGen,
        newest: Option<MsgId>,
    },
    Edited(Message),
    Deleted {
        chat: ChatId,
        ids: Vec<MsgId>,
    },
    SendState {
        local_id: LocalId,
        state: SendState,
    },
    Error(UserError),
}

impl Event {
    /// Approximate heap bytes this event retains, for the event byte budget.
    pub fn retained_bytes(&self) -> usize {
        const BASE: usize = std::mem::size_of::<Event>();
        let chats = |v: &[ChatSummary]| {
            std::mem::size_of_val(v) + v.iter().map(ChatSummary::retained_bytes).sum::<usize>()
        };
        BASE + match self {
            Event::DialogsReset(v) | Event::DialogsAppended(v) => chats(v),
            Event::ChatBumped(c) => c.retained_bytes(),
            Event::History {
                messages, removed, ..
            } => {
                messages.iter().map(Message::retained_bytes).sum::<usize>()
                    + removed.capacity() * std::mem::size_of::<MsgId>()
            }
            Event::Edited(m) => m.retained_bytes(),
            Event::Deleted { ids, .. } => ids.capacity() * std::mem::size_of::<MsgId>(),
            _ => 0,
        }
    }
}
