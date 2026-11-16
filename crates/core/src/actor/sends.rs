//! Logical sends (plan section 9). One `random_id` per logical send, reused on
//! every retry so the server deduplicates; the table is bounded.

use std::collections::HashMap;

use super::Actor;
use super::jobs::{CallError, Job};
use crate::api::{Event, SendState, UserError};
use crate::backend::{Backend, BackendError};
use crate::model::{ChatId, LocalId, MsgId};

/// Maximum sends tracked at once (in flight, failed or unknown).
pub(super) const MAX_PENDING_SENDS: usize = 256;
/// Automatic retries after a transient failure or time-out, before `Unknown`.
pub(super) const AUTO_RETRIES: u8 = 2;
/// Telegram's limit for a text message, in characters.
pub(super) const MAX_SEND_CHARS: usize = 4096;

#[derive(Debug)]
struct Pending {
    chat: ChatId,
    text: String,
    random_id: i64,
    /// Attempts made since the last user action.
    attempts: u8,
    state: SendState,
}

#[derive(Debug, Default)]
pub(super) struct PendingSends {
    map: HashMap<LocalId, Pending>,
}

impl PendingSends {
    /// Marks a send whose task died (panic) as unknown. Returns the new state.
    pub(super) fn mark_unknown(&mut self, id: LocalId) -> Option<SendState> {
        let p = self.map.get_mut(&id)?;
        p.state = SendState::Unknown {
            attempts: p.attempts,
        };
        Some(p.state)
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
        };
        self.sends.map.insert(local_id, pending);
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

    async fn dispatch_send(&mut self, local_id: LocalId) {
        let Some(job) = self.sends.job(local_id) else {
            return;
        };
        if let Some(p) = self.sends.map.get_mut(&local_id) {
            p.attempts = p.attempts.saturating_add(1);
            p.state = SendState::Sending;
        }
        if self.enqueue(job).await {
            self.send_state(local_id, SendState::Sending).await;
        } else {
            self.finish_send(local_id, SendState::Failed).await;
        }
    }

    async fn finish_send(&mut self, local_id: LocalId, state: SendState) {
        if let Some(p) = self.sends.map.get_mut(&local_id) {
            p.state = state;
        }
        self.send_state(local_id, state).await;
    }

    /// Applies a send outcome. Runs regardless of client generation: the logical
    /// send outlives a rebuild.
    pub(super) async fn on_send(&mut self, local_id: LocalId, res: Result<MsgId, CallError>) {
        let Some(attempts) = self.sends.map.get(&local_id).map(|p| p.attempts) else {
            return;
        };
        match res {
            Ok(id) => {
                self.sends.map.remove(&local_id);
                self.send_state(local_id, SendState::Sent { id }).await;
            }
            Err(CallError::Backend(BackendError::Transient) | CallError::Timeout) => {
                if attempts <= AUTO_RETRIES {
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
}
