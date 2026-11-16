//! Client lifecycle (plan section 2): build, supervise, retire, rebuild.
//!
//! The actor owns at most one live client instance. Every failure is handled the
//! same way: stop admission and bump `ClientGen`, retire the instance (cancel and
//! join its tasks, stop its runner and await its completion, revoke its session),
//! and only then recycle parked permits and build again with backoff. Build and
//! retirement run as one spawned "step" at a time, so the actor keeps serving
//! commands, the cache and the UI meanwhile.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::boot::Boot;
use super::jobs::{self, CallError, Done, Outcome, TaskKind};
use super::{Actor, admission::MAX_CONCURRENT};
use crate::api::{ClientGen, ConnState, Event};
use crate::backend::{
    Backend, BackendError, CarriedPeers, Instance, InstanceControl, Seed, SessionSeed,
    SessionSnapshot,
};

/// First rebuild delay; doubles per attempt.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Online this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_mins(5);
/// A stall: this many deadline misses within [`STALL_WINDOW`].
const STALL_TIMEOUTS: usize = 3;
const STALL_WINDOW: Duration = Duration::from_mins(2);

/// Time limits for retiring an instance.
#[derive(Clone, Copy, Debug)]
pub(super) struct Caps {
    /// For the instance's tasks to finish after cancellation, before abort.
    tasks: Duration,
    /// For the runner to finish after `quit`, before abort.
    runner: Duration,
}

pub(super) const REBUILD_CAPS: Caps = Caps {
    tasks: Duration::from_secs(3),
    runner: Duration::from_secs(5),
};
pub(super) const SHUTDOWN_CAPS: Caps = Caps {
    tasks: Duration::from_secs(1),
    runner: Duration::from_secs(2),
};

/// Why an instance is being replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    RunnerEnded,
    StreamFailed,
    Stall,
    Wake,
    Migrated,
    NoBaseline,
    BootOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// A build step is running (after its backoff).
    Building,
    /// An instance exists; the bootstrap has not finished. Only snapshot calls
    /// are admitted.
    Booting,
    Live,
    /// A retirement step is running; then build again, or halt.
    Retiring(After),
    /// The account needs a new sign-in; no instance until then (task 6).
    Halted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum After {
    Rebuild,
    Halt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Migration {
    Never,
    /// Already rebuilt once for a migration; the next one halts.
    Once,
}

/// The output of a lifecycle step.
pub(super) enum Step<B: Backend> {
    Built(Result<Instance<B>, BackendError>),
    Retired(Retired),
}

pub(super) struct Retired {
    snapshot: SessionSnapshot,
    /// Completions that must still be applied: send outcomes (which outlive the
    /// instance) and timed-out calls holding a permit.
    dones: Vec<Done>,
}

/// Rebuild delays: exponential with jitter, reset after stable operation.
#[derive(Debug, Default)]
struct Backoff {
    attempt: u32,
    stable_since: Option<Instant>,
}

impl Backoff {
    fn next(&mut self, now: Instant) -> (u32, Duration) {
        if self
            .stable_since
            .take()
            .is_some_and(|t| now.duration_since(t) >= STABLE_AFTER)
        {
            self.attempt = 0;
        }
        self.attempt = self.attempt.saturating_add(1);
        let factor = 2u32
            .checked_pow(self.attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        let base = BACKOFF_MIN.saturating_mul(factor).min(BACKOFF_MAX);
        (self.attempt, base + jitter(base))
    }
}

/// Up to a fifth of `base`, so clients that failed together retry apart.
fn jitter(base: Duration) -> Duration {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return Duration::ZERO;
    }
    let span = u64::try_from(base.as_millis() / 5).unwrap_or(0);
    Duration::from_millis(u64::from_le_bytes(bytes) % span.saturating_add(1))
}

/// Lifecycle state owned by the actor.
pub(super) struct Lifecycle<B: Backend> {
    pub(super) phase: Phase,
    step: Option<JoinHandle<Step<B>>>,
    /// Live critical snapshot of the last retired instance; `None` before the
    /// first retirement (the first build loads persisted state).
    session: Option<SessionSnapshot>,
    backoff: Backoff,
    /// Delay before the build that follows the running retirement.
    pending_delay: Duration,
    timeouts: VecDeque<Instant>,
    migration: Migration,
}

impl<B: Backend> Default for Lifecycle<B> {
    fn default() -> Self {
        Self {
            phase: Phase::Building,
            step: None,
            session: None,
            backoff: Backoff::default(),
            pending_delay: Duration::ZERO,
            timeouts: VecDeque::with_capacity(STALL_TIMEOUTS),
            migration: Migration::Never,
        }
    }
}

/// Awaits the running step, if any.
pub(super) async fn next_step<B: Backend>(
    slot: &mut Option<JoinHandle<Step<B>>>,
) -> Result<Step<B>, JoinError> {
    let Some(handle) = slot.as_mut() else {
        return std::future::pending().await;
    };
    let res = handle.await;
    *slot = None;
    res
}

impl<B: Backend> Lifecycle<B> {
    pub(super) fn step_mut(&mut self) -> &mut Option<JoinHandle<Step<B>>> {
        &mut self.step
    }
}

/// One live client instance and everything bound to it.
pub(super) struct Live<B: Backend> {
    pub(super) client: Arc<B::Client>,
    control: B::Control,
    runner: Option<JoinHandle<()>>,
    pub(super) tasks: JoinSet<Done>,
    pub(super) kinds: HashMap<Id, TaskKind>,
    pub(super) cancel: CancellationToken,
    cgen: ClientGen,
}

pub(super) enum LiveEvent {
    Joined(Result<(Id, Done), JoinError>),
    /// The runner returned or panicked without being asked to quit.
    RunnerEnded,
}

/// Next event of the live instance; pending when there is none.
pub(super) async fn next_live<B: Backend>(live: &mut Option<Live<B>>) -> LiveEvent {
    let Some(l) = live.as_mut() else {
        return std::future::pending().await;
    };
    let runner = async {
        match l.runner.as_mut() {
            Some(h) => {
                let _ = h.await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        Some(joined) = l.tasks.join_next_with_id(), if !l.tasks.is_empty() => {
            LiveEvent::Joined(joined)
        }
        () = runner => {
            l.runner = None;
            LiveEvent::RunnerEnded
        }
    }
}

/// Retires an instance (plan section 2, rebuild steps 2-6). Returns once its
/// runner has completed or its cancellation has completed.
pub(super) async fn retire<B: Backend>(live: Live<B>, caps: Caps) -> Retired {
    let Live {
        client,
        control,
        runner,
        mut tasks,
        mut kinds,
        cancel,
        cgen,
    } = live;
    cancel.cancel();
    let mut dones = Vec::with_capacity(MAX_CONCURRENT);
    let deadline = Instant::now() + caps.tasks;
    while let Ok(Some(joined)) = tokio::time::timeout_at(deadline, tasks.join_next_with_id()).await
    {
        keep(joined, &mut kinds, cgen, &mut dones);
    }
    tasks.abort_all();
    while let Some(joined) = tasks.join_next_with_id().await {
        keep(joined, &mut kinds, cgen, &mut dones);
    }
    control.quit();
    if let Some(mut handle) = runner
        && tokio::time::timeout(caps.runner, &mut handle)
            .await
            .is_err()
    {
        // `abort` only schedules cancellation: await it to completion.
        handle.abort();
        let _ = handle.await;
    }
    let snapshot = control.retire();
    drop(client);
    Retired { snapshot, dones }
}

/// Keeps the completions retirement must hand back; a send whose task was
/// aborted or panicked has an unknown outcome.
fn keep(
    joined: Result<(Id, Done), JoinError>,
    kinds: &mut HashMap<Id, TaskKind>,
    cgen: ClientGen,
    dones: &mut Vec<Done>,
) {
    match joined {
        Ok((id, done)) => {
            kinds.remove(&id);
            if done.parked.is_some() || matches!(done.outcome, Outcome::Send { .. }) {
                dones.push(done);
            }
        }
        Err(e) => {
            if let Some(TaskKind::Send(local_id)) = kinds.remove(&e.id()) {
                dones.push(Done {
                    cgen,
                    parked: None,
                    outcome: Outcome::Send {
                        local_id,
                        res: Err(CallError::Dropped),
                    },
                });
            }
        }
    }
}

impl<B: Backend> Actor<B> {
    /// Schedules a build after `delay`.
    pub(super) fn start_build(&mut self, delay: Duration) {
        let session = self
            .life
            .session
            .clone()
            .map_or(SessionSeed::Stored, SessionSeed::Live);
        let seed = Seed {
            session,
            peers: self.carried_peers(),
        };
        let backend = Arc::clone(&self.backend);
        self.life.phase = Phase::Building;
        self.life.step = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            Step::Built(backend.build(seed).await)
        }));
    }

    fn carried_peers(&self) -> CarriedPeers {
        let mut sends = self.sends.chats();
        sends.sort_unstable();
        CarriedPeers::new(self.windows.chats().chain(sends))
    }

    pub(super) async fn on_step(&mut self, res: Result<Step<B>, JoinError>) {
        match (res, self.life.phase) {
            (Ok(Step::Built(Ok(instance))), _) => self.go_live(instance),
            (Ok(Step::Built(Err(_))) | Err(_), Phase::Building) => self.build_failed().await,
            (Ok(Step::Retired(retired)), Phase::Retiring(after)) => {
                self.finish_retire(retired, after).await;
            }
            // A retirement panicked: the instance is gone with it, so carry on
            // with the last known snapshot.
            (Err(_), Phase::Retiring(after)) => {
                self.admission.release_parked();
                self.after_retire(after).await;
            }
            _ => {}
        }
    }

    async fn build_failed(&mut self) {
        self.emit(Event::Connection(ConnState::Offline)).await;
        let (_, delay) = self.life.backoff.next(Instant::now());
        self.start_build(delay);
    }

    /// Starts the instance's runner and update consumer, then bootstraps.
    fn go_live(&mut self, instance: Instance<B>) {
        let Instance {
            client,
            updates,
            runner,
            control,
        } = instance;
        let cancel = CancellationToken::new();
        let mut live = Live {
            client: Arc::new(client),
            control,
            runner: Some(tokio::spawn(runner)),
            tasks: JoinSet::new(),
            kinds: HashMap::new(),
            cancel: cancel.clone(),
            cgen: self.cgen,
        };
        let consumer = jobs::consume_updates(updates, self.cgen, self.updates_tx.clone(), cancel);
        let handle = live.tasks.spawn(consumer);
        live.kinds.insert(handle.id(), TaskKind::Updates);
        self.live = Some(live);
        self.life.phase = Phase::Booting;
        self.boot = Some(Boot::full(Instant::now()));
    }

    /// Replaces the instance. Coalesces: a trigger while no instance is up is
    /// ignored, so each failure causes exactly one rebuild.
    pub(super) async fn rebuild(&mut self, reason: Reason) {
        if !matches!(self.life.phase, Phase::Booting | Phase::Live) {
            return;
        }
        let Some(live) = self.live.take() else {
            return;
        };
        let after = self.after_for(reason);
        self.begin_retire();
        self.life.phase = Phase::Retiring(after);
        if after == After::Rebuild {
            let (attempt, delay) = self.life.backoff.next(Instant::now());
            self.life.pending_delay = delay;
            self.emit(Event::Connection(ConnState::Rebuilding { attempt }))
                .await;
        }
        self.life.step = Some(tokio::spawn(async move {
            Step::Retired(retire(live, REBUILD_CAPS).await)
        }));
    }

    /// An authenticated migration rebuilds once; a second one halts. (The
    /// failed call already reported `AccountMigrated`.)
    fn after_for(&mut self, reason: Reason) -> After {
        if reason != Reason::Migrated {
            return After::Rebuild;
        }
        match self.life.migration {
            Migration::Never => {
                self.life.migration = Migration::Once;
                After::Rebuild
            }
            Migration::Once => After::Halt,
        }
    }

    /// Rebuild step 1: stop admission and make every completion of the old
    /// instance stale. Parked permits stay parked until retirement completes.
    fn begin_retire(&mut self) {
        self.cgen = ClientGen(self.cgen.0 + 1);
        self.boot = None;
        self.life.timeouts.clear();
        self.history_busy.clear();
        self.dialogs_busy = false;
        self.dialogs_queued = false;
        self.purge_queued();
    }

    async fn finish_retire(&mut self, retired: Retired, after: After) {
        let Retired { snapshot, dones } = retired;
        self.life.session = Some(snapshot);
        for done in dones {
            self.on_done(done).await;
        }
        // The runner has completed: grammers' queues are gone with it.
        self.admission.release_parked();
        self.after_retire(after).await;
    }

    async fn after_retire(&mut self, after: After) {
        match after {
            After::Rebuild => self.start_build(self.life.pending_delay),
            After::Halt => {
                self.life.phase = Phase::Halted;
                self.emit(Event::Connection(ConnState::Offline)).await;
            }
        }
    }

    /// Bootstrap finished: resume admission and replay unresolved sends.
    pub(super) async fn go_online(&mut self) {
        self.life.phase = Phase::Live;
        self.life.backoff.stable_since = Some(Instant::now());
        self.emit(Event::Connection(ConnState::Online)).await;
        self.replay_sends().await;
    }

    /// Stall and migration detection on a current-generation call result.
    pub(super) async fn inspect_call(&mut self, err: CallError) {
        match err {
            CallError::Backend(BackendError::Migrated) => self.rebuild(Reason::Migrated).await,
            CallError::Timeout => {
                let now = Instant::now();
                let t = &mut self.life.timeouts;
                while t
                    .front()
                    .is_some_and(|f| now.duration_since(*f) > STALL_WINDOW)
                    || t.len() >= STALL_TIMEOUTS
                {
                    t.pop_front();
                }
                t.push_back(now);
                if t.len() >= STALL_TIMEOUTS || self.admission.parked() >= MAX_CONCURRENT {
                    self.rebuild(Reason::Stall).await;
                }
            }
            CallError::Backend(_) | CallError::Dropped => {}
        }
    }

    /// Shutdown: retires whatever instance exists, with short caps.
    pub(super) async fn shut_down_instance(&mut self) {
        if let Some(mut step) = self.life.step.take()
            && tokio::time::timeout(SHUTDOWN_CAPS.tasks + SHUTDOWN_CAPS.runner, &mut step)
                .await
                .is_err()
        {
            step.abort();
            let _ = step.await;
        }
        if let Some(live) = self.live.take() {
            retire(live, SHUTDOWN_CAPS).await;
        }
    }
}
