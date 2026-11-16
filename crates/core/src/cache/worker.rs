//! Dedicated OS thread that owns the [`Cache`]. Commands are processed strictly
//! in order, so a read observes every write sent before it, and `Erase` runs
//! only after everything queued ahead of it.

use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

use super::{Binding, Cache, CacheError, CacheKey, EraseError};
use crate::model::{ChatId, ChatSummary, Message, MsgId};

/// Commands queued to the worker.
pub const CACHE_QUEUE: usize = 256;

type Reply<T> = oneshot::Sender<Result<T, CacheError>>;
/// Error from deleting the persistent cache key (Keychain).
pub type KeyError = Box<dyn std::error::Error + Send + Sync>;
/// Deletes the persistent cache key (Keychain). Runs on the worker thread.
pub type DeleteKey = Box<dyn FnOnce() -> Result<(), KeyError> + Send>;

pub(crate) enum CacheCmd {
    PutChats(Vec<ChatSummary>),
    PutMessages(ChatId, Vec<Message>),
    Reconcile {
        chat: ChatId,
        lo: MsgId,
        hi: MsgId,
        messages: Vec<Message>,
    },
    /// A server tail (ascending, plus any newer live messages); see `reconcile_tail`.
    ReconcileTail {
        chat: ChatId,
        messages: Vec<Message>,
        /// The oldest id the server page covered when it is partial (which
        /// may be absent from `messages` if it was deleted meanwhile), or
        /// `None` when the page is the chat's whole history.
        from: Option<MsgId>,
    },
    Delete(ChatId, Vec<MsgId>),
    DeleteUnscoped(Vec<MsgId>),
    Touch(ChatId),
    Chats(Reply<Vec<ChatSummary>>),
    Tail {
        chat: ChatId,
        limit: u32,
        reply: Reply<Vec<Message>>,
    },
    Before {
        chat: ChatId,
        id: MsgId,
        limit: u32,
        reply: Reply<Vec<Message>>,
    },
    After {
        chat: ChatId,
        id: MsgId,
        limit: u32,
        reply: Reply<Vec<Message>>,
    },
    /// Replies once every earlier command has been processed.
    Barrier(oneshot::Sender<()>),
    /// See [`Cache::bind`].
    Bind {
        owner: Option<i64>,
        reply: Reply<Binding>,
    },
    Erase {
        key: CacheKey,
        delete_key: DeleteKey,
        reply: oneshot::Sender<Result<(), EraseFailure>>,
    },
}

/// Async handle to the worker. Cloneable; the worker exits when all handles drop
/// or after `erase`.
#[derive(Clone, Debug)]
pub struct CacheHandle {
    tx: mpsc::Sender<CacheCmd>,
}

impl std::fmt::Debug for CacheCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheCmd")
    }
}

/// Error from a worker request: the worker is gone or the cache failed.
#[derive(Debug)]
pub enum WorkerError {
    Stopped,
    Cache(CacheError),
}

impl CacheHandle {
    /// Opens the cache and starts the worker thread `abyss-cache`.
    pub fn spawn(dir: PathBuf, key: &CacheKey) -> Result<CacheHandle, CacheError> {
        let cache = Cache::open(&dir, key)?;
        let (tx, rx) = mpsc::channel(CACHE_QUEUE);
        std::thread::Builder::new()
            .name("abyss-cache".into())
            .spawn(move || run(cache, &dir, rx))
            .map_err(CacheError::Io)?;
        Ok(CacheHandle { tx })
    }

    async fn send(&self, cmd: CacheCmd) -> Result<(), WorkerError> {
        self.tx.send(cmd).await.map_err(|_| WorkerError::Stopped)
    }

    /// Queues a write. Writes are fire-and-forget: the cache is disposable and the
    /// server stays authoritative.
    pub(crate) async fn write(&self, cmd: CacheCmd) {
        let _ = self.send(cmd).await;
    }

    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> CacheCmd) -> Result<T, WorkerError> {
        let (reply, rx) = oneshot::channel();
        self.send(make(reply)).await?;
        rx.await
            .map_err(|_| WorkerError::Stopped)?
            .map_err(WorkerError::Cache)
    }

    pub async fn chats(&self) -> Result<Vec<ChatSummary>, WorkerError> {
        self.ask(CacheCmd::Chats).await
    }

    pub async fn tail(&self, chat: ChatId, limit: u32) -> Result<Vec<Message>, WorkerError> {
        self.ask(|reply| CacheCmd::Tail { chat, limit, reply })
            .await
    }

    pub async fn before(
        &self,
        chat: ChatId,
        id: MsgId,
        limit: u32,
    ) -> Result<Vec<Message>, WorkerError> {
        self.ask(|reply| CacheCmd::Before {
            chat,
            id,
            limit,
            reply,
        })
        .await
    }

    pub async fn after(
        &self,
        chat: ChatId,
        id: MsgId,
        limit: u32,
    ) -> Result<Vec<Message>, WorkerError> {
        self.ask(|reply| CacheCmd::After {
            chat,
            id,
            limit,
            reply,
        })
        .await
    }

    /// Binds the cache to a verified account (see [`Cache::bind`]).
    pub async fn bind(&self, owner: Option<i64>) -> Result<Binding, WorkerError> {
        self.ask(|reply| CacheCmd::Bind { owner, reply }).await
    }

    /// Waits until every command sent before this one has been processed.
    pub async fn barrier(&self) -> Result<(), WorkerError> {
        let (tx, rx) = oneshot::channel();
        self.send(CacheCmd::Barrier(tx)).await?;
        rx.await.map_err(|_| WorkerError::Stopped)
    }

    /// Crypto-erases the cache after all queued commands, then stops the worker.
    pub async fn erase(&self, key: CacheKey, delete_key: DeleteKey) -> Result<(), EraseFailure> {
        let (reply, rx) = oneshot::channel();
        self.send(CacheCmd::Erase {
            key,
            delete_key,
            reply,
        })
        .await
        .map_err(|_| EraseFailure::Stopped)?;
        rx.await.map_err(|_| EraseFailure::Stopped)?
    }
}

fn run(mut cache: Cache, dir: &Path, mut rx: mpsc::Receiver<CacheCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        if let CacheCmd::Erase {
            key,
            delete_key,
            reply,
        } = cmd
        {
            rx.close();
            let result = cache
                .erase(dir, key, delete_key)
                .map_err(EraseFailure::from);
            let _ = reply.send(result);
            return;
        }
        handle(&mut cache, cmd);
    }
}

/// Applies a server tail. If it overlaps the cached history (or reaches the
/// start of the chat), only the range it covers is reconciled. Otherwise there
/// may be a gap between the cache and the tail, so the chat's cache is replaced.
/// Decided here, against what the cache holds, because the actor's window may
/// have been invalidated by a rebuild. `older` is the tail's first id when older
/// history exists beyond it.
fn reconcile_tail(
    cache: &mut Cache,
    chat: ChatId,
    messages: &[Message],
    older: Option<MsgId>,
) -> Result<Vec<MsgId>, CacheError> {
    const MIN: MsgId = MsgId(i32::MIN);
    let lo = match older {
        Some(first) => {
            let newest = cache.tail(chat, 1)?.last().map(|m| m.id);
            if newest.is_none_or(|n| first <= n) {
                first
            } else {
                MIN
            }
        }
        None => MIN,
    };
    cache.reconcile_range(chat, lo, MsgId(i32::MAX), messages)
}

/// Write errors are dropped deliberately: the cache is disposable, and a failed
/// write only means a later read falls back to the server.
fn handle(cache: &mut Cache, cmd: CacheCmd) {
    match cmd {
        CacheCmd::PutChats(chats) => drop(cache.put_chats(&chats)),
        CacheCmd::PutMessages(chat, msgs) => drop(cache.put_messages(chat, &msgs)),
        CacheCmd::Reconcile {
            chat,
            lo,
            hi,
            messages,
        } => {
            drop(cache.reconcile_range(chat, lo, hi, &messages));
        }
        CacheCmd::ReconcileTail {
            chat,
            messages,
            from,
        } => {
            drop(reconcile_tail(cache, chat, &messages, from));
        }
        CacheCmd::Delete(chat, ids) => drop(cache.delete_messages(chat, &ids)),
        CacheCmd::DeleteUnscoped(ids) => drop(cache.delete_unscoped(&ids)),
        CacheCmd::Touch(chat) => drop(cache.touch(chat)),
        CacheCmd::Chats(reply) => drop(reply.send(cache.chats())),
        CacheCmd::Tail { chat, limit, reply } => drop(reply.send(cache.tail(chat, limit))),
        CacheCmd::Before {
            chat,
            id,
            limit,
            reply,
        } => drop(reply.send(cache.before(chat, id, limit))),
        CacheCmd::After {
            chat,
            id,
            limit,
            reply,
        } => drop(reply.send(cache.after(chat, id, limit))),
        CacheCmd::Barrier(reply) => drop(reply.send(())),
        CacheCmd::Bind { owner, reply } => drop(reply.send(cache.bind(owner))),
        // Unreachable: `run` intercepts Erase. Report "not performed" if it ever arrives.
        CacheCmd::Erase { reply, .. } => drop(reply.send(Err(EraseFailure::Stopped))),
    }
}

/// Why an erase did not complete. On any error other than `Files`, the key or the
/// files may still exist; the caller must not report a successful logout.
#[derive(Debug)]
pub enum EraseFailure {
    /// The worker stopped before the erase ran.
    Stopped,
    /// Closing the database failed; nothing was deleted.
    Close(rusqlite::Error),
    /// Deleting the Keychain key failed; the encrypted files were kept.
    DeleteKey(KeyError),
    /// The key is gone (data is unreadable) but removing the files failed.
    Files(std::io::Error),
}

impl From<EraseError<KeyError>> for EraseFailure {
    fn from(e: EraseError<KeyError>) -> Self {
        match e {
            EraseError::Close(e) => Self::Close(e),
            EraseError::DeleteKey(e) => Self::DeleteKey(e),
            EraseError::Files(e) => Self::Files(e),
        }
    }
}

impl std::fmt::Display for EraseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => f.write_str("cache worker stopped before erasing"),
            Self::Close(_) => f.write_str("could not close the cache database"),
            Self::DeleteKey(_) => f.write_str("could not delete the cache key"),
            Self::Files(_) => f.write_str("cache key deleted but files could not be removed"),
        }
    }
}

impl std::error::Error for EraseFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Stopped => None,
            Self::Close(e) => Some(e),
            Self::DeleteKey(e) => Some(&**e),
            Self::Files(e) => Some(e),
        }
    }
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => f.write_str("cache worker stopped"),
            Self::Cache(_) => f.write_str("cache operation failed"),
        }
    }
}

impl std::error::Error for WorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Stopped => None,
            Self::Cache(e) => Some(e),
        }
    }
}
