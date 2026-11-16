//! Logical sends (plan section 9). One `random_id` per logical send, reused on
//! every retry so the server deduplicates; the table is bounded and outlives
//! client instances.
//!
//! Outcomes:
//! - a definite rejection is `Failed` (manual retry, same `random_id`);
//! - an unknown outcome (time-out, transport, dropped by a rebuild) retries
//!   automatically up to [`MAX_ATTEMPTS`] across rebuilds, then `Unknown`;
//! - `FLOOD_WAIT` and an authenticated `303` are *not* rejections: the server
//!   did not run the request. A flood wait is held as `FloodWait` and resent
//!   when it expires; a migration stays `Sending` and replays after the
//!   rebuild it triggers. Neither consumes an attempt.
//! - The send's server id comes from the RPC response or from a stream
//!   `updateMessageID`, whichever arrives first; the other is a no-op. A
//!   `SentUnconfirmed` send is remembered (bounded) so a later stream
//!   confirmation still resolves it to `Sent`.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use tokio::time::Instant;

use super::Actor;
use super::jobs::{CallError, Job};
use super::lifecycle::Phase;
use crate::api::{Event, SendState, UserError};
use crate::backend::{Backend, BackendError};
use crate::model::{ChatId, LocalId, MsgId};

/// Maximum sends tracked at once (in flight, failed, waiting or unknown).
pub(super) const MAX_PENDING_SENDS: usize = 256;
/// Automatic retries after a transient failure or time-out, before `Unknown`.
/// The cap counts across rebuilds.
pub(super) const AUTO_RETRIES: u8 = 2;
/// Attempts after which a send is no longer replayed automatically.
const MAX_ATTEMPTS: u8 = AUTO_RETRIES + 1;
/// Telegram's limit for a text message, in characters.
pub(super) const MAX_SEND_CHARS: usize = 4096;
/// `SentUnconfirmed` sends remembered for a late stream `updateMessageID`.
const MAX_UNCONFIRMED: usize = MAX_PENDING_SENDS;

#[derive(Debug)]
struct Pending {
    chat: ChatId,
    text: String,
    random_id: i64,
    /// Attempts made since the last user action.
    attempts: u8,
    state: SendState,
    /// Queued or running on the live instance.
    in_flight: bool,
    /// While `FloodWait`: when the send may go again.
    retry_at: Option<Instant>,
}

#[derive(Debug, Default)]
pub(super) struct PendingSends {
    map: HashMap<LocalId, Pending>,
    /// `(random_id, local_id)` of sends that ended `SentUnconfirmed`, oldest
    /// first, at most [`MAX_UNCONFIRMED`].
    unconfirmed: VecDeque<(i64, LocalId)>,
    /// Bumped whenever a send enters or leaves the table (the required peer
    /// set depends on it).
    rev: u64,
}

impl PendingSends {
    fn insert(&mut self, id: LocalId, p: Pending) {
        self.map.insert(id, p);
        self.rev = self.rev.wrapping_add(1);
    }

    fn remove(&mut self, id: LocalId) -> Option<Pending> {
        let p = self.map.remove(&id)?;
        self.rev = self.rev.wrapping_add(1);
        Some(p)
    }

    pub(super) fn rev(&self) -> u64 {
        self.rev
    }

    /// Marks a send whose task died (panic) as unknown. Returns the new state.
    pub(super) fn mark_unknown(&mut self, id: LocalId) -> Option<SendState> {
        let p = self.map.get_mut(&id)?;
        p.in_flight = false;
        p.state = SendState::Unknown {
            attempts: p.attempts,
        };
        Some(p.state)
    }

    /// Chats with an unresolved send, in `LocalId` order, for the peers
    /// carried across a rebuild and the required peer set.
    pub(super) fn chats(&self) -> Vec<ChatId> {
        let mut ids: Vec<(&LocalId, &Pending)> = self.map.iter().collect();
        ids.sort_unstable_by_key(|(id, _)| **id);
        ids.into_iter().map(|(_, p)| p.chat).collect()
    }

    fn job(&self, id: LocalId) -> Option<Job> {
        let p = self.map.get(&id)?;
        Some(Job::Send {
            local_id: id,
            chat: p.chat,
            text: p.text.clone(),
            random_id: p.random_id,
        })
    }

    /// Sends to replay once an instance is online: `Sending` ones that are not
    /// queued, and `Unknown` ones still under the attempt cap. In id order.
    fn replayable(&self) -> Vec<LocalId> {
        let mut ids: Vec<LocalId> = self
            .map
            .iter()
            .filter(|(_, p)| !p.in_flight)
            .filter(|(_, p)| match p.state {
                SendState::Sending => true,
                SendState::Unknown { .. } => p.attempts < MAX_ATTEMPTS,
                _ => false,
            })
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// When the earliest flood wait ends, if any send is waiting.
    pub(super) fn next_due(&self) -> Option<Instant> {
        self.map.values().filter_map(|p| p.retry_at).min()
    }

    /// Flood waits that have ended, in id order; their deadlines are cleared.
    fn take_due(&mut self, now: Instant) -> Vec<LocalId> {
        let mut ids: Vec<LocalId> = self
            .map
            .iter_mut()
            .filter(|(_, p)| p.retry_at.is_some_and(|at| at <= now))
            .map(|(id, p)| {
                p.retry_at = None;
                *id
            })
            .collect();
        ids.sort_unstable();
        ids
    }

    fn remember_unconfirmed(&mut self, random_id: i64, id: LocalId) {
        if self.unconfirmed.len() >= MAX_UNCONFIRMED {
            self.unconfirmed.pop_front();
        }
        self.unconfirmed.push_back((random_id, id));
    }

    /// The send `random_id` belongs to: a pending one (removed from the
    /// table), or a remembered `SentUnconfirmed` one (forgotten).
    fn resolve(&mut self, random_id: i64) -> Option<LocalId> {
        let pending = self
            .map
            .iter()
            .find(|(_, p)| p.random_id == random_id)
            .map(|(id, _)| *id);
        if let Some(id) = pending {
            self.remove(id);
            return Some(id);
        }
        let at = self.unconfirmed.iter().position(|(r, _)| *r == random_id)?;
        self.unconfirmed.remove(at).map(|(_, id)| id)
    }
}

/// A random, nonzero `random_id`.
fn random_id() -> Option<i64> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).ok()?;
    Some(i64::from_le_bytes(bytes)).filter(|id| *id != 0)
}

fn sendable(text: &str) -> bool {
    !text.trim().is_empty() && text.chars().count() <= MAX_SEND_CHARS
}

impl<B: Backend> Actor<B> {
    async fn send_state(&self, local_id: LocalId, state: SendState) {
        self.emit(Event::SendState { local_id, state }).await;
    }

    pub(super) async fn send_text(&mut self, chat: ChatId, text: String, local_id: LocalId) {
        if self.sends.map.contains_key(&local_id) {
            self.error(UserError::Busy).await;
            return;
        }
        if self.sends.map.len() >= MAX_PENDING_SENDS {
            self.error(UserError::Busy).await;
            return self.send_state(local_id, SendState::Failed).await;
        }
        if !sendable(&text) {
            self.error(UserError::Rejected).await;
            return self.send_state(local_id, SendState::Failed).await;
        }
        let Some(random_id) = random_id() else {
            self.error(UserError::Internal).await;
            return self.send_state(local_id, SendState::Failed).await;
        };
        let pending = Pending {
            chat,
            text,
            random_id,
            attempts: 0,
            state: SendState::Sending,
            in_flight: false,
            retry_at: None,
        };
        self.sends.insert(local_id, pending);
        self.dispatch_send(local_id).await;
    }

    /// Retries a failed or unknown send with its original `random_id`.
    pub(super) async fn retry_send(&mut self, local_id: LocalId) {
        let Some(p) = self.sends.map.get_mut(&local_id) else {
            return self.error(UserError::NotFound).await;
        };
        if !matches!(p.state, SendState::Failed | SendState::Unknown { .. }) {
            return self.error(UserError::Busy).await;
        }
        p.attempts = 0;
        self.dispatch_send(local_id).await;
    }

    /// Removes a failed, unknown or flood-waiting send from the table,
    /// freeing its slot.
    pub(super) async fn discard_send(&mut self, local_id: LocalId) {
        let Some(p) = self.sends.map.get(&local_id) else {
            return self.error(UserError::NotFound).await;
        };
        let settled = matches!(
            p.state,
            SendState::Failed | SendState::Unknown { .. } | SendState::FloodWait { .. }
        );
        if p.in_flight || !settled {
            return self.error(UserError::Busy).await;
        }
        self.sends.remove(local_id);
    }

    /// Queues an attempt on the live instance. Without one, the send stays
    /// `Sending` and is replayed once an instance is online.
    async fn dispatch_send(&mut self, local_id: LocalId) {
        let live = self.life.phase == Phase::Live;
        let Some(p) = self.sends.map.get_mut(&local_id) else {
            return;
        };
        p.state = SendState::Sending;
        p.retry_at = None;
        if !live {
            return self.send_state(local_id, SendState::Sending).await;
        }
        p.attempts = p.attempts.saturating_add(1);
        p.in_flight = true;
        let Some(job) = self.sends.job(local_id) else {
            return;
        };
        if self.enqueue(job).await {
            self.send_state(local_id, SendState::Sending).await;
        } else {
            self.finish_send(local_id, SendState::Failed).await;
        }
    }

    async fn finish_send(&mut self, local_id: LocalId, state: SendState) {
        if let Some(p) = self.sends.map.get_mut(&local_id) {
            p.state = state;
            p.in_flight = false;
        }
        self.send_state(local_id, state).await;
    }

    /// Replays unresolved sends with their original `random_id`s (after `Online`).
    pub(super) async fn replay_sends(&mut self) {
        for local_id in self.sends.replayable() {
            self.dispatch_send(local_id).await;
        }
    }

    /// Resends the sends whose flood wait has ended (or keeps them `Sending`
    /// for the next `Online`).
    pub(super) async fn resume_flooded_sends(&mut self) {
        for local_id in self.sends.take_due(Instant::now()) {
            self.dispatch_send(local_id).await;
        }
    }

    /// A stream `updateMessageID`: the send carrying `random_id` has server
    /// id `id`. Resolves a pending (any state) or `SentUnconfirmed` send to
    /// `Sent`; a later RPC answer for it then finds nothing and is a no-op.
    pub(super) async fn on_message_id(&mut self, random_id: i64, id: MsgId) {
        if let Some(local_id) = self.sends.resolve(random_id) {
            self.send_state(local_id, SendState::Sent { id }).await;
        }
    }

    /// Applies a send outcome. Runs regardless of client generation: the logical
    /// send outlives a rebuild. A send already resolved (by a stream
    /// `updateMessageID`, or discarded) is not in the table: nothing happens.
    pub(super) async fn on_send(&mut self, local_id: LocalId, res: Result<MsgId, CallError>) {
        let Some(p) = self.sends.map.get_mut(&local_id) else {
            return;
        };
        p.in_flight = false;
        let attempts = p.attempts;
        match res {
            Ok(id) => {
                self.sends.remove(local_id);
                self.send_state(local_id, SendState::Sent { id }).await;
            }
            Err(CallError::Backend(BackendError::SentUnconfirmed)) => {
                // Accepted without an id: never replayed, so it leaves the
                // table; the echo is matched by a later reconcile or a stream
                // `updateMessageID`.
                if let Some(p) = self.sends.remove(local_id) {
                    self.sends.remember_unconfirmed(p.random_id, local_id);
                }
                self.send_state(local_id, SendState::SentUnconfirmed).await;
            }
            Err(CallError::Backend(BackendError::FloodWait { seconds })) => {
                self.hold_flooded(local_id, seconds).await;
            }
            Err(CallError::Backend(BackendError::Migrated)) => {
                // Not executed; the rebuild this triggers replays it after
                // `Online` (or after a new sign-in if migration halts).
                p.attempts = attempts.saturating_sub(1);
                p.state = SendState::Sending;
                self.error(UserError::AccountMigrated).await;
            }
            Err(
                CallError::Backend(BackendError::Transient)
                | CallError::Timeout
                | CallError::Dropped,
            ) => {
                if attempts < MAX_ATTEMPTS {
                    self.dispatch_send(local_id).await;
                } else {
                    self.finish_send(local_id, SendState::Unknown { attempts })
                        .await;
                }
            }
            Err(CallError::Backend(e)) => {
                self.error(super::apply::user_error(CallError::Backend(e)))
                    .await;
                self.finish_send(local_id, SendState::Failed).await;
            }
        }
    }

    /// `FLOOD_WAIT`: the server did not run the send. Hold it for `seconds`
    /// without consuming an attempt, then resend with the same `random_id`.
    async fn hold_flooded(&mut self, local_id: LocalId, seconds: u32) {
        let Some(p) = self.sends.map.get_mut(&local_id) else {
            return;
        };
        p.attempts = p.attempts.saturating_sub(1);
        p.retry_at = Some(Instant::now() + Duration::from_secs(u64::from(seconds)));
        self.finish_send(local_id, SendState::FloodWait { seconds })
            .await;
    }
}
