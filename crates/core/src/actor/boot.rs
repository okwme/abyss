//! Bootstrap and light refresh (plan section 2): take snapshots only after the
//! update baseline, buffer live updates meanwhile, then merge the buffer over the
//! snapshots so a stale snapshot never overwrites a newer update.
//!
//! - **Full bootstrap** (after every build): wait for the stream's baseline (at
//!   most [`BASELINE_WAIT`], else rebuild), invalidate every message window, then
//!   fetch the first dialogs page and the open chat's tail.
//! - **Light refresh** (window activation, and every [`REFRESH_EVERY`] while
//!   active): the same snapshots on the live instance, without invalidation; the
//!   open chat's tail is merged into its window rather than replacing it.
//!
//! In both, a deletion seen while snapshots are pending goes into a bounded
//! tombstone set, so a snapshot fetched later cannot resurrect the message.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use super::Actor;
use super::jobs::{self, Job, TaskKind};
use super::lifecycle::{Phase, Reason};
use crate::api::{Event, HistoryDir, WindowState};
use crate::backend::{Backend, HistoryQuery, StreamItem, Update};
use crate::model::{ChatId, ChatSummary, MsgId};
use crate::store::ViewGen;

/// How long a new instance may take to establish its update baseline.
pub(super) const BASELINE_WAIT: Duration = Duration::from_secs(10);
/// Updates buffered while snapshots are pending; overflow rebuilds.
pub(super) const MAX_BOOT_BUFFER: usize = 4096;
/// Deleted ids remembered while snapshots are pending; overflow rebuilds.
pub(super) const MAX_TOMBSTONES: usize = 4096;
/// Light refresh period while the window is active.
pub(super) const REFRESH_EVERY: Duration = Duration::from_mins(5);
/// Activation does not refresh again within this long of the last refresh.
pub(super) const MIN_REFRESH_GAP: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BootKind {
    Full,
    Refresh,
}

/// Which snapshot finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Snapshot {
    Dialogs,
    Tail(ChatId),
}

/// Too many updates or deletions arrived while snapshots were pending.
#[derive(Debug)]
pub(super) struct Overflow;

#[derive(Debug)]
pub(super) struct Boot {
    pub(super) kind: BootKind,
    /// Deadline for the baseline; `None` once it is established.
    baseline_by: Option<Instant>,
    buffer: Vec<Update>,
    tombstones: HashSet<(Option<ChatId>, MsgId)>,
    dialogs_pending: bool,
    tails_pending: HashSet<ChatId>,
    /// `last_date` per chat in the dialogs snapshot (at most one page).
    dates: HashMap<ChatId, i64>,
}

impl Boot {
    pub(super) fn full(now: Instant) -> Self {
        Self {
            kind: BootKind::Full,
            baseline_by: Some(now + BASELINE_WAIT),
            buffer: Vec::new(),
            tombstones: HashSet::new(),
            dialogs_pending: false,
            tails_pending: HashSet::new(),
            dates: HashMap::new(),
        }
    }

    fn refresh() -> Self {
        Self {
            kind: BootKind::Refresh,
            baseline_by: None,
            ..Self::full(Instant::now())
        }
    }

    pub(super) fn baseline_deadline(&self) -> Option<Instant> {
        self.baseline_by
    }

    /// Buffers `update`, recording deletions as tombstones.
    fn push(&mut self, update: Update) -> Result<(), Overflow> {
        if self.buffer.len() >= MAX_BOOT_BUFFER {
            return Err(Overflow);
        }
        if let Update::Deleted { chat, ids } = &update {
            for id in ids {
                if self.tombstones.len() >= MAX_TOMBSTONES {
                    return Err(Overflow);
                }
                self.tombstones.insert((*chat, *id));
            }
        }
        self.buffer.push(update);
        Ok(())
    }

    /// Whether `id` in `chat` was deleted while snapshots were pending.
    pub(super) fn is_dead(&self, chat: ChatId, id: MsgId) -> bool {
        self.tombstones.contains(&(Some(chat), id))
            || (chat.kind.has_account_unique_ids() && self.tombstones.contains(&(None, id)))
    }

    /// Whether the tail of `chat` is one of this boot's snapshots.
    pub(super) fn awaits_tail(&self, chat: ChatId) -> bool {
        self.tails_pending.contains(&chat)
    }

    /// Records what the dialogs snapshot says about each chat's last message.
    pub(super) fn note_dialogs(&mut self, chats: &[ChatSummary]) {
        self.dates.extend(chats.iter().map(|c| (c.id, c.last_date)));
    }

    fn snapshots_done(&self) -> bool {
        self.baseline_by.is_none() && !self.dialogs_pending && self.tails_pending.is_empty()
    }
}

/// Where an update being applied comes from.
#[derive(Clone, Copy, Debug)]
pub(super) enum Origin<'a> {
    Live,
    /// Buffered during snapshots and merged over them; carries the dialogs
    /// snapshot's `last_date` per chat.
    Replay(&'a HashMap<ChatId, i64>),
}

impl<B: Backend> Actor<B> {
    pub(super) async fn on_feed(&mut self, item: StreamItem) {
        match item {
            StreamItem::Baseline => self.on_baseline().await,
            StreamItem::Update(update) => match self.boot.as_mut() {
                Some(boot) => {
                    if boot.push(update).is_err() {
                        self.rebuild(Reason::BootOverflow).await;
                    }
                }
                None => self.on_update(update, Origin::Live).await,
            },
        }
    }

    async fn on_baseline(&mut self) {
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        if boot.baseline_by.take().is_none() {
            return; // a second baseline on the same stream: nothing to do
        }
        self.start_snapshots(BootKind::Full).await;
    }

    /// A light refresh. Single-flight; skipped unless the instance is live.
    pub(super) async fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        if self.life.phase != Phase::Live || self.boot.is_some() {
            return;
        }
        self.boot = Some(Boot::refresh());
        self.start_snapshots(BootKind::Refresh).await;
    }

    /// When the next periodic refresh is due, if one is.
    pub(super) fn refresh_due(&self) -> Option<Instant> {
        (self.window == WindowState::Active
            && self.life.phase == Phase::Live
            && self.boot.is_none())
        .then(|| self.last_refresh + REFRESH_EVERY)
    }

    pub(super) async fn on_window(&mut self, state: WindowState) {
        self.window = state;
        if state == WindowState::Active
            && Instant::now().duration_since(self.last_refresh) >= MIN_REFRESH_GAP
        {
            self.refresh().await;
        }
    }

    async fn start_snapshots(&mut self, kind: BootKind) {
        if kind == BootKind::Full {
            self.purge_queued();
            self.contiguous.clear();
            for (chat, _) in self.windows.invalidate_all() {
                self.emit(Event::ChatInvalidated(chat)).await;
            }
        }
        let dialogs = !self.dialogs_busy;
        if dialogs {
            self.dialogs_busy = true;
            self.priority.push_back(Job::Dialogs { cursor: None });
        }
        // A full bootstrap always refetches the open tail (any older load was
        // for a window that is now invalidated); a refresh never doubles a load.
        let tail = self
            .open
            .filter(|c| kind == BootKind::Full || !self.history_busy.contains_key(c))
            .and_then(|c| Some((c, self.windows.get(c)?.view_gen())));
        if let Some(boot) = self.boot.as_mut() {
            boot.dialogs_pending = dialogs;
            boot.tails_pending.extend(tail.map(|(c, _)| c));
        }
        if let Some((chat, view)) = tail {
            self.history_busy.insert(chat, view);
            match self.cache.clone().filter(|_| kind == BootKind::Full) {
                // Show the cached tail again right away (it may be all there is
                // while offline); the server tail is chained after it.
                Some(cache) => {
                    let cgen = self.cgen;
                    let fut = jobs::cache_history(cache, cgen, chat, view, HistoryDir::Tail, None);
                    self.spawn_local(TaskKind::History(chat), fut);
                }
                None => self.push_tail_snapshot(chat, view),
            }
        }
        self.maybe_finish_boot().await;
    }

    /// Queues the server tail of `chat` as a snapshot of the running boot.
    pub(super) fn push_tail_snapshot(&mut self, chat: ChatId, view: ViewGen) {
        self.priority.push_back(Job::History {
            chat,
            view,
            dir: HistoryDir::Tail,
            query: HistoryQuery::Tail,
        });
    }

    /// Whether the history load `(chat, view)` is the running boot's tail snapshot.
    pub(super) fn is_tail_snapshot(&self, chat: ChatId, view: ViewGen) -> bool {
        self.boot.as_ref().is_some_and(|b| b.awaits_tail(chat))
            && self.history_busy.get(&chat) == Some(&view)
    }

    /// Records a finished snapshot (successful or not).
    pub(super) async fn snapshot_done(&mut self, snapshot: Snapshot) {
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        match snapshot {
            Snapshot::Dialogs => boot.dialogs_pending = false,
            Snapshot::Tail(chat) => {
                boot.tails_pending.remove(&chat);
            }
        }
        self.maybe_finish_boot().await;
    }

    async fn maybe_finish_boot(&mut self) {
        if !self.boot.as_ref().is_some_and(Boot::snapshots_done) {
            return;
        }
        let Some(boot) = self.boot.take() else {
            return;
        };
        for update in boot.buffer {
            self.on_update(update, Origin::Replay(&boot.dates)).await;
        }
        self.last_refresh = Instant::now();
        if boot.kind == BootKind::Full {
            self.go_online().await;
        }
    }
}

#[cfg(all(test, feature = "fake"))]
mod tests {
    use super::*;
    use crate::backend::fake::text_message;
    use crate::model::PeerKind;

    #[test]
    fn tombstones_match_scoped_and_unscoped_deletions() {
        let user = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        let group = ChatId {
            kind: PeerKind::Channel,
            id: 2,
        };
        let mut boot = Boot::full(Instant::now());
        boot.push(Update::Deleted {
            chat: None,
            ids: vec![MsgId(5)],
        })
        .unwrap();
        boot.push(Update::Deleted {
            chat: Some(group),
            ids: vec![MsgId(6)],
        })
        .unwrap();
        assert!(boot.is_dead(user, MsgId(5)));
        assert!(!boot.is_dead(group, MsgId(5)), "channel ids are per chat");
        assert!(boot.is_dead(group, MsgId(6)));
        assert!(!boot.is_dead(user, MsgId(6)));
    }

    #[test]
    fn buffer_overflow_is_reported() {
        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        let mut boot = Boot::full(Instant::now());
        for i in 0..MAX_BOOT_BUFFER {
            let id = i32::try_from(i).unwrap();
            boot.push(Update::NewMessage(text_message(chat, id, "x")))
                .unwrap();
        }
        assert!(
            boot.push(Update::NewMessage(text_message(chat, 0, "x")))
                .is_err()
        );
    }
}
