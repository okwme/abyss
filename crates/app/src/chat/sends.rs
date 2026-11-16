//! Echoes: the UI's copy of the user's own sends until the server's message
//! takes their place (plan section 9). Core owns the outcome (the pending-send
//! table, `random_id`, retries); this only shows it and offers Retry and
//! Discard where core accepts them.

use abyss_core::api::{Command, SendState};
use abyss_core::model::{ChatId, LocalId, MsgId};

/// Same bound as core's pending-send table: an echo exists only while core
/// may still report on its send.
pub(crate) const MAX_ECHOES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Echo {
    pub(crate) local_id: LocalId,
    pub(crate) chat: ChatId,
    pub(crate) text: String,
    pub(crate) state: SendState,
    /// Unix seconds when the user sent it.
    pub(crate) date: i64,
}

impl Echo {
    /// Retry and Discard are offered only where core accepts them.
    pub(crate) fn actionable(&self) -> bool {
        matches!(self.state, SendState::Failed | SendState::Unknown { .. })
    }
}

#[derive(Debug, Default)]
pub(crate) struct Echoes {
    /// Oldest first.
    list: Vec<Echo>,
}

/// The echo table is full; the composer keeps the text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Full;

impl Echoes {
    pub(crate) fn push(&mut self, echo: Echo) -> Result<(), Full> {
        if self.list.len() >= MAX_ECHOES {
            return Err(Full);
        }
        self.list.push(echo);
        Ok(())
    }

    pub(crate) fn for_chat(&self, chat: ChatId) -> impl Iterator<Item = &Echo> {
        self.list.iter().filter(move |e| e.chat == chat)
    }

    pub(crate) fn count(&self, chat: ChatId) -> usize {
        self.for_chat(chat).count()
    }

    pub(crate) fn count_all(&self) -> usize {
        self.list.len()
    }

    /// Applies a `SendState` event. Returns the echo's chat if it changed.
    pub(crate) fn set_state(&mut self, local_id: LocalId, state: SendState) -> Option<ChatId> {
        let echo = self.list.iter_mut().find(|e| e.local_id == local_id)?;
        echo.state = state;
        Some(echo.chat)
    }

    /// Retry with core's original `random_id`. Optimistically shows
    /// "Sending…"; core reports the real state.
    pub(crate) fn retry(&mut self, local_id: LocalId) -> Option<Command> {
        let echo = self
            .list
            .iter_mut()
            .find(|e| e.local_id == local_id && e.actionable())?;
        echo.state = SendState::Sending;
        Some(Command::RetrySend(local_id))
    }

    /// Drops a failed or unknown send, here and in core's table.
    pub(crate) fn discard(&mut self, local_id: LocalId) -> Option<Command> {
        let at = self
            .list
            .iter()
            .position(|e| e.local_id == local_id && e.actionable())?;
        self.list.remove(at);
        Some(Command::DiscardSend(local_id))
    }

    /// Drops sent echoes of `chat` whose server copy is now shown. Returns
    /// whether any went.
    pub(crate) fn resolve(&mut self, chat: ChatId, shown: impl Fn(MsgId) -> bool) -> bool {
        let before = self.list.len();
        self.list.retain(|e| {
            let SendState::Sent { id } = e.state else {
                return true;
            };
            e.chat != chat || !shown(id)
        });
        self.list.len() != before
    }

    /// `chat` was reloaded from the server (a new view). Its history is now
    /// authoritative: sent echoes at or below its newest message are in it
    /// (or were deleted), and an unconfirmed one shows up from history if it
    /// was sent, never matched by text (plan section 9).
    pub(crate) fn reconciled(&mut self, chat: ChatId, newest: Option<MsgId>) -> bool {
        let before = self.list.len();
        self.list.retain(|e| {
            e.chat != chat
                || match e.state {
                    SendState::SentUnconfirmed => false,
                    SendState::Sent { id } => newest.is_none_or(|n| id > n),
                    _ => true,
                }
        });
        self.list.len() != before
    }
}

#[cfg(test)]
mod tests {
    use abyss_core::model::PeerKind;

    use super::*;

    const A: ChatId = ChatId {
        kind: PeerKind::User,
        id: 1,
    };
    const B: ChatId = ChatId {
        kind: PeerKind::User,
        id: 2,
    };

    fn echo(n: u64, chat: ChatId) -> Echo {
        Echo {
            local_id: LocalId(n),
            chat,
            text: format!("t{n}"),
            state: SendState::Sending,
            date: 0,
        }
    }

    fn states(e: &Echoes, chat: ChatId) -> Vec<SendState> {
        e.for_chat(chat).map(|e| e.state).collect()
    }

    #[test]
    fn retry_and_discard_only_when_core_accepts_them() {
        let mut e = Echoes::default();
        e.push(echo(1, A)).unwrap();
        assert!(e.retry(LocalId(1)).is_none(), "in flight");
        assert!(e.discard(LocalId(1)).is_none(), "in flight");
        e.set_state(LocalId(1), SendState::Failed);
        assert!(matches!(
            e.retry(LocalId(1)),
            Some(Command::RetrySend(LocalId(1)))
        ));
        assert_eq!(states(&e, A), [SendState::Sending], "optimistic");
        assert!(e.retry(LocalId(9)).is_none(), "unknown id");
        e.set_state(LocalId(1), SendState::Unknown { attempts: 3 });
        assert!(matches!(
            e.discard(LocalId(1)),
            Some(Command::DiscardSend(LocalId(1)))
        ));
        assert_eq!(e.count(A), 0);
        assert_eq!(
            e.set_state(LocalId(1), SendState::Failed),
            None,
            "late event"
        );
    }

    #[test]
    fn sent_echo_gives_way_to_the_server_copy() {
        let mut e = Echoes::default();
        e.push(echo(1, A)).unwrap();
        e.push(echo(2, A)).unwrap();
        e.push(echo(3, B)).unwrap();
        e.set_state(LocalId(1), SendState::Sent { id: MsgId(10) });
        e.set_state(LocalId(3), SendState::Sent { id: MsgId(10) });
        assert!(!e.resolve(A, |id| id == MsgId(11)));
        assert!(e.resolve(A, |id| id == MsgId(10)));
        assert_eq!(states(&e, A), [SendState::Sending]);
        assert_eq!(e.count(B), 1, "other chats are untouched");
    }

    #[test]
    fn reload_drops_unconfirmed_and_covered_echoes() {
        let mut e = Echoes::default();
        for n in 1..=4 {
            e.push(echo(n, A)).unwrap();
        }
        e.set_state(LocalId(1), SendState::SentUnconfirmed);
        e.set_state(LocalId(2), SendState::Sent { id: MsgId(5) });
        e.set_state(LocalId(3), SendState::Sent { id: MsgId(50) });
        assert!(e.reconciled(A, Some(MsgId(20))));
        assert_eq!(
            states(&e, A),
            [SendState::Sent { id: MsgId(50) }, SendState::Sending]
        );
    }

    #[test]
    fn bounded() {
        let mut e = Echoes::default();
        for n in 0..MAX_ECHOES {
            e.push(echo(u64::try_from(n).unwrap(), A)).unwrap();
        }
        assert_eq!(e.push(echo(9999, A)), Err(Full));
    }
}
