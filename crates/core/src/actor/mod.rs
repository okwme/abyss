//! The core actor: owns the store, admits backend calls, applies completions and
//! live updates, and emits events. Runs on the core runtime; never blocks.

mod admission;
mod apply;
mod jobs;
mod sends;
#[cfg(all(test, feature = "fake"))]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{Id, JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use admission::Admission;
pub use admission::MAX_CONCURRENT;
use jobs::{Done, Job, TaskKind};

use crate::api::{ClientGen, Command, ConnState, Event, HistoryDir, UserError};
use crate::backend::{Backend, DialogsCursor, HistoryQuery, Update};
use crate::cache::CacheHandle;
use crate::events::EventSender;
use crate::model::{ChatId, MsgId};
use crate::store::{Dialogs, MAX_DIALOGS, ViewGen, Windows};

/// Per-call deadline for backend calls.
pub const CALL_DEADLINE: Duration = Duration::from_secs(30);
/// Coalescing interval for read marks.
pub const READ_FLUSH: Duration = Duration::from_millis(500);
/// Admitted-call backlog waiting for a permit.
const MAX_QUEUED: usize = 256;
/// Live updates buffered between the consumer task and the actor.
const UPDATE_QUEUE: usize = 256;

/// Messages from the supervisor (task 4 drives rebuilds through this).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Internal {
    /// The client instance has fully retired: bump the generation, drop stale
    /// completions, and recycle parked permits.
    #[cfg_attr(
        not(all(test, feature = "fake")),
        expect(
            dead_code,
            reason = "task 4 sends this once a client instance has retired"
        )
    )]
    Retire,
}

pub(crate) struct Actor<B: Backend> {
    backend: Arc<B>,
    events: EventSender,
    cache: Option<CacheHandle>,
    dialogs: Dialogs,
    windows: Windows,
    cgen: ClientGen,
    admission: Admission,
    queue: VecDeque<Job>,
    tasks: JoinSet<Done>,
    kinds: HashMap<Id, TaskKind>,
    /// Chats with a history load (cache or server) in flight, and its view.
    history_busy: HashMap<ChatId, ViewGen>,
    dialogs_busy: bool,
    dialogs_queued: bool,
    dialogs_loaded: bool,
    dialogs_next: Option<DialogsCursor>,
    open: Option<ChatId>,
    /// Chats whose disk cache is known to be contiguous with the live tail this
    /// session, so older/newer pages may be served from it.
    contiguous: HashSet<ChatId>,
    /// Highest displayed id per chat, flushed every `READ_FLUSH`.
    reads: HashMap<ChatId, MsgId>,
    sends: sends::PendingSends,
    deadline: Duration,
    updates_tx: mpsc::Sender<(ClientGen, Update)>,
}

pub(crate) struct ActorChannels {
    pub commands: mpsc::Receiver<Command>,
    pub internal: mpsc::Receiver<Internal>,
    pub updates: mpsc::Receiver<(ClientGen, Update)>,
}

impl<B: Backend> Actor<B> {
    pub(crate) fn new(
        backend: Arc<B>,
        events: EventSender,
        cache: Option<CacheHandle>,
        commands: mpsc::Receiver<Command>,
        internal: mpsc::Receiver<Internal>,
    ) -> (Self, ActorChannels) {
        let (updates_tx, updates) = mpsc::channel(UPDATE_QUEUE);
        let actor = Actor {
            backend,
            events,
            cache,
            dialogs: Dialogs::default(),
            windows: Windows::default(),
            cgen: ClientGen(1),
            admission: Admission::default(),
            queue: VecDeque::new(),
            tasks: JoinSet::new(),
            kinds: HashMap::new(),
            history_busy: HashMap::new(),
            dialogs_busy: false,
            dialogs_queued: false,
            dialogs_loaded: false,
            dialogs_next: None,
            open: None,
            contiguous: HashSet::new(),
            reads: HashMap::new(),
            sends: sends::PendingSends::default(),
            deadline: CALL_DEADLINE,
            updates_tx,
        };
        (
            actor,
            ActorChannels {
                commands,
                internal,
                updates,
            },
        )
    }

    /// Runs until cancelled or the command channel closes, then aborts and joins
    /// every task it spawned.
    pub(crate) async fn run(mut self, mut ch: ActorChannels, cancel: CancellationToken) {
        self.start().await;
        let mut flush = tokio::time::interval(READ_FLUSH);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                Some(msg) = ch.internal.recv() => self.on_internal(msg),
                Some(joined) = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    self.on_joined(joined).await;
                }
                Some((cgen, update)) = ch.updates.recv() => {
                    if cgen == self.cgen {
                        self.on_update(update).await;
                    }
                }
                cmd = ch.commands.recv() => match cmd {
                    Some(cmd) => self.on_command(cmd).await,
                    None => break,
                },
                _ = flush.tick() => self.flush_reads().await,
            }
            self.pump();
        }
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }

    async fn start(&mut self) {
        self.emit(Event::Connection(ConnState::Online)).await;
        if let Some(cache) = self.cache.clone() {
            let cgen = self.cgen;
            self.spawn(TaskKind::CacheChats, async move {
                let chats = cache.chats().await.ok();
                Done {
                    cgen,
                    parked: None,
                    outcome: jobs::Outcome::CacheChats(chats),
                }
            });
        }
        let (backend, cgen, tx) = (
            Arc::clone(&self.backend),
            self.cgen,
            self.updates_tx.clone(),
        );
        self.spawn(TaskKind::Updates, jobs::consume_updates(backend, cgen, tx));
        self.load_dialogs().await;
    }

    fn spawn(
        &mut self,
        kind: TaskKind,
        fut: impl std::future::Future<Output = Done> + Send + 'static,
    ) {
        let handle = self.tasks.spawn(fut);
        self.kinds.insert(handle.id(), kind);
    }

    async fn emit(&self, event: Event) {
        // A closed channel means the UI is gone; shutdown follows via cancellation.
        let _ = self.events.send(event).await;
    }

    async fn error(&self, e: UserError) {
        self.emit(Event::Error(e)).await;
    }

    /// Queues a backend call; it is spawned when a permit is free.
    async fn enqueue(&mut self, job: Job) -> bool {
        if self.queue.len() >= MAX_QUEUED {
            self.error(UserError::Busy).await;
            return false;
        }
        self.queue.push_back(job);
        true
    }

    /// Spawns queued jobs while permits are available.
    fn pump(&mut self) {
        while !self.queue.is_empty() {
            let Some(permit) = self.admission.try_acquire() else {
                return;
            };
            let Some(job) = self.queue.pop_front() else {
                return;
            };
            let (kind, fut) = jobs::job_future(
                Arc::clone(&self.backend),
                job,
                self.cgen,
                self.deadline,
                permit,
            );
            self.spawn(kind, fut);
        }
    }

    fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Retire => self.retire(),
        }
    }

    /// See [`Internal::Retire`]. Task 4 performs teardown before sending it.
    fn retire(&mut self) {
        self.cgen = ClientGen(self.cgen.0 + 1);
        self.admission.release_parked();
        self.history_busy.clear();
        self.dialogs_busy = false;
        self.dialogs_queued = false;
    }

    async fn on_joined(&mut self, joined: Result<(Id, Done), JoinError>) {
        match joined {
            Ok((id, done)) => {
                self.kinds.remove(&id);
                self.on_done(done).await;
            }
            Err(e) => {
                let kind = self.kinds.remove(&e.id());
                if e.is_panic() {
                    self.on_panic(kind).await;
                }
            }
        }
    }

    /// A task panicked (for example inside the Telegram library). Its permit was
    /// released by unwinding; clear the state it held and keep running.
    async fn on_panic(&mut self, kind: Option<TaskKind>) {
        match kind {
            Some(TaskKind::Dialogs) => self.dialogs_busy = false,
            Some(TaskKind::History(chat)) => {
                self.history_busy.remove(&chat);
            }
            Some(TaskKind::Send(local_id)) => {
                if let Some(state) = self.sends.mark_unknown(local_id) {
                    self.emit(Event::SendState { local_id, state }).await;
                }
            }
            _ => {}
        }
        self.error(UserError::Internal).await;
    }

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::LoadDialogs => self.load_dialogs().await,
            Command::OpenChat(chat) => self.open_chat(chat).await,
            Command::CloseChat(chat) => self.close_chat(chat),
            Command::LoadOlder(chat) => self.load_more(chat, HistoryDir::Older).await,
            Command::LoadNewer(chat) => self.load_more(chat, HistoryDir::Newer).await,
            Command::SendText {
                chat,
                text,
                local_id,
            } => self.send_text(chat, text, local_id).await,
            Command::RetrySend(local_id) => self.retry_send(local_id).await,
            Command::MarkRead { chat, max_id } => self.mark_read(chat, max_id).await,
            Command::SubmitPhone(_)
            | Command::SubmitCode(_)
            | Command::SubmitPassword(_)
            | Command::ResendCode
            | Command::RetryPasswordParams
            | Command::Logout => self.error(UserError::Unsupported).await,
        }
    }

    async fn load_dialogs(&mut self) {
        if self.dialogs_busy {
            self.dialogs_queued = true;
            return;
        }
        if self.dialogs_loaded && self.dialogs_next.is_none() {
            return; // every page is loaded
        }
        let cursor = if self.dialogs_loaded {
            self.dialogs_next.clone()
        } else {
            None
        };
        if self.enqueue(Job::Dialogs { cursor }).await {
            self.dialogs_busy = true;
        }
    }

    async fn open_chat(&mut self, chat: ChatId) {
        if let Some(prev) = self.open.filter(|p| *p != chat) {
            self.windows.close(prev);
        }
        let view = self.windows.open(chat);
        self.open = Some(chat);
        self.contiguous.remove(&chat);
        self.history_busy.insert(chat, view);
        match self.cache.clone() {
            // The server tail is requested once the cache tail is shown.
            Some(cache) => {
                cache.write(crate::cache::CacheCmd::Touch(chat)).await;
                let cgen = self.cgen;
                let fut = jobs::cache_history(cache, cgen, chat, view, HistoryDir::Tail, None);
                self.spawn(TaskKind::History(chat), fut);
            }
            None => self.request_tail(chat, view).await,
        }
    }

    async fn request_tail(&mut self, chat: ChatId, view: ViewGen) {
        let job = Job::History {
            chat,
            view,
            dir: HistoryDir::Tail,
            query: HistoryQuery::Tail,
        };
        if !self.enqueue(job).await {
            self.release_history(chat, view);
        }
    }

    fn close_chat(&mut self, chat: ChatId) {
        self.windows.close(chat);
        self.contiguous.remove(&chat);
        if self.open == Some(chat) {
            self.open = None;
        }
    }

    /// Scrolls the open window. Serves from the disk cache when it is known to be
    /// contiguous, else asks the server. Single-flight per chat.
    async fn load_more(&mut self, chat: ChatId, dir: HistoryDir) {
        if self.open != Some(chat) || self.history_busy.contains_key(&chat) {
            return;
        }
        let Some(w) = self.windows.get(chat) else {
            return;
        };
        let view = w.view_gen();
        let (edge, more) = match dir {
            HistoryDir::Older => (w.messages().front(), w.has_older()),
            _ => (w.messages().back(), w.has_newer()),
        };
        let Some(edge) = edge.filter(|_| more).map(|m| m.id) else {
            return;
        };
        self.history_busy.insert(chat, view);
        if let Some(cache) = self
            .cache
            .clone()
            .filter(|_| self.contiguous.contains(&chat))
        {
            let cgen = self.cgen;
            self.spawn(
                TaskKind::History(chat),
                jobs::cache_history(cache, cgen, chat, view, dir, Some(edge)),
            );
            return;
        }
        self.fetch_more(chat, view, dir, edge).await;
    }

    async fn fetch_more(&mut self, chat: ChatId, view: ViewGen, dir: HistoryDir, edge: MsgId) {
        let query = if dir == HistoryDir::Older {
            HistoryQuery::Before(edge)
        } else {
            HistoryQuery::After(edge)
        };
        if !self
            .enqueue(Job::History {
                chat,
                view,
                dir,
                query,
            })
            .await
        {
            self.release_history(chat, view);
        }
    }

    async fn mark_read(&mut self, chat: ChatId, max_id: MsgId) {
        if self.reads.len() >= MAX_DIALOGS && !self.reads.contains_key(&chat) {
            return;
        }
        let entry = self.reads.entry(chat).or_insert(max_id);
        *entry = (*entry).max(max_id);
        if let Some(summary) = self.dialogs.set_unread(chat, 0).cloned() {
            self.emit(Event::ChatBumped(summary)).await;
        }
    }

    async fn flush_reads(&mut self) {
        let reads: Vec<(ChatId, MsgId)> = self.reads.drain().collect();
        for (chat, max_id) in reads {
            self.enqueue(Job::Read { chat, max_id }).await;
        }
    }
}
