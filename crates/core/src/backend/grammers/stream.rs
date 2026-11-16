//! The update stream of one instance (plan section 2, "Bootstrap").
//!
//! grammers' first `next` sends `updates.getState` and records it through
//! `AbyssSession::set_update_state`, which flips the session's
//! `baseline_established` flag, *before* it waits for updates. That wait can
//! last indefinitely, so the baseline is reported by racing the flag against
//! the pending pull.
//!
//! A grammers pull is never cancelled midway (it may be in the middle of
//! `getDifference`, whose result would be lost): the pull future owns the
//! grammers stream and hands it back on completion, and a pull interrupted by
//! the baseline is kept and resumed by the next `next` call. The racing logic
//! is generic over [`Source`] so it is tested without a network.

use std::future::Future;
use std::pin::Pin;

use grammers_client::InvocationError;
use grammers_client::client::UpdateStream as GrammersStream;
use grammers_client::peer::{Peer, PeerMap};
use grammers_client::session::updates::State;
use grammers_client::tl;
use tokio::sync::watch;

use super::errors::{kind, loggable_name, rpc_code, stream_error};
use super::map::{self, Directory};
use crate::backend::{StreamError, StreamItem, Update, UpdateStream};

/// A pull that owns its source and hands it back.
pub(crate) type Pull<S> =
    Pin<Box<dyn Future<Output = (S, Result<<S as Source>::Raw, InvocationError>)> + Send>>;

/// Where raw updates come from: grammers' stream, or a script in tests.
pub(crate) trait Source: Send + Sized + 'static {
    type Raw: Send;

    fn pull(self) -> Pull<Self>;

    /// The domain update, or `None` for updates Stage 1 ignores.
    fn map(raw: &Self::Raw) -> Option<Update>;
}

impl Source for GrammersStream {
    type Raw = (tl::enums::Update, State, PeerMap);

    fn pull(mut self) -> Pull<Self> {
        Box::pin(async move {
            let res = self.next_raw().await;
            (self, res)
        })
    }

    fn map(raw: &Self::Raw) -> Option<Update> {
        let (update, _, peers) = raw;
        map::update(update, &directory(peers))
    }
}

/// The peers delivered with an update, as a mapping directory.
fn directory(peers: &PeerMap) -> Directory {
    let mut dir = Directory::default();
    for peer in peers.iter_peers().take(map::MAX_DIRECTORY) {
        match peer {
            Peer::User(u) => dir.insert_user(&u.raw),
            Peer::Group(g) => dir.insert_chat(&g.raw),
            Peer::Channel(c) => dir.insert_chat(&tl::enums::Chat::Channel(c.raw.clone())),
            Peer::Community(_) => {}
        }
    }
    dir
}

/// The baseline race over any [`Source`].
pub(crate) struct Updates<S: Source> {
    /// The source while no pull is pending.
    idle: Option<S>,
    pull: Option<Pull<S>>,
    baseline: watch::Receiver<bool>,
    /// The baseline is still to be reported.
    baseline_pending: bool,
    /// A mapped update that arrived together with the baseline.
    ready: Option<Update>,
    /// Updates Stage 1 does not handle, for debug logs.
    ignored: u64,
}

impl<S: Source> Updates<S> {
    pub(crate) fn new(source: S, baseline: watch::Receiver<bool>) -> Self {
        Self {
            idle: Some(source),
            pull: None,
            baseline,
            baseline_pending: true,
            ready: None,
            ignored: 0,
        }
    }

    fn accept(&mut self, raw: &S::Raw) {
        self.ready = S::map(raw);
        if self.ready.is_none() {
            self.ignored = self.ignored.saturating_add(1);
            tracing::trace!(ignored = self.ignored, "update ignored");
        }
    }

    pub(crate) async fn next(&mut self) -> Result<StreamItem, StreamError> {
        loop {
            if self.baseline_pending && *self.baseline.borrow() {
                self.baseline_pending = false;
                return Ok(StreamItem::Baseline);
            }
            if let Some(update) = self.ready.take() {
                return Ok(StreamItem::Update(update));
            }
            let mut pending = match (self.pull.take(), self.idle.take()) {
                (Some(p), _) => p,
                (None, Some(source)) => source.pull(),
                // Only reachable if a pull panicked: the source went with it.
                (None, None) => return Err(StreamError::Fatal),
            };
            let waiting = self.baseline_pending;
            tokio::select! {
                biased;
                changed = self.baseline.changed(), if waiting => {
                    self.pull = Some(pending);
                    if changed.is_err() {
                        // The session is gone; no baseline will be recorded.
                        self.baseline_pending = false;
                    }
                }
                (source, res) = &mut pending => {
                    self.idle = Some(source);
                    match res {
                        Ok(raw) => self.accept(&raw),
                        Err(e) => {
                            tracing::debug!(
                                error = loggable_name(&e),
                                code = rpc_code(&e),
                                "update stream failed"
                            );
                            return Err(stream_error(kind(&e)));
                        }
                    }
                }
            }
        }
    }
}

/// Live updates of one grammers instance.
pub struct GrammersUpdates(Updates<GrammersStream>);

impl std::fmt::Debug for GrammersUpdates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrammersUpdates")
            .field("baseline_pending", &self.0.baseline_pending)
            .field("ignored", &self.0.ignored)
            .finish_non_exhaustive()
    }
}

impl GrammersUpdates {
    pub(crate) fn new(stream: GrammersStream, baseline: watch::Receiver<bool>) -> Self {
        Self(Updates::new(stream, baseline))
    }
}

impl UpdateStream for GrammersUpdates {
    async fn next(&mut self) -> Result<StreamItem, StreamError> {
        self.0.next().await
    }
}
