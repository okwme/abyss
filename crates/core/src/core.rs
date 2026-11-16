//! Starting core: a dedicated `abyss-core` OS thread running a small multi-thread
//! tokio runtime, plus the handle the UI talks to.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::actor::{Actor, ActorInit, Internal};
use crate::api::{Command, Event};
use crate::backend::Backend;
use crate::cache::{CacheError, CacheHandle, CacheKey};
use crate::events::{self, EventReceiver};

/// Queued UI commands.
const COMMAND_CAPACITY: usize = 256;
/// Queued supervisor messages (rebuilds are rare and coalesce).
const INTERNAL_CAPACITY: usize = 8;
/// Runtime worker threads.
const WORKERS: usize = 2;
/// How long shutdown waits for spawned tasks after the actor stops.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);
/// How long shutdown waits for queued cache writes to land.
const CACHE_FLUSH: Duration = Duration::from_secs(3);

pub struct CacheConfig {
    pub dir: PathBuf,
    pub key: CacheKey,
}

impl std::fmt::Debug for CacheConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheConfig")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub struct CoreConfig {
    /// `None` runs without a disk cache.
    pub cache: Option<CacheConfig>,
}

/// Telegram API credentials, read from the environment at runtime only.
pub struct Credentials {
    pub api_id: i32,
    pub api_hash: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials(<redacted>)")
    }
}

#[derive(Debug)]
pub enum BackendChoice {
    /// The real backend (task 11).
    Grammers(Credentials),
    #[cfg(feature = "fake")]
    Fake(crate::backend::fake::FakeBackend),
}

#[derive(Debug)]
pub enum StartError {
    NotImplemented,
    Runtime(std::io::Error),
    Thread(std::io::Error),
    Cache(CacheError),
}

/// Why a command was not queued. Carries the command back.
#[derive(Debug)]
pub enum SendError {
    /// The command queue is full; core is busy.
    Full(Command),
    /// Core has stopped.
    Closed(Command),
}

/// Whether core confirmed a clean stop: actor joined and cache writes flushed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownReport {
    Confirmed,
    Unconfirmed,
}

/// The UI's connection to core. Dropping it stops core.
#[derive(Debug)]
pub struct CoreHandle {
    cmd: mpsc::Sender<Command>,
    events: EventReceiver,
    cancel: CancellationToken,
    done: Option<oneshot::Receiver<ShutdownReport>>,
    /// Control plane: never queued behind UI commands.
    internal: mpsc::Sender<Internal>,
}

impl CoreHandle {
    /// Tells core the system woke from sleep (`NSWorkspaceDidWakeNotification`).
    /// Core rebuilds the client rather than trust sockets across sleep. Never
    /// blocks; wakes that arrive while one is pending coalesce.
    pub fn notify_wake(&self) {
        let _ = self.internal.try_send(Internal::Wake);
    }

    /// Queues a command without waiting.
    pub fn send(&self, cmd: Command) -> Result<(), SendError> {
        self.cmd.try_send(cmd).map_err(|e| match e {
            mpsc::error::TrySendError::Full(c) => SendError::Full(c),
            mpsc::error::TrySendError::Closed(c) => SendError::Closed(c),
        })
    }

    /// Next event; `None` once core has stopped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.events.try_recv()
    }

    /// Asks core to stop. The receiver resolves when the core thread is done
    /// (`None` if called twice).
    pub fn begin_shutdown(&mut self) -> Option<oneshot::Receiver<ShutdownReport>> {
        self.cancel.cancel();
        self.done.take()
    }
}

impl Drop for CoreHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Starts core on its own thread.
#[cfg_attr(
    not(feature = "fake"),
    expect(
        clippy::needless_pass_by_value,
        reason = "takes ownership of the cache key; only the fake backend consumes it until task 11"
    )
)]
pub fn start(config: CoreConfig, backend: BackendChoice) -> Result<CoreHandle, StartError> {
    match backend {
        BackendChoice::Grammers(_) => {
            drop(config);
            Err(StartError::NotImplemented)
        }
        #[cfg(feature = "fake")]
        BackendChoice::Fake(fake) => start_with(config, fake),
    }
}

#[cfg_attr(
    not(feature = "fake"),
    expect(dead_code, reason = "only the fake backend exists until task 11")
)]
fn start_with<B: Backend>(config: CoreConfig, backend: B) -> Result<CoreHandle, StartError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .thread_name("abyss-rt")
        .enable_all()
        .build()
        .map_err(StartError::Runtime)?;
    let cache = config
        .cache
        .map(|c| CacheHandle::spawn(c.dir, &c.key))
        .transpose()
        .map_err(StartError::Cache)?;
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (internal_tx, internal_rx) = mpsc::channel(INTERNAL_CAPACITY);
    let (events_tx, events_rx) = events::channel();
    let (done_tx, done_rx) = oneshot::channel();
    let cancel = CancellationToken::new();
    let (actor, channels) = Actor::new(ActorInit {
        backend: Arc::new(backend),
        events: events_tx,
        cache: cache.clone(),
        commands: cmd_rx,
        internal: internal_rx,
        cancel: cancel.clone(),
    });
    std::thread::Builder::new()
        .name("abyss-core".into())
        .spawn(move || {
            let report = runtime.block_on(async move {
                // Spawned so a panic in the actor itself is contained and reported.
                let joined = tokio::spawn(actor.run(channels)).await;
                let flushed = match cache {
                    Some(c) => matches!(
                        tokio::time::timeout(CACHE_FLUSH, c.barrier()).await,
                        Ok(Ok(()))
                    ),
                    None => true,
                };
                if joined.is_ok() && flushed {
                    ShutdownReport::Confirmed
                } else {
                    ShutdownReport::Unconfirmed
                }
            });
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
            let _ = done_tx.send(report);
        })
        .map_err(StartError::Thread)?;
    Ok(CoreHandle {
        cmd: cmd_tx,
        events: events_rx,
        cancel,
        done: Some(done_rx),
        internal: internal_tx,
    })
}

#[cfg(all(test, feature = "fake"))]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, FakeScript};

    #[tokio::test]
    async fn starts_loads_dialogs_and_shuts_down_cleanly() {
        let (fake, _h) = FakeBackend::new(FakeScript::demo(3, 5));
        let mut core = start(CoreConfig::default(), BackendChoice::Fake(fake)).unwrap();
        let reset = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match core.recv().await {
                    Some(Event::DialogsReset(chats)) => return chats,
                    Some(_) => {}
                    None => panic!("core stopped"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(reset.len(), 3);
        let done = core.begin_shutdown().unwrap();
        assert_eq!(done.await.unwrap(), ShutdownReport::Confirmed);
        assert!(core.begin_shutdown().is_none());
    }

    #[test]
    fn grammers_is_not_implemented_yet() {
        let creds = Credentials {
            api_id: 1,
            api_hash: "x".into(),
        };
        assert!(format!("{creds:?}").contains("redacted"));
        let res = start(CoreConfig::default(), BackendChoice::Grammers(creds));
        assert!(matches!(res, Err(StartError::NotImplemented)));
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImplemented => f.write_str("this backend is not implemented yet"),
            Self::Runtime(_) => f.write_str("could not start the async runtime"),
            Self::Thread(_) => f.write_str("could not start the core thread"),
            Self::Cache(_) => f.write_str("could not open the message cache"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotImplemented => None,
            Self::Runtime(e) | Self::Thread(e) => Some(e),
            Self::Cache(e) => Some(e),
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
