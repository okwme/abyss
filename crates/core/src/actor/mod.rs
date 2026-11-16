//! The core actor: owns the store and the client lifecycle, admits backend calls,
//! applies completions and live updates, and emits events. Runs on the core
//! runtime; never blocks.

mod admission;
mod apply;
mod auth;
mod auth_outcome;
mod boot;
mod jobs;
mod lifecycle;
mod logout;
mod merge;
mod onboarding;
mod peers;
mod sends;
#[cfg(all(test, feature = "fake"))]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use admission::Admission;
pub use admission::MAX_CONCURRENT;
use auth::AuthMachine;
use boot::Boot;
use jobs::{CallCtx, Done, Job, Load, ReqId, TaskKind};
use lifecycle::{Lifecycle, Live, LiveEvent, Phase, Reason};
pub(crate) use logout::{CacheParts, open_cache};

use crate::api::{
    AuthState, ClientGen, Command, ConnState, Event, HistoryDir, RemoteLogout, SaveStatus,
    UserError, WindowState,
};
use crate::backend::{Backend, DialogsCursor, HistoryQuery, StreamItem};
use crate::cache::CacheHandle;
use crate::core::ShutdownReport;
use crate::events::EventSender;
use crate::model::{ChatId, MsgId};
use crate::session::{PersistHandle, SessionSnapshot};
use crate::store::{Dialogs, MAX_DIALOGS, ViewGen, Window, Windows};

/// Shutdown's deadline for retirement and the persistence seal, counted from
/// its start (plan section 5).
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(3);
/// How long shutdown waits for queued cache writes (best effort; the cache
/// is disposable and does not affect the save status).
const CACHE_FLUSH: Duration = Duration::from_secs(3);

/// Per-call deadline for backend calls.
pub const CALL_DEADLINE: Duration = Duration::from_secs(30);
/// Coalescing interval for read marks.
pub const READ_FLUSH: Duration = Duration::from_millis(500);
/// Admitted-call backlog waiting for a permit.
const MAX_QUEUED: usize = 256;
/// Stream items buffered between the update consumer and the actor.
const UPDATE_QUEUE: usize = 256;

/// Control-plane messages, never queued behind UI commands.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Internal {
    /// The system woke from sleep: rebuild rather than trust old sockets.
    Wake,
}

pub(crate) struct Actor<B: Backend> {
    backend: Arc<B>,
    events: EventSender,
    /// Shutdown. Also interrupts an `emit` blocked on a full event queue.
    cancel: CancellationToken,
    cache: Option<CacheHandle>,
    dialogs: Dialogs,
    windows: Windows,
    cgen: ClientGen,
    admission: Admission,
    /// UI-initiated calls; admitted only while live.
    queue: VecDeque<Job>,
    /// Bootstrap and refresh snapshots; admitted whenever an instance exists.
    priority: VecDeque<Job>,
    /// Cache reads, which outlive client instances.
    tasks: JoinSet<Done>,
    kinds: HashMap<Id, TaskKind>,
    live: Option<Live<B>>,
    life: Lifecycle<B>,
    boot: Option<Boot>,
    window: WindowState,
    last_refresh: Instant,
    /// Chats with a history load (cache or server) in flight, and that load.
    /// At most one per window chat.
    history_busy: HashMap<ChatId, Load>,
    /// Source of [`jobs::ReqId`]s.
    next_req: u64,
    dialogs_busy: bool,
    dialogs_queued: bool,
    dialogs_loaded: bool,
    dialogs_next: Option<DialogsCursor>,
    open: Option<ChatId>,
    /// The chat whose live tail the UI reports on screen
    /// (`Command::ViewingTail`).
    viewing: Option<ChatId>,
    /// The peer set last handed to the instance (`Client::retain_peers`).
    peers: peers::PeerSync,
    /// Highest displayed id per chat, flushed every `READ_FLUSH`.
    reads: HashMap<ChatId, MsgId>,
    sends: sends::PendingSends,
    deadline: Duration,
    updates_tx: mpsc::Sender<(ClientGen, StreamItem)>,
    persist: PersistHandle,
    /// The account's live critical state; a new generation after logout.
    account: SessionSnapshot,
    /// The cache's directory and key, for crypto-erase on logout.
    cache_parts: Option<CacheParts>,
    auth: AuthMachine<B::Client>,
    /// The remote half of a running logout, until it is reported.
    logout_remote: Option<RemoteLogout>,
}

pub(crate) struct ActorChannels {
    pub commands: mpsc::Receiver<Command>,
    pub internal: mpsc::Receiver<Internal>,
    pub updates: mpsc::Receiver<(ClientGen, StreamItem)>,
}

/// Inputs of [`Actor::new`].
pub(crate) struct ActorInit<B> {
    pub backend: Arc<B>,
    pub events: EventSender,
    pub cache: Option<CacheHandle>,
    pub commands: mpsc::Receiver<Command>,
    pub internal: mpsc::Receiver<Internal>,
    pub cancel: CancellationToken,
    pub persist: PersistHandle,
    /// Loaded at startup (`PersistHandle::load_session`).
    pub account: SessionSnapshot,
    pub cache_parts: Option<CacheParts>,
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl<B: Backend> Actor<B> {
    pub(crate) fn new(init: ActorInit<B>) -> (Self, ActorChannels) {
        let (updates_tx, updates) = mpsc::channel(UPDATE_QUEUE);
        init.backend.use_account(init.account.clone());
        let auth = AuthMachine::new(&init.account);
        let actor = Actor {
            backend: init.backend,
            events: init.events,
            cancel: init.cancel,
            cache: init.cache,
            dialogs: Dialogs::default(),
            windows: Windows::default(),
            cgen: ClientGen(1),
            admission: Admission::default(),
            queue: VecDeque::new(),
            priority: VecDeque::new(),
            tasks: JoinSet::new(),
            kinds: HashMap::new(),
            live: None,
            life: Lifecycle::default(),
            boot: None,
            window: WindowState::Active,
            last_refresh: Instant::now(),
            history_busy: HashMap::new(),
            next_req: 0,
            dialogs_busy: false,
            dialogs_queued: false,
            dialogs_loaded: false,
            dialogs_next: None,
            open: None,
            viewing: None,
            peers: peers::PeerSync::default(),
            reads: HashMap::new(),
            sends: sends::PendingSends::default(),
            deadline: CALL_DEADLINE,
            updates_tx,
            persist: init.persist,
            account: init.account,
            cache_parts: init.cache_parts,
            auth,
            logout_remote: None,
        };
        let channels = ActorChannels {
            commands: init.commands,
            internal: init.internal,
            updates,
        };
        (actor, channels)
    }

    /// Runs until cancelled or the command channel closes, then shuts down
    /// (plan section 5) and reports what it observed.
    pub(crate) async fn run(mut self, mut ch: ActorChannels) -> ShutdownReport {
        let cancel = self.cancel.clone();
        self.start().await;
        let mut flush = tokio::time::interval(READ_FLUSH);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let baseline_by = self.boot.as_ref().and_then(Boot::baseline_deadline);
            let refresh_at = self.refresh_due();
            let flood_over = self.auth.flood_deadline();
            let send_due = self.sends.next_due();
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                Some(msg) = ch.internal.recv() => self.on_internal(msg).await,
                step = lifecycle::next_step(self.life.step_mut()) => self.on_step(step).await,
                Some(msg) = self.auth.rx.recv() => self.on_auth(msg).await,
                ev = lifecycle::next_live(&mut self.live) => self.on_live(ev).await,
                Some(joined) = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    self.on_joined(joined).await;
                }
                Some((cgen, item)) = ch.updates.recv() => {
                    if cgen == self.cgen {
                        self.on_feed(item).await;
                    }
                }
                cmd = ch.commands.recv() => match cmd {
                    Some(cmd) => self.on_command(cmd).await,
                    None => break,
                },
                () = sleep_until(baseline_by) => self.rebuild(Reason::NoBaseline).await,
                () = sleep_until(refresh_at) => self.refresh().await,
                () = sleep_until(flood_over) => self.flood_over().await,
                () = sleep_until(send_due) => self.resume_flooded_sends().await,
                _ = flush.tick() => self.flush_reads().await,
            }
            self.pump();
            self.sync_peers();
        }
        self.shut_down().await
    }

    /// Plan section 5, steps 2-6: end the actor's tasks, retire the instance
    /// (finishing a running retirement or logout), then seal persistence with
    /// a barrier on the current revision. Never claims a save it did not see.
    async fn shut_down(&mut self) -> ShutdownReport {
        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        self.auth.abort_task();
        self.auth.creds.abort();
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
        let life = self.shut_down_lifecycle(deadline).await;
        // After a logout there is nothing to save; its status says the rest.
        let sealed = life.logout.is_some()
            || matches!(
                tokio::time::timeout_at(deadline, self.account.barrier()).await,
                Ok(Ok(()))
            );
        if let Some(cache) = &self.cache {
            let _ = tokio::time::timeout(CACHE_FLUSH, cache.barrier()).await;
        }
        let save = if life.retired && sealed {
            SaveStatus::Confirmed
        } else {
            SaveStatus::Unconfirmed
        };
        ShutdownReport {
            save,
            logout: life.logout,
        }
    }

    async fn start(&mut self) {
        self.set_auth(AuthState::Connecting).await;
        self.emit(Event::Connection(ConnState::Connecting)).await;
        if let Some(cache) = self.cache.clone() {
            let cgen = self.cgen;
            self.spawn_local(TaskKind::CacheChats, async move {
                let chats = cache.chats().await.ok();
                Done {
                    cgen,
                    parked: None,
                    outcome: jobs::Outcome::CacheChats(chats),
                }
            });
        }
        self.resolve_credentials().await;
        self.start_build(Duration::ZERO);
    }

    /// Spawns a task that is not bound to a client instance (cache reads).
    fn spawn_local(
        &mut self,
        kind: TaskKind,
        fut: impl std::future::Future<Output = Done> + Send + 'static,
    ) {
        let handle = self.tasks.spawn(fut);
        self.kinds.insert(handle.id(), kind);
    }

    /// Emits an event, waiting for queue space; gives up on shutdown so a UI
    /// that stopped draining cannot block cancellation.
    async fn emit(&self, event: Event) {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => {}
            // A closed channel means the UI is gone; shutdown follows via cancellation.
            _ = self.events.send(event) => {}
        }
    }

    async fn error(&self, e: UserError) {
        self.emit(Event::Error(e)).await;
    }

    /// Queues a backend call; it is spawned when admitted and a permit is free.
    async fn enqueue(&mut self, job: Job) -> bool {
        if self.queue.len() >= MAX_QUEUED {
            self.error(UserError::Busy).await;
            return false;
        }
        self.queue.push_back(job);
        true
    }

    /// Spawns queued jobs on the live instance while permits are available.
    /// Snapshot jobs go first; UI jobs wait until the bootstrap has finished.
    fn pump(&mut self) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let user = self.life.phase == Phase::Live;
        loop {
            let queue = if !self.priority.is_empty() {
                &mut self.priority
            } else if user && !self.queue.is_empty() {
                &mut self.queue
            } else {
                return;
            };
            let Some(permit) = self.admission.try_acquire() else {
                return;
            };
            let Some(job) = queue.pop_front() else {
                return;
            };
            let ctx = CallCtx {
                client: Arc::clone(&live.client),
                cgen: self.cgen,
                deadline: self.deadline,
                cancel: live.cancel.clone(),
            };
            let (kind, fut) = jobs::job_future(ctx, job, permit);
            let handle = live.tasks.spawn(fut);
            live.kinds.insert(handle.id(), kind);
        }
    }

    /// Drops queued snapshot and history loads (their views are going away) and
    /// dialogs loads; sends and read marks stay queued for the next instance.
    fn purge_queued(&mut self) {
        let keep = |job: &Job| matches!(job, Job::Send { .. } | Job::Read { .. });
        let (kept, purged): (Vec<Job>, Vec<Job>) = self.queue.drain(..).partition(keep);
        self.queue = kept.into();
        let purged: Vec<Job> = self.priority.drain(..).chain(purged).collect();
        for job in purged {
            match job {
                Job::Dialogs { .. } => self.dialogs_busy = false,
                Job::History { load, .. } => self.release_history(load),
                Job::Send { .. } | Job::Read { .. } => {}
            }
        }
    }

    async fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Wake => self.rebuild(Reason::Wake).await,
        }
    }

    async fn on_live(&mut self, ev: LiveEvent) {
        match ev {
            LiveEvent::Joined(Ok((id, done))) => {
                if let Some(live) = self.live.as_mut() {
                    live.kinds.remove(&id);
                }
                self.on_done(done).await;
            }
            LiveEvent::Joined(Err(e)) => {
                let kind = self.live.as_mut().and_then(|l| l.kinds.remove(&e.id()));
                if e.is_panic() {
                    self.on_panic(kind).await;
                }
            }
            LiveEvent::RunnerEnded => self.rebuild(Reason::RunnerEnded).await,
        }
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
            Some(TaskKind::Dialogs) => {
                self.dialogs_busy = false;
                self.snapshot_done(boot::Snapshot::Dialogs).await;
            }
            Some(TaskKind::History(load)) => {
                self.release_history(load);
                self.snapshot_done(boot::Snapshot::Tail(load)).await;
            }
            Some(TaskKind::Send(local_id)) => {
                if let Some(state) = self.sends.mark_unknown(local_id) {
                    self.emit(Event::SendState { local_id, state }).await;
                }
            }
            Some(TaskKind::Updates) => {
                self.error(UserError::Internal).await;
                return self.rebuild(Reason::StreamFailed).await;
            }
            Some(TaskKind::Auth(tag)) => return self.on_auth_panic(tag).await,
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
            Command::DiscardSend(local_id) => self.discard_send(local_id).await,
            Command::Window(state) => self.on_window(state).await,
            Command::MarkRead { chat, max_id } => self.mark_read(chat, max_id).await,
            Command::ViewingTail(chat) => self.viewing = chat,
            Command::SubmitPhone(phone) => {
                let phone = zeroize::Zeroizing::new(phone);
                self.submit_phone(&phone).await;
            }
            Command::SubmitCode(code) => self.submit_code(zeroize::Zeroizing::new(code)).await,
            Command::SubmitPassword(password) => self.submit_password(password).await,
            Command::ResendCode => self.resend_code().await,
            Command::RetryPasswordParams => self.retry_password_params().await,
            Command::Logout => self.on_logout_command().await,
            Command::SubmitCredentials { api_id, api_hash } => {
                self.submit_credentials(api_id, api_hash).await;
            }
            Command::ForgetStoredCredentials => self.forget_credentials().await,
            Command::ReloadCredentials => self.reload_credentials().await,
            Command::RetryCredentialSave => self.retry_credential_save(),
            Command::Back => self.back().await,
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

    /// Opens `chat`: the retained window (if attached to the live tail) is
    /// shown at once as a complete `Tail` snapshot of the new view, then the
    /// cached and server tails follow. During a full bootstrap the tail is a
    /// boot snapshot, replacing any earlier one of the same chat.
    async fn open_chat(&mut self, chat: ChatId) {
        if let Some(prev) = self.open.filter(|p| *p != chat) {
            self.windows.close(prev);
        }
        // A detached window is not what the user asked for: start from the tail.
        if self.windows.get(chat).is_some_and(Window::has_newer) {
            self.windows.invalidate(chat);
        }
        let view = self.windows.open(chat);
        self.open = Some(chat);
        self.seed_view(chat, view).await;
        let load = self.begin_load(chat, view);
        self.claim_if_booting(load);
        if let Some(cache) = &self.cache {
            cache.write(crate::cache::CacheCmd::Touch(chat)).await;
        }
        // The server tail is requested once the cache tail is shown.
        self.start_tail(load, boot::TailVia::Cache).await;
    }

    /// Emits the window `chat` retains as a complete `Tail` of `view`, so a
    /// newly opened view never starts empty while its tail loads.
    async fn seed_view(&self, chat: ChatId, view: ViewGen) {
        let Some(w) = self.windows.get(chat).filter(|w| !w.messages().is_empty()) else {
            return;
        };
        self.emit(Event::History {
            chat,
            view,
            dir: HistoryDir::Tail,
            messages: w.messages().iter().cloned().collect(),
            removed: Vec::new(),
            has_more: w.has_older(),
        })
        .await;
    }

    /// Starts a history load for `chat`'s `view`, making it the chat's
    /// single-flight load.
    fn begin_load(&mut self, chat: ChatId, view: ViewGen) -> Load {
        self.next_req = self.next_req.wrapping_add(1);
        let load = Load {
            chat,
            view,
            req: ReqId(self.next_req),
        };
        self.history_busy.insert(chat, load);
        load
    }

    /// The newest id `chat`'s window holds now: what a tail requested now
    /// is authoritative up to.
    fn tail_floor(&self, chat: ChatId) -> Option<MsgId> {
        self.windows
            .get(chat)
            .and_then(|w| w.messages().back())
            .map(|m| m.id)
    }

    async fn request_tail(&mut self, load: Load) {
        let job = Job::History {
            load,
            dir: HistoryDir::Tail,
            query: HistoryQuery::Tail,
            floor: self.tail_floor(load.chat),
        };
        if !self.enqueue(job).await {
            self.release_history(load);
        }
    }

    fn close_chat(&mut self, chat: ChatId) {
        self.windows.close(chat);
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
        let load = self.begin_load(chat, view);
        if let Some(cache) = self
            .cache
            .clone()
            .filter(|_| self.windows.is_cache_contiguous(chat))
        {
            let cgen = self.cgen;
            self.spawn_local(
                TaskKind::History(load),
                jobs::cache_history(cache, cgen, load, dir, Some(edge)),
            );
            return;
        }
        self.fetch_more(load, dir, edge).await;
    }

    async fn fetch_more(&mut self, load: Load, dir: HistoryDir, edge: MsgId) {
        let query = if dir == HistoryDir::Older {
            HistoryQuery::Before(edge)
        } else {
            HistoryQuery::After(edge)
        };
        let job = Job::History {
            load,
            dir,
            query,
            floor: None,
        };
        if !self.enqueue(job).await {
            self.release_history(load);
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

    /// Queues coalesced read marks. While no instance is live they keep
    /// coalescing instead of piling up in the queue.
    async fn flush_reads(&mut self) {
        if self.life.phase != Phase::Live {
            return;
        }
        let reads: Vec<(ChatId, MsgId)> = self.reads.drain().collect();
        for (chat, max_id) in reads {
            self.enqueue(Job::Read { chat, max_id }).await;
        }
    }
}
