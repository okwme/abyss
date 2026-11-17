//! Starting core: a dedicated `abyss-core` OS thread running a small multi-thread
//! tokio runtime, plus the handle the UI talks to.
//!
//! Startup (task 6): spawn the persistence worker (the Keychain in
//! production, an in-memory store under the fake), load the session, provision
//! the cache key and open the cache, then run the actor. Storage problems at
//! startup are reported as events, never as a failed start.

use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::actor::{
    Actor, ActorChannels, ActorInit, Identity, Internal, Stamped, provision_cache,
    read_pending_logout,
};

use crate::api::{Command, Event, LogoutStatus, SaveStatus, UserError};
use crate::backend::Backend;
use crate::backend::grammers::GrammersBackend;
use crate::events::{self, EpochObserver, EventReceiver, EventSender};
use crate::secret::SecretStore;
use crate::session::PersistHandle;

/// Queued UI commands.
const COMMAND_CAPACITY: usize = 256;
/// Queued supervisor messages (rebuilds are rare and coalesce).
const INTERNAL_CAPACITY: usize = 8;
/// Runtime worker threads.
const WORKERS: usize = 2;
/// How long shutdown waits for spawned tasks after the actor stops.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

/// Where the encrypted message cache lives. Core provisions its key.
#[derive(Debug)]
pub struct CacheConfig {
    pub dir: PathBuf,
}

#[derive(Debug, Default)]
pub struct CoreConfig {
    /// `None` runs without a disk cache.
    pub cache: Option<CacheConfig>,
    /// Faults for manual testing of the fake (`ABYSS_FAKE_FAULTS`).
    #[cfg(feature = "fake")]
    pub fake_faults: FakeFaults,
}

/// Environment variable naming fake faults, parsed only in `fake` builds.
#[cfg(feature = "fake")]
pub const FAKE_FAULTS_VAR: &str = "ABYSS_FAKE_FAULTS";

/// Store faults for `--fake` runs: `store-stall` holds every Keychain call
/// after startup (each for at most 10 s), `store-fail` fails every write.
/// Comma-separated; unknown names are ignored.
#[cfg(feature = "fake")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FakeFaults {
    pub store_stall: bool,
    pub store_fail: bool,
}

#[cfg(feature = "fake")]
impl FakeFaults {
    pub fn parse(spec: &str) -> Self {
        let mut faults = Self::default();
        for name in spec.split(',').map(str::trim) {
            match name {
                "store-stall" => faults.store_stall = true,
                "store-fail" => faults.store_fail = true,
                _ => {}
            }
        }
        faults
    }
}

#[derive(Debug)]
pub enum BackendChoice {
    /// The real backend (task 11). The env pair, if any, is resolved first;
    /// then the Keychain, the embedded pair, or onboarding (plan 17.1).
    Grammers(crate::credentials::EnvResolution),
    #[cfg(feature = "fake")]
    Fake(crate::backend::fake::FakeBackend),
}

#[derive(Debug)]
pub enum StartError {
    Runtime(std::io::Error),
    Thread(std::io::Error),
    /// The persistence worker thread could not start.
    Persist(std::io::Error),
}

/// Why a command was not queued. Carries the command back.
#[derive(Debug)]
pub enum SendError {
    /// The command queue is full; core is busy.
    Full(Command),
    /// Core has stopped.
    Closed(Command),
}

/// What core observed while stopping (plan section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownReport {
    /// `Confirmed` only if the instance was retired (runner joined, session
    /// revoked) and the persistence barrier succeeded within the deadline.
    pub save: SaveStatus,
    /// Present when a logout was running or finished during shutdown.
    pub logout: Option<LogoutStatus>,
}

impl ShutdownReport {
    pub const UNCONFIRMED: Self = Self {
        save: SaveStatus::Unconfirmed,
        logout: None,
    };
}

/// Cancels core when the last half of a handle is dropped.
#[derive(Debug)]
struct StopOnDrop(CancellationToken);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// The command half of a [`CoreHandle`].
#[derive(Debug)]
pub struct CoreSender {
    cmd: mpsc::Sender<Stamped>,
    /// The account epoch the UI has seen (stamped on each command).
    epoch: EpochObserver,
    cancel: CancellationToken,
    done: Option<oneshot::Receiver<ShutdownReport>>,
    /// Control plane: never queued behind UI commands.
    internal: mpsc::Sender<Internal>,
    _stop: Arc<StopOnDrop>,
}

/// The event half of a [`CoreHandle`].
#[derive(Debug)]
pub struct CoreEvents {
    events: EventReceiver,
    _stop: Arc<StopOnDrop>,
}

/// The UI's connection to core. Dropping it (both halves, once split) stops
/// core.
#[derive(Debug)]
pub struct CoreHandle {
    sender: CoreSender,
    events: CoreEvents,
}

impl CoreSender {
    /// Tells core the system woke from sleep (`NSWorkspaceDidWakeNotification`).
    /// Core rebuilds the client rather than trust sockets across sleep. Never
    /// blocks; wakes that arrive while one is pending coalesce.
    pub fn notify_wake(&self) {
        let _ = self.internal.try_send(Internal::Wake);
    }

    /// Queues a command without waiting.
    pub fn send(&self, cmd: Command) -> Result<(), SendError> {
        let stamped = Stamped {
            epoch: self.epoch.get(),
            cmd,
        };
        self.cmd.try_send(stamped).map_err(|e| match e {
            mpsc::error::TrySendError::Full(s) => SendError::Full(s.cmd),
            mpsc::error::TrySendError::Closed(s) => SendError::Closed(s.cmd),
        })
    }

    /// Asks core to stop. The receiver resolves when the core thread is done
    /// (`None` if called twice).
    pub fn begin_shutdown(&mut self) -> Option<oneshot::Receiver<ShutdownReport>> {
        self.cancel.cancel();
        self.done.take()
    }

    /// Stops core without waiting for its report.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl CoreEvents {
    /// Next event; `None` once core has stopped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// Polls for the next event; `Ready(None)` once core has stopped.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.events.poll_recv(cx)
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.events.try_recv()
    }
}

impl CoreHandle {
    /// Splits into the command and event halves, so the UI's event pump can
    /// own the receiver. Core stops once both halves are dropped.
    pub fn split(self) -> (CoreSender, CoreEvents) {
        (self.sender, self.events)
    }

    pub fn notify_wake(&self) {
        self.sender.notify_wake();
    }

    pub fn send(&self, cmd: Command) -> Result<(), SendError> {
        self.sender.send(cmd)
    }

    pub async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.events.poll_recv(cx)
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.events.try_recv()
    }

    pub fn begin_shutdown(&mut self) -> Option<oneshot::Receiver<ShutdownReport>> {
        self.sender.begin_shutdown()
    }
}

/// Starts core on its own thread.
pub fn start(config: CoreConfig, backend: BackendChoice) -> Result<CoreHandle, StartError> {
    match backend {
        BackendChoice::Grammers(env) => {
            let store = crate::secret::KeychainStore::new();
            let slot = crate::credentials::CredentialSlot::new(env);
            let make = move |persist| GrammersBackend::new(slot, persist);
            start_with(config, make, store, || {})
        }
        #[cfg(feature = "fake")]
        BackendChoice::Fake(fake) => start_fake(config, fake),
    }
}

#[cfg(feature = "fake")]
fn start_fake(
    config: CoreConfig,
    fake: crate::backend::fake::FakeBackend,
) -> Result<CoreHandle, StartError> {
    use crate::secret::{FaultyStore, MemoryStore, StoreOp};
    let (store, control) = FaultyStore::new(MemoryStore::new());
    let faults = config.fake_faults;
    if faults.store_fail {
        control.fail(StoreOp::Set);
    }
    // Stall only after the startup reads, so the app still comes up.
    let after_load = move || {
        if faults.store_stall {
            control.hold();
        }
    };
    start_with(config, move |_| fake, store, after_load)
}

/// Everything the core thread needs to bring the actor up.
struct Boot<B> {
    backend: B,
    persist: PersistHandle,
    cache: Option<CacheConfig>,
    events: EventSender,
    commands: mpsc::Receiver<Stamped>,
    internal: mpsc::Receiver<Internal>,
    cancel: CancellationToken,
}

/// Starts the persistence worker over `store`, builds the backend over it
/// (`make`), and runs the actor on the core thread.
fn start_with<B: Backend>(
    config: CoreConfig,
    make: impl FnOnce(PersistHandle) -> B,
    store: impl SecretStore,
    after_load: impl FnOnce() + Send + 'static,
) -> Result<CoreHandle, StartError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .thread_name("abyss-rt")
        .enable_all()
        .build()
        .map_err(StartError::Runtime)?;
    let persist = PersistHandle::spawn(store).map_err(StartError::Persist)?;
    let backend = make(persist.clone());
    let (cmd_tx, commands) = mpsc::channel(COMMAND_CAPACITY);
    let (internal_tx, internal) = mpsc::channel(INTERNAL_CAPACITY);
    let (events, events_rx) = events::channel();
    let (done_tx, done_rx) = oneshot::channel();
    let cancel = CancellationToken::new();
    let boot = Boot {
        backend,
        persist,
        cache: config.cache,
        events,
        commands,
        internal,
        cancel: cancel.clone(),
    };
    std::thread::Builder::new()
        .name("abyss-core".into())
        .spawn(move || {
            let report = runtime.block_on(async move {
                // Shutdown during startup (a stalled Keychain read or
                // prompt): nothing was confirmed. The persistence worker is
                // its own thread, so its stalled call stays off this path.
                let Some((actor, channels)) = bring_up(boot, after_load).await else {
                    return ShutdownReport::UNCONFIRMED;
                };
                // Spawned so a panic in the actor itself is contained and reported.
                tokio::spawn(actor.run(channels))
                    .await
                    .unwrap_or(ShutdownReport::UNCONFIRMED)
            });
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
            let _ = done_tx.send(report);
        })
        .map_err(StartError::Thread)?;
    let epoch = events_rx.observer();
    let stop = Arc::new(StopOnDrop(cancel.clone()));
    Ok(CoreHandle {
        sender: CoreSender {
            cmd: cmd_tx,
            epoch,
            cancel,
            done: Some(done_rx),
            internal: internal_tx,
            _stop: Arc::clone(&stop),
        },
        events: CoreEvents {
            events: events_rx,
            _stop: stop,
        },
    })
}

/// [`prepare`], abandoned when shutdown starts first (review round 8): the
/// startup waits must never hold up the shutdown acknowledgement.
async fn bring_up<B: Backend>(
    boot: Boot<B>,
    after_load: impl FnOnce(),
) -> Option<(Actor<B>, ActorChannels)> {
    let cancel = boot.cancel.clone();
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        up = prepare(boot, after_load) => Some(up),
    }
}

/// Loads the session and opens the cache (task 5 wiring, steps 1-2). A
/// Keychain that cannot be read counts as logged out, with a warning. The
/// cache is bound to the loaded account, or quarantined (plan section 16).
async fn prepare<B: Backend>(
    boot: Boot<B>,
    after_load: impl FnOnce(),
) -> (Actor<B>, ActorChannels) {
    let Boot {
        backend,
        persist,
        cache,
        events,
        commands,
        internal,
        cancel,
    } = boot;
    // Before the session: an interrupted logout is finished first, and the
    // old account's session is never loaded (review round 15, A).
    let pending_logout = read_pending_logout(&persist).await;
    let (account, identity) = match pending_logout {
        Some(_) => (persist.fresh_session(), Identity::Unknown),
        None => load_account(&persist, &events).await,
    };
    let (handle, cache_parts) = match cache {
        Some(CacheConfig { dir }) => {
            let provisioned = provision_cache(&persist, dir, identity).await;
            (provisioned.served, Some(provisioned.parts))
        }
        None => (None, None),
    };
    after_load();
    Actor::new(ActorInit {
        backend: Arc::new(backend),
        events,
        cache: handle,
        commands,
        internal,
        cancel,
        persist,
        account,
        cache_parts,
        pending_logout,
    })
}

/// Loads the stored session. A Keychain that cannot be read leaves the
/// account unknown, with a warning.
async fn load_account(
    persist: &PersistHandle,
    events: &EventSender,
) -> (crate::session::SessionSnapshot, Identity) {
    match persist.load_session().await {
        Ok(loaded) => {
            let identity = match loaded.snapshot.self_id() {
                Ok(Some(id)) => Identity::Verified(id),
                _ => Identity::SignedOut,
            };
            (loaded.snapshot, identity)
        }
        Err(e) => {
            let _ = events.send(Event::Error(UserError::from(&e))).await;
            (persist.fresh_session(), Identity::Unknown)
        }
    }
}

#[cfg(all(test, feature = "fake"))]
mod startup_tests;

#[cfg(all(test, feature = "fake"))]
mod tests {
    use super::*;
    use crate::api::AuthState;
    use crate::backend::fake::{FAKE_CODE, FakeBackend, FakeScript};

    /// The next event matching `pred`; `None` if core stopped or 10 s passed.
    async fn until(core: &mut CoreHandle, pred: impl Fn(&Event) -> bool) -> Option<Event> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match core.recv().await {
                    Some(e) if pred(&e) => return Some(e),
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    #[tokio::test]
    async fn logs_in_loads_dialogs_and_shuts_down_cleanly() {
        let mut script = FakeScript::demo(3, 5);
        script.login.password = None;
        let (fake, _h) = FakeBackend::new(script);
        let mut core = start(CoreConfig::default(), BackendChoice::Fake(fake)).unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Phone { .. }))
        })
        .await;
        core.send(Command::SubmitPhone("+15550100".into())).unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Code { .. }))
        })
        .await;
        core.send(Command::SubmitCode(FAKE_CODE.into())).unwrap();
        let signed_in = until(
            &mut core,
            |e| matches!(e, Event::Auth(s) if s.is_signed_in()),
        )
        .await;
        assert!(matches!(
            signed_in,
            Some(Event::Auth(AuthState::SignedIn { .. }))
        ));
        let reset = until(
            &mut core,
            |e| matches!(e, Event::DialogsReset(v) if !v.is_empty()),
        )
        .await;
        assert!(matches!(reset, Some(Event::DialogsReset(v)) if v.len() == 3));
        let done = core.begin_shutdown().unwrap();
        let report = done.await.unwrap();
        assert_eq!(report.save, SaveStatus::Confirmed);
        assert_eq!(report.logout, None);
        assert!(core.begin_shutdown().is_none());
    }

    #[tokio::test]
    async fn split_halves_and_poll_recv() {
        let (fake, _h) = FakeBackend::new(FakeScript::default());
        let core = start(CoreConfig::default(), BackendChoice::Fake(fake)).unwrap();
        let (mut sender, mut events) = core.split();
        let first = std::future::poll_fn(|cx| events.poll_recv(cx)).await;
        assert!(matches!(first, Some(Event::Auth(AuthState::Connecting))));
        let report = sender.begin_shutdown().unwrap().await.unwrap();
        assert_eq!(report.save, SaveStatus::Confirmed);
    }

    #[tokio::test]
    async fn store_fail_fault_makes_the_login_unsaved() {
        let (fake, _h) = FakeBackend::new(FakeScript::default());
        let config = CoreConfig {
            fake_faults: FakeFaults::parse("store-fail, unknown"),
            ..CoreConfig::default()
        };
        let mut core = start(config, BackendChoice::Fake(fake)).unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Phone { .. }))
        })
        .await;
        core.send(Command::SubmitPhone("+15550100".into())).unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Code { .. }))
        })
        .await;
        core.send(Command::SubmitCode(FAKE_CODE.into())).unwrap();
        let ev = until(
            &mut core,
            |e| matches!(e, Event::Auth(s) if s.is_signed_in()),
        )
        .await;
        assert!(matches!(
            ev,
            Some(Event::Auth(AuthState::SignedInUnsaved { .. }))
        ));
        let report = core.begin_shutdown().unwrap().await.unwrap();
        assert_eq!(report.save, SaveStatus::Unconfirmed);
    }

    #[test]
    fn fake_faults_parse() {
        let f = FakeFaults::parse("store-stall");
        assert!(f.store_stall && !f.store_fail);
        assert_eq!(FakeFaults::parse(""), FakeFaults::default());
    }

    /// `--fake --onboarding`: no env pair and nothing stored, so core asks
    /// for credentials and builds nothing until a pair is entered.
    #[tokio::test]
    async fn onboarding_starts_from_need_credentials() {
        let script = FakeScript {
            env: crate::credentials::EnvResolution::Absent,
            ..FakeScript::default()
        };
        let (fake, h) = FakeBackend::new(script);
        let mut core = start(CoreConfig::default(), BackendChoice::Fake(fake)).unwrap();
        let ev = until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::NeedCredentials { .. }))
        })
        .await;
        assert!(matches!(
            ev,
            Some(Event::Auth(AuthState::NeedCredentials {
                error: None,
                source: None
            }))
        ));
        assert_eq!(h.builds(), 0);
        core.send(Command::SubmitCredentials {
            api_id: 12345,
            api_hash: crate::api::ApiHash::parse(&"a".repeat(32)).unwrap(),
        })
        .unwrap();
        until(&mut core, |e| {
            matches!(e, Event::Auth(AuthState::Phone { .. }))
        })
        .await;
        assert_eq!(h.builds(), 1);
        let report = core.begin_shutdown().unwrap().await.unwrap();
        assert_eq!(report.save, SaveStatus::Confirmed);
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(_) => f.write_str("could not start the async runtime"),
            Self::Thread(_) => f.write_str("could not start the core thread"),
            Self::Persist(_) => f.write_str("could not start the session store"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(e) | Self::Thread(e) | Self::Persist(e) => Some(e),
        }
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(_) => f.write_str("core is busy; command not queued"),
            Self::Closed(_) => f.write_str("core has stopped; command not queued"),
        }
    }
}

impl std::error::Error for SendError {}
