//! Spawned work: backend calls (admitted, with deadlines) and cache reads.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::OwnedSemaphorePermit;

use crate::api::{ClientGen, HistoryDir};
use crate::backend::{
    Backend, BackendError, DialogsCursor, DialogsPage, HistoryPage, HistoryQuery, Update,
};
use crate::cache::CacheHandle;
use crate::model::{ChatId, ChatSummary, LocalId, Message, MsgId};
use crate::store::ViewGen;

/// Messages per history page (server and cache).
pub(super) const PAGE: u32 = 50;
/// Dialogs per page.
pub(super) const DIALOGS_PAGE: u32 = 100;

/// A backend call waiting for admission.
#[derive(Debug)]
pub(super) enum Job {
    Dialogs {
        cursor: Option<DialogsCursor>,
    },
    History {
        chat: ChatId,
        view: ViewGen,
        dir: HistoryDir,
        query: HistoryQuery,
    },
    Send {
        local_id: LocalId,
        chat: ChatId,
        text: String,
        random_id: i64,
    },
    Read {
        chat: ChatId,
        max_id: MsgId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CallError {
    Backend(BackendError),
    /// The deadline passed; the outcome of a write is unknown.
    Timeout,
}

pub(super) type CallResult<T> = Result<T, CallError>;

/// Whether a dialogs page starts the list (replacing it) or continues it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DialogsStep {
    First,
    Next,
}

/// What a finished task reports back to the actor.
#[derive(Debug)]
pub(super) enum Outcome {
    Dialogs {
        step: DialogsStep,
        res: CallResult<DialogsPage>,
    },
    History {
        chat: ChatId,
        view: ViewGen,
        dir: HistoryDir,
        query: HistoryQuery,
        res: CallResult<HistoryPage>,
    },
    Send {
        local_id: LocalId,
        res: CallResult<MsgId>,
    },
    Read,
    CacheChats(Option<Vec<ChatSummary>>),
    CacheHistory {
        chat: ChatId,
        view: ViewGen,
        dir: HistoryDir,
        anchor: Option<MsgId>,
        messages: Option<Vec<Message>>,
    },
    /// The update consumer stopped (stream error); task 4 turns this into a rebuild.
    UpdatesEnded(BackendError),
}

#[derive(Debug)]
pub(super) struct Done {
    pub(super) cgen: ClientGen,
    /// Present when the call timed out: the permit must be parked.
    pub(super) parked: Option<OwnedSemaphorePermit>,
    pub(super) outcome: Outcome,
}

/// What kind of task a `JoinSet` id belongs to, so a panic can clean up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TaskKind {
    Dialogs,
    History(ChatId),
    Send(LocalId),
    Read,
    CacheChats,
    Updates,
}

/// A spawned task reporting back to the actor.
pub(super) type DoneFuture = Pin<Box<dyn Future<Output = Done> + Send>>;

/// What every admitted call carries: its generation, deadline and permit.
struct Admitted {
    cgen: ClientGen,
    deadline: Duration,
    permit: OwnedSemaphorePermit,
}

impl Admitted {
    /// Runs `call` under the deadline and reports `outcome(result)`. On time-out
    /// the permit is handed back for parking; otherwise it is released here.
    async fn run<T>(
        self,
        call: impl Future<Output = Result<T, BackendError>>,
        outcome: impl FnOnce(CallResult<T>) -> Outcome,
    ) -> Done {
        let (res, parked) = match tokio::time::timeout(self.deadline, call).await {
            Ok(res) => {
                drop(self.permit);
                (res.map_err(CallError::Backend), None)
            }
            Err(_) => (Err(CallError::Timeout), Some(self.permit)),
        };
        Done {
            cgen: self.cgen,
            parked,
            outcome: outcome(res),
        }
    }
}

/// Builds the future for an admitted job, plus its kind.
pub(super) fn job_future<B: Backend>(
    backend: Arc<B>,
    job: Job,
    cgen: ClientGen,
    deadline: Duration,
    permit: OwnedSemaphorePermit,
) -> (TaskKind, DoneFuture) {
    let admitted = Admitted {
        cgen,
        deadline,
        permit,
    };
    match job {
        Job::Dialogs { cursor } => {
            let step = if cursor.is_none() {
                DialogsStep::First
            } else {
                DialogsStep::Next
            };
            let call = async move { backend.dialogs(cursor, DIALOGS_PAGE).await };
            let fut = admitted.run(call, move |res| Outcome::Dialogs { step, res });
            (TaskKind::Dialogs, Box::pin(fut))
        }
        Job::History {
            chat,
            view,
            dir,
            query,
        } => {
            let call = async move { backend.history(chat, query, PAGE).await };
            let fut = admitted.run(call, move |res| Outcome::History {
                chat,
                view,
                dir,
                query,
                res,
            });
            (TaskKind::History(chat), Box::pin(fut))
        }
        Job::Send {
            local_id,
            chat,
            text,
            random_id,
        } => {
            let call = async move { backend.send_text(chat, text, random_id).await };
            let fut = admitted.run(call, move |res| Outcome::Send { local_id, res });
            (TaskKind::Send(local_id), Box::pin(fut))
        }
        Job::Read { chat, max_id } => {
            let call = async move { backend.read_history(chat, max_id).await };
            // Read marks are best effort; the next read covers a lost one.
            let fut = admitted.run(call, |_| Outcome::Read);
            (TaskKind::Read, Box::pin(fut))
        }
    }
}

/// Reads a page from the cache. `anchor` is the window edge for older/newer reads.
pub(super) async fn cache_history(
    cache: CacheHandle,
    cgen: ClientGen,
    chat: ChatId,
    view: ViewGen,
    dir: HistoryDir,
    anchor: Option<MsgId>,
) -> Done {
    let res = match (dir, anchor) {
        (HistoryDir::Older, Some(id)) => cache.before(chat, id, PAGE).await,
        (HistoryDir::Newer, Some(id)) => cache.after(chat, id, PAGE).await,
        _ => cache.tail(chat, PAGE).await,
    };
    Done {
        cgen,
        parked: None,
        outcome: Outcome::CacheHistory {
            chat,
            view,
            dir,
            anchor,
            messages: res.ok(),
        },
    }
}

/// Forwards live updates to the actor until the stream fails or the actor goes away.
pub(super) async fn consume_updates<B: Backend>(
    backend: Arc<B>,
    cgen: ClientGen,
    tx: tokio::sync::mpsc::Sender<(ClientGen, Update)>,
) -> Done {
    loop {
        match backend.next_update().await {
            Ok(update) => {
                if tx.send((cgen, update)).await.is_err() {
                    return Done {
                        cgen,
                        parked: None,
                        outcome: Outcome::UpdatesEnded(BackendError::Transient),
                    };
                }
            }
            Err(e) => {
                return Done {
                    cgen,
                    parked: None,
                    outcome: Outcome::UpdatesEnded(e),
                };
            }
        }
    }
}
