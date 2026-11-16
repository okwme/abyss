//! Commands the UI sends to core and events core sends back.

use zeroize::Zeroizing;

use crate::model::{ChatId, ChatSummary, LocalId, Message, MsgId};
use crate::store::ViewGen;

/// Generation of the client instance. Bumps on every rebuild, login and logout;
/// completions from an older generation are dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientGen(pub u64);

/// Password bytes for 2FA. Placeholder until task 5/6 replaces it with the fixed
/// 256-byte secure buffer (plan section 10); already zeroized on drop.
pub struct PasswordBuf(pub Zeroizing<Vec<u8>>);

impl std::fmt::Debug for PasswordBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordBuf(<redacted>)")
    }
}

#[derive(Debug)]
pub enum Command {
    SubmitPhone(String),
    SubmitCode(String),
    SubmitPassword(PasswordBuf),
    ResendCode,
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
    MarkRead {
        chat: ChatId,
        max_id: MsgId,
    },
    Logout,
}

/// Login state. Placeholder: tasks 5–6 add the phone/code/password states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthState {
    SignedIn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Online,
    Rebuilding { attempt: u32 },
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
    /// Not available yet in this build (auth lands in tasks 5–6).
    Unsupported,
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
    DialogsReset(Vec<ChatSummary>),
    DialogsAppended(Vec<ChatSummary>),
    /// A chat's summary changed (new message, unread count).
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
