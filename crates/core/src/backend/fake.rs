//! Scripted in-process backend for development and tests (`fake` feature only).
//!
//! Every build yields a fresh instance over one shared fake server. Supports
//! per-call fault injection (error, hang forever, panic, delay), lifecycle faults
//! (failed build, missing baseline, runner exit or panic, a runner that is slow
//! to quit or ignores `quit`), stream faults (recoverable or fatal error, panic),
//! fixed latency, live updates pushed by the test, and counters.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, mpsc};

use super::{
    Backend, BackendError, Client, DialogsCursor, DialogsPage, HistoryPage, HistoryQuery, Instance,
    InstanceControl, Seed, SessionSeed, SessionSnapshot, StreamError, StreamItem, Update,
    UpdateStream,
};
use crate::model::{Body, ChatId, ChatKind, ChatSummary, Message, MsgId, PeerKind, ThreadKey};
use crate::rich_text::RichText;

/// Maximum faults queued per operation.
const MAX_FAULTS: usize = 1024;
/// Capacity of the pushed-update channel (shared by every instance's stream).
const UPDATE_CAPACITY: usize = 256;
/// Recorded sends, seeds and other logs are capped at this many entries.
const MAX_LOG: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Dialogs,
    History,
    Send,
    Read,
}

#[derive(Clone, Copy, Debug)]
pub enum Fault {
    Error(BackendError),
    /// Never completes (a stalled transport).
    Hang,
    /// Panics inside the call (a bug in the Telegram library).
    Panic,
    /// Succeeds after the given extra delay.
    Delay(Duration),
}

/// A fault applied to the next build.
#[derive(Clone, Copy, Debug)]
pub enum BuildFault {
    /// The build itself fails.
    Fail(BackendError),
    /// The stream never reports a baseline (`getState` failed).
    NoBaseline,
}

/// How the current runner reacts to `quit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuitBehavior {
    Prompt,
    /// Stops after the given delay.
    Slow(Duration),
    /// Never stops by itself; only abort ends it.
    Ignore,
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
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body: Body::Text(RichText::plain(text)),
    }
}

/// What the test asks the current runner to do.
#[derive(Clone, Copy, Debug)]
enum RunnerCmd {
    Exit,
    Panic,
}

/// What the shared stream channel carries.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "test-only channel; updates are the common item"
)]
enum StreamCmd {
    Update(Update),
    Error(StreamError),
    Panic,
}

#[derive(Debug, Default)]
struct State {
    dialogs: Vec<ChatSummary>,
    history: HashMap<ChatId, Vec<Message>>,
    faults: HashMap<Op, VecDeque<Fault>>,
    build_faults: VecDeque<BuildFault>,
    calls: HashMap<Op, usize>,
    sent: Vec<(ChatId, i64)>,
    attempted: Vec<(ChatId, i64)>,
    read: Vec<(ChatId, MsgId)>,
    seeds: Vec<Seed>,
    quit: Option<QuitBehavior>,
    runner: Option<mpsc::Sender<RunnerCmd>>,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    latency: Duration,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    runners_finished: AtomicUsize,
    retired: AtomicUsize,
}

/// Test-side control of a running [`FakeBackend`].
#[derive(Clone, Debug)]
pub struct FakeHandle {
    shared: Arc<Shared>,
    stream: mpsc::Sender<StreamCmd>,
}

#[derive(Debug)]
pub struct FakeBackend {
    shared: Arc<Shared>,
    stream: Arc<tokio::sync::Mutex<mpsc::Receiver<StreamCmd>>>,
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
            runners_finished: AtomicUsize::new(0),
            retired: AtomicUsize::new(0),
        });
        let handle = FakeHandle {
            shared: Arc::clone(&shared),
            stream: tx,
        };
        let backend = FakeBackend {
            shared,
            stream: Arc::new(tokio::sync::Mutex::new(rx)),
        };
        (backend, handle)
    }
}

#[expect(
    clippy::panic,
    reason = "deliberate fault injection for panic-containment tests"
)]
fn injected_panic(what: &str) -> ! {
    panic!("injected fake backend panic in {what}")
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

fn push_capped<T>(log: &mut Vec<T>, item: T) {
    if log.len() < MAX_LOG {
        log.push(item);
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

/// Counts runner completions, including cancellation (the future is dropped).
struct RunnerGuard(Arc<Shared>);

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        self.0.runners_finished.fetch_add(1, Ordering::SeqCst);
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

    /// Queues a fault for the next build.
    pub fn on_next_build(&self, fault: BuildFault) {
        self.shared.with(|s| {
            if s.build_faults.len() < MAX_FAULTS {
                s.build_faults.push_back(fault);
            }
        });
    }

    /// Sets how runners react to `quit` from now on.
    pub fn set_quit(&self, behavior: QuitBehavior) {
        self.shared.with(|s| s.quit = Some(behavior));
    }

    /// Makes the current runner return (the connection driver died).
    pub fn runner_exit(&self) {
        self.runner_cmd(RunnerCmd::Exit);
    }

    /// Makes the current runner panic (a connection-task panic re-raised).
    pub fn runner_panic(&self) {
        self.runner_cmd(RunnerCmd::Panic);
    }

    fn runner_cmd(&self, cmd: RunnerCmd) {
        if let Some(tx) = self.shared.with(|s| s.runner.clone()) {
            let _ = tx.try_send(cmd);
        }
    }

    /// Delivers a live update on whichever stream is current (or the next one).
    pub async fn push_update(&self, update: Update) {
        let _ = self.stream.send(StreamCmd::Update(update)).await;
    }

    /// The current stream's next `next` call fails with `e`.
    pub async fn stream_error(&self, e: StreamError) {
        let _ = self.stream.send(StreamCmd::Error(e)).await;
    }

    /// The current stream's next `next` call panics.
    pub async fn stream_panic(&self) {
        let _ = self.stream.send(StreamCmd::Panic).await;
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

    /// Number of builds attempted (including failed ones).
    pub fn builds(&self) -> usize {
        self.shared.with(|s| s.seeds.len())
    }

    /// The seed of every build, in order.
    pub fn seeds(&self) -> Vec<Seed> {
        self.shared.with(|s| s.seeds.clone())
    }

    /// Builds that loaded persisted state (process startup only).
    pub fn stored_loads(&self) -> usize {
        self.shared.with(|s| {
            s.seeds
                .iter()
                .filter(|seed| seed.session == SessionSeed::Stored)
                .count()
        })
    }

    /// Runners that completed or were cancelled.
    pub fn runners_finished(&self) -> usize {
        self.shared.runners_finished.load(Ordering::SeqCst)
    }

    /// Instances whose control was retired.
    pub fn retired(&self) -> usize {
        self.shared.retired.load(Ordering::SeqCst)
    }

    /// `(chat, random_id)` of every send that reached the backend and succeeded.
    pub fn sent(&self) -> Vec<(ChatId, i64)> {
        self.shared.with(|s| s.sent.clone())
    }

    /// `(chat, random_id)` of every send call, including failed or hung ones.
    pub fn send_attempts(&self) -> Vec<(ChatId, i64)> {
        self.shared.with(|s| s.attempted.clone())
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

impl Backend for FakeBackend {
    type Client = FakeClient;
    type Updates = FakeUpdates;
    type Control = FakeControl;

    fn build(
        &self,
        seed: Seed,
    ) -> impl Future<Output = Result<Instance<Self>, BackendError>> + Send {
        std::future::ready(self.build_now(seed))
    }
}

impl FakeBackend {
    fn build_now(&self, seed: Seed) -> Result<Instance<Self>, BackendError> {
        let rev = match &seed.session {
            SessionSeed::Stored => 0,
            SessionSeed::Live(snapshot) => snapshot.rev(),
        };
        let (runner_tx, runner_rx) = mpsc::channel(4);
        let fault = self.shared.with(|s| {
            push_capped(&mut s.seeds, seed);
            s.runner = Some(runner_tx);
            s.build_faults.pop_front()
        });
        let baseline = match fault {
            Some(BuildFault::Fail(e)) => return Err(e),
            Some(BuildFault::NoBaseline) => false,
            None => true,
        };
        let quit = Arc::new(Notify::new());
        Ok(Instance {
            client: FakeClient {
                shared: Arc::clone(&self.shared),
            },
            updates: FakeUpdates {
                stream: Arc::clone(&self.stream),
                baseline,
            },
            runner: Box::pin(run_runner(
                Arc::clone(&self.shared),
                Arc::clone(&quit),
                runner_rx,
            )),
            control: FakeControl {
                shared: Arc::clone(&self.shared),
                quit,
                rev,
            },
        })
    }
}

async fn run_runner(shared: Arc<Shared>, quit: Arc<Notify>, mut cmds: mpsc::Receiver<RunnerCmd>) {
    let _guard = RunnerGuard(Arc::clone(&shared));
    tokio::select! {
        () = quit.notified() => {
            match shared.with(|s| s.quit).unwrap_or(QuitBehavior::Prompt) {
                QuitBehavior::Prompt => {}
                QuitBehavior::Slow(d) => tokio::time::sleep(d).await,
                QuitBehavior::Ignore => std::future::pending().await,
            }
        }
        cmd = cmds.recv() => match cmd {
            Some(RunnerCmd::Exit) => {}
            Some(RunnerCmd::Panic) => injected_panic("runner"),
            // Replaced by a newer build; keep running until quit.
            None => quit.notified().await,
        },
    }
}

/// Retirement hooks of one fake instance.
#[derive(Debug)]
pub struct FakeControl {
    shared: Arc<Shared>,
    quit: Arc<Notify>,
    rev: u64,
}

impl InstanceControl for FakeControl {
    fn quit(&self) {
        self.quit.notify_one();
    }

    fn retire(self) -> SessionSnapshot {
        self.shared.retired.fetch_add(1, Ordering::SeqCst);
        // Simulates a critical write (e.g. a DC option) during the instance's life.
        SessionSnapshot::new(self.rev.saturating_add(1))
    }
}

/// The update stream of one fake instance.
#[derive(Debug)]
pub struct FakeUpdates {
    stream: Arc<tokio::sync::Mutex<mpsc::Receiver<StreamCmd>>>,
    /// The baseline is still to be reported.
    baseline: bool,
}

impl UpdateStream for FakeUpdates {
    async fn next(&mut self) -> Result<StreamItem, StreamError> {
        if std::mem::take(&mut self.baseline) {
            return Ok(StreamItem::Baseline);
        }
        let mut rx = self.stream.lock().await;
        match rx.recv().await {
            Some(StreamCmd::Update(u)) => Ok(StreamItem::Update(u)),
            Some(StreamCmd::Error(e)) => Err(e),
            Some(StreamCmd::Panic) => injected_panic("update stream"),
            None => Err(StreamError::Fatal),
        }
    }
}

/// Calls of one fake instance, served by the shared fake server.
#[derive(Debug)]
pub struct FakeClient {
    shared: Arc<Shared>,
}

impl FakeClient {
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
            Some(Fault::Delay(d)) => {
                tokio::time::sleep(d).await;
                Ok(guard)
            }
            Some(Fault::Error(e)) => Err(e),
            Some(Fault::Hang) => std::future::pending().await,
            Some(Fault::Panic) => injected_panic("call"),
        }
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

impl Client for FakeClient {
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
        self.shared
            .with(|s| push_capped(&mut s.attempted, (chat, random_id)));
        let _g = self.enter(Op::Send).await?;
        // The real server also echoes own sends as updates; the fake does not, so tests
        // control update ordering explicitly with `FakeHandle::push_update`.
        let id = self.shared.with(|s| {
            push_capped(&mut s.sent, (chat, random_id));
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
        self.shared
            .with(|s| push_capped(&mut s.read, (chat, max_id)));
        Ok(())
    }
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
