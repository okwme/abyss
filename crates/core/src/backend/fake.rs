//! Scripted in-process backend for development and tests (`fake` feature only).
//!
//! Supports per-operation fault injection (error, hang forever, panic), fixed
//! latency, live updates pushed by the test, and call/concurrency counters.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use super::{Backend, BackendError, DialogsCursor, DialogsPage, HistoryPage, HistoryQuery, Update};
use crate::model::{Body, ChatId, ChatKind, ChatSummary, Message, MsgId, PeerKind, ThreadKey};
use crate::rich_text::RichText;

/// Maximum faults queued per operation.
const MAX_FAULTS: usize = 1024;
/// Capacity of the pushed-update channel.
const UPDATE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Dialogs,
    History,
    Send,
    Read,
    Update,
}

#[derive(Clone, Copy, Debug)]
pub enum Fault {
    Error(BackendError),
    /// Never completes (a stalled transport).
    Hang,
    /// Panics inside the call (a bug in the Telegram library).
    Panic,
}

/// Initial data for a fake account.
#[derive(Clone, Debug, Default)]
pub struct FakeScript {
    pub dialogs: Vec<ChatSummary>,
    /// Per chat, ascending by id.
    pub history: HashMap<ChatId, Vec<Message>>,
    pub latency: Duration,
}

impl FakeScript {
    /// A small demo account for running the app without credentials.
    pub fn demo(chats: u32, messages_per_chat: u32) -> Self {
        let mut script = FakeScript::default();
        for i in 1..=i64::from(chats) {
            let id = ChatId {
                kind: if i % 3 == 0 {
                    PeerKind::Channel
                } else {
                    PeerKind::User
                },
                id: i,
            };
            script.dialogs.push(summary(id, &format!("Chat {i}")));
            let count = i32::try_from(messages_per_chat).unwrap_or(i32::MAX);
            let msgs = (1..=count)
                .map(|m| text_message(id, m, &format!("Message {m} in chat {i}")))
                .collect();
            script.history.insert(id, msgs);
        }
        script
    }
}

pub fn summary(id: ChatId, title: &str) -> ChatSummary {
    ChatSummary {
        id,
        title: crate::sanitize::display_name(title),
        kind: if id.kind == PeerKind::Channel {
            ChatKind::Supergroup {
                forum: false,
                gigagroup: false,
            }
        } else {
            ChatKind::Private
        },
        pinned: false,
        unread: 0,
        unread_mentions: 0,
        last: None,
        last_date: 0,
        can_send: true,
    }
}

pub fn text_message(chat: ChatId, id: i32, text: &str) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey { chat, topic: None },
        sender: None,
        date: i64::from(id),
        edited: false,
        outgoing: false,
        reply_to: None,
        body: Body::Text(RichText::plain(text)),
    }
}

#[derive(Debug, Default)]
struct State {
    dialogs: Vec<ChatSummary>,
    history: HashMap<ChatId, Vec<Message>>,
    faults: HashMap<Op, VecDeque<Fault>>,
    calls: HashMap<Op, usize>,
    sent: Vec<(ChatId, i64)>,
    read: Vec<(ChatId, MsgId)>,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    latency: Duration,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

/// Test-side control of a running [`FakeBackend`].
#[derive(Clone, Debug)]
pub struct FakeHandle {
    shared: Arc<Shared>,
    updates: mpsc::Sender<Update>,
}

#[derive(Debug)]
pub struct FakeBackend {
    shared: Arc<Shared>,
    updates: tokio::sync::Mutex<mpsc::Receiver<Update>>,
}

impl FakeBackend {
    pub fn new(script: FakeScript) -> (FakeBackend, FakeHandle) {
        let (tx, rx) = mpsc::channel(UPDATE_CAPACITY);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                dialogs: script.dialogs,
                history: script.history,
                ..State::default()
            }),
            latency: script.latency,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        });
        let handle = FakeHandle {
            shared: Arc::clone(&shared),
            updates: tx,
        };
        (
            FakeBackend {
                shared,
                updates: tokio::sync::Mutex::new(rx),
            },
            handle,
        )
    }

    /// Counts the call, applies latency, then any injected fault.
    async fn enter(&self, op: Op) -> Result<InFlight<'_>, BackendError> {
        let fault = self.shared.with(|s| {
            *s.calls.entry(op).or_default() += 1;
            s.faults.get_mut(&op).and_then(VecDeque::pop_front)
        });
        let guard = InFlight::new(&self.shared);
        if !self.shared.latency.is_zero() {
            tokio::time::sleep(self.shared.latency).await;
        }
        match fault {
            None => Ok(guard),
            Some(Fault::Error(e)) => Err(e),
            Some(Fault::Hang) => std::future::pending().await,
            Some(Fault::Panic) => injected_panic(op),
        }
    }
}

#[expect(
    clippy::panic,
    reason = "deliberate fault injection for panic-containment tests"
)]
fn injected_panic(op: Op) -> ! {
    panic!("injected fake backend panic in {op:?}")
}

impl Shared {
    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }
}

/// Tracks concurrent calls for admission tests.
struct InFlight<'a>(&'a Shared);

impl<'a> InFlight<'a> {
    fn new(shared: &'a Shared) -> Self {
        let now = shared.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        shared.max_in_flight.fetch_max(now, Ordering::SeqCst);
        Self(shared)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FakeHandle {
    /// Queues a one-shot fault for the next call of `op`.
    pub fn inject(&self, op: Op, fault: Fault) {
        self.shared.with(|s| {
            let q = s.faults.entry(op).or_default();
            if q.len() < MAX_FAULTS {
                q.push_back(fault);
            }
        });
    }

    pub async fn push_update(&self, update: Update) {
        let _ = self.updates.send(update).await;
    }

    pub fn calls(&self, op: Op) -> usize {
        self.shared.with(|s| s.calls.get(&op).copied().unwrap_or(0))
    }

    pub fn in_flight(&self) -> usize {
        self.shared.in_flight.load(Ordering::SeqCst)
    }

    pub fn max_in_flight(&self) -> usize {
        self.shared.max_in_flight.load(Ordering::SeqCst)
    }

    /// `(chat, random_id)` of every send that reached the backend.
    pub fn sent(&self) -> Vec<(ChatId, i64)> {
        self.shared.with(|s| s.sent.clone())
    }

    pub fn read_calls(&self) -> Vec<(ChatId, MsgId)> {
        self.shared.with(|s| s.read.clone())
    }

    /// Replaces a chat's server-side history (e.g. simulate deletions).
    pub fn set_history(&self, chat: ChatId, messages: Vec<Message>) {
        self.shared.with(|s| {
            s.history.insert(chat, messages);
        });
    }
}

fn page(all: &[Message], query: HistoryQuery, limit: usize) -> HistoryPage {
    let (selected, has_more): (Vec<Message>, bool) = match query {
        HistoryQuery::Tail => {
            let start = all.len().saturating_sub(limit);
            (all.iter().skip(start).cloned().collect(), start > 0)
        }
        HistoryQuery::Before(id) => {
            let below: Vec<&Message> = all.iter().filter(|m| m.id < id).collect();
            let start = below.len().saturating_sub(limit);
            (below.into_iter().skip(start).cloned().collect(), start > 0)
        }
        HistoryQuery::After(id) => {
            let above: Vec<&Message> = all.iter().filter(|m| m.id > id).collect();
            let more = above.len() > limit;
            (above.into_iter().take(limit).cloned().collect(), more)
        }
    };
    HistoryPage {
        messages: selected,
        has_more,
    }
}

impl Backend for FakeBackend {
    async fn dialogs(
        &self,
        cursor: Option<DialogsCursor>,
        limit: u32,
    ) -> Result<DialogsPage, BackendError> {
        let _g = self.enter(Op::Dialogs).await?;
        let start = cursor.map_or(0, |c| to_usize(c.index));
        let limit = to_usize(limit);
        Ok(self.shared.with(|s| {
            let chats: Vec<ChatSummary> =
                s.dialogs.iter().skip(start).take(limit).cloned().collect();
            let end = start + chats.len();
            let next = (end < s.dialogs.len()).then(|| DialogsCursor {
                offset_date: 0,
                offset_id: 0,
                offset_peer: None,
                index: u32::try_from(end).unwrap_or(u32::MAX),
            });
            DialogsPage { chats, next }
        }))
    }

    async fn history(
        &self,
        chat: ChatId,
        query: HistoryQuery,
        limit: u32,
    ) -> Result<HistoryPage, BackendError> {
        let _g = self.enter(Op::History).await?;
        Ok(self.shared.with(|s| {
            let all = s.history.get(&chat).map(Vec::as_slice).unwrap_or_default();
            page(all, query, to_usize(limit))
        }))
    }

    async fn send_text(
        &self,
        chat: ChatId,
        text: String,
        random_id: i64,
    ) -> Result<MsgId, BackendError> {
        let _g = self.enter(Op::Send).await?;
        // The real server also echoes own sends as updates; the fake does not, so tests
        // control update ordering explicitly with `FakeHandle::push_update`.
        let id = self.shared.with(|s| {
            s.sent.push((chat, random_id));
            let history = s.history.entry(chat).or_default();
            let id = history.last().map_or(1, |m| m.id.0.saturating_add(1));
            let mut msg = text_message(chat, id, &text);
            msg.outgoing = true;
            history.push(msg);
            MsgId(id)
        });
        Ok(id)
    }

    async fn read_history(&self, chat: ChatId, max_id: MsgId) -> Result<(), BackendError> {
        let _g = self.enter(Op::Read).await?;
        self.shared.with(|s| s.read.push((chat, max_id)));
        Ok(())
    }

    async fn next_update(&self) -> Result<Update, BackendError> {
        let fault = self
            .shared
            .with(|s| s.faults.get_mut(&Op::Update).and_then(VecDeque::pop_front));
        match fault {
            Some(Fault::Error(e)) => return Err(e),
            Some(Fault::Hang) => std::future::pending::<()>().await,
            Some(Fault::Panic) => injected_panic(Op::Update),
            None => {}
        }
        let mut rx = self.updates.lock().await;
        rx.recv().await.ok_or(BackendError::Transient)
    }
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
