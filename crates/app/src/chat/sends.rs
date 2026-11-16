//! Echoes: the UI's copy of the user's own sends until the server's message
//! takes their place (plan section 9). Core owns the outcome (the pending-send
//! table, `random_id`, retries); this only shows it and offers Retry and
//! Discard where core accepts them. Every change that follows a command is
//! made only after core admitted the command (`ChatShell`).

use std::time::{Duration, Instant};

use abyss_core::api::SendState;
use abyss_core::model::{ChatId, LocalId, MsgId};

/// Same bound as core's pending-send table: an echo exists only while core
/// may still report on its send. A `SentUnconfirmed` echo (the server has
/// the message; only its id is unknown) gives way to a new send at the cap.
pub(crate) const MAX_ECHOES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Echo {
    pub(crate) local_id: LocalId,
    pub(crate) chat: ChatId,
    pub(crate) text: String,
    pub(crate) state: SendState,
    /// Unix seconds when the user sent it.
    pub(crate) date: i64,
    /// When core resends a flood-waiting send (`SendState::FloodWait`).
    resend_at: Option<Instant>,
}

impl Echo {
    pub(crate) fn new(local_id: LocalId, chat: ChatId, text: String, date: i64) -> Self {
        Self {
            local_id,
            chat,
            text,
            state: SendState::Sending,
            date,
            resend_at: None,
        }
    }

    /// Retry is offered only where core accepts it.
    pub(crate) fn can_retry(&self) -> bool {
        matches!(self.state, SendState::Failed | SendState::Unknown { .. })
    }

    /// Discard is offered only where core accepts it: also while a send
    /// waits out a flood wait.
    pub(crate) fn can_discard(&self) -> bool {
        self.can_retry() || matches!(self.state, SendState::FloodWait { .. })
    }

    /// Whole seconds until core resends a flood-waiting send, rounded up.
    pub(crate) fn wait_left(&self, now: Instant) -> Option<u64> {
        let left = self.resend_at?.saturating_duration_since(now);
        Some(left.as_secs() + u64::from(left.subsec_nanos() > 0))
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
    /// Whether a new send has no room: the table is at its cap and no
    /// `SentUnconfirmed` echo can give way.
    pub(crate) fn is_full(&self) -> bool {
        self.list.len() >= MAX_ECHOES && self.oldest_unconfirmed().is_none()
    }

    fn oldest_unconfirmed(&self) -> Option<usize> {
        self.list
            .iter()
            .position(|e| e.state == SendState::SentUnconfirmed)
    }

    pub(crate) fn push(&mut self, echo: Echo) -> Result<(), Full> {
        if self.list.len() >= MAX_ECHOES {
            let at = self.oldest_unconfirmed().ok_or(Full)?;
            self.list.remove(at);
        }
        self.list.push(echo);
        debug_assert!(self.list.len() <= MAX_ECHOES);
        Ok(())
    }

    pub(crate) fn get(&self, local_id: LocalId) -> Option<&Echo> {
        self.list.iter().find(|e| e.local_id == local_id)
    }

    pub(crate) fn for_chat(&self, chat: ChatId) -> impl Iterator<Item = &Echo> {
        self.list.iter().filter(move |e| e.chat == chat)
    }

    pub(crate) fn count(&self, chat: ChatId) -> usize {
        self.for_chat(chat).count()
    }

    /// Whether any echo shows a flood-wait countdown.
    pub(crate) fn any_waiting(&self) -> bool {
        self.list.iter().any(|e| e.resend_at.is_some())
    }

    /// Applies a `SendState` event received at `now`. Returns the echo's
    /// chat if it changed.
    pub(crate) fn set_state(
        &mut self,
        local_id: LocalId,
        state: SendState,
        now: Instant,
    ) -> Option<ChatId> {
        let echo = self.list.iter_mut().find(|e| e.local_id == local_id)?;
        echo.state = state;
        echo.resend_at = match state {
            SendState::FloodWait { seconds } => {
                now.checked_add(Duration::from_secs(u64::from(seconds)))
            }
            _ => None,
        };
        Some(echo.chat)
    }

    /// Core admitted a retry with the original `random_id`: shows
    /// "Sending…" until core reports the real state.
    pub(crate) fn retried(&mut self, local_id: LocalId) {
        if let Some(echo) = self.list.iter_mut().find(|e| e.local_id == local_id) {
            echo.state = SendState::Sending;
            echo.resend_at = None;
        }
    }

    /// Core admitted a discard: the send leaves its table, the echo goes.
    pub(crate) fn discarded(&mut self, local_id: LocalId) {
        self.list.retain(|e| e.local_id != local_id);
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

    /// Core reconciled `chat` with the server up to `newest`
    /// (`Event::Reconciled`). `shown` answers whether the reconciled view
    /// holds an id, when that view is the one on screen. A sent echo at or
    /// below `newest` that the view lacks was deleted. An unconfirmed echo
    /// is never matched by text (plan section 9), nor by how many tails
    /// arrived: it leaves only on core's evidence ([`Self::covered`]).
    /// Returns whether any echo went.
    pub(crate) fn reconciled(
        &mut self,
        chat: ChatId,
        newest: Option<MsgId>,
        shown: Option<&dyn Fn(MsgId) -> bool>,
    ) -> bool {
        let (Some(shown), Some(newest)) = (shown, newest) else {
            return false;
        };
        let before = self.list.len();
        self.list.retain(|e| match e.state {
            SendState::Sent { id } if e.chat == chat => id > newest || shown(id),
            _ => true,
        });
        self.list.len() != before
    }

    /// Core showed server history covering these unconfirmed sends
    /// (`Event::SendsCovered`): their messages are on screen (or deleted).
    /// Returns whether any echo went.
    pub(crate) fn covered(&mut self, sends: &[LocalId]) -> bool {
        let before = self.list.len();
        self.list
            .retain(|e| e.state != SendState::SentUnconfirmed || !sends.contains(&e.local_id));
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
        Echo::new(LocalId(n), chat, format!("t{n}"), 0)
    }

    fn states(e: &Echoes, chat: ChatId) -> Vec<SendState> {
        e.for_chat(chat).map(|e| e.state).collect()
    }

    fn set(e: &mut Echoes, n: u64, state: SendState) {
        e.set_state(LocalId(n), state, Instant::now());
    }

    #[test]
    fn retry_and_discard_only_where_core_accepts_them() {
        let mut e = Echoes::default();
        e.push(echo(1, A)).unwrap();
        let get = |e: &Echoes| e.get(LocalId(1)).unwrap().clone();
        assert!(!get(&e).can_retry(), "in flight");
        assert!(!get(&e).can_discard(), "in flight");
        set(&mut e, 1, SendState::Failed);
        assert!(get(&e).can_retry() && get(&e).can_discard());
        set(&mut e, 1, SendState::FloodWait { seconds: 30 });
        assert!(!get(&e).can_retry(), "core resends it by itself");
        assert!(get(&e).can_discard(), "a waiting send can be dropped");
        // Flood waits exhausted (4c): Failed, with Retry and Discard again
        // and no countdown left over.
        set(&mut e, 1, SendState::Failed);
        assert!(get(&e).can_retry() && get(&e).can_discard());
        assert_eq!(get(&e).wait_left(Instant::now()), None);
        assert!(!e.any_waiting());
        e.retried(LocalId(1));
        assert_eq!(states(&e, A), [SendState::Sending]);
        e.discarded(LocalId(1));
        assert_eq!(e.count(A), 0);
        assert_eq!(
            e.set_state(LocalId(1), SendState::Failed, Instant::now()),
            None,
            "late event"
        );
    }

    #[test]
    fn flood_wait_counts_down() {
        let mut e = Echoes::default();
        e.push(echo(1, A)).unwrap();
        let now = Instant::now();
        e.set_state(LocalId(1), SendState::FloodWait { seconds: 3 }, now);
        let w = e.get(LocalId(1)).unwrap();
        assert_eq!(w.wait_left(now), Some(3));
        assert_eq!(w.wait_left(now + Duration::from_millis(1_500)), Some(2));
        assert_eq!(w.wait_left(now + Duration::from_secs(9)), Some(0));
        assert!(e.any_waiting());
        e.set_state(LocalId(1), SendState::Sending, now);
        assert!(!e.any_waiting(), "resent: no countdown");
    }

    #[test]
    fn sent_echo_gives_way_to_the_server_copy() {
        let mut e = Echoes::default();
        e.push(echo(1, A)).unwrap();
        e.push(echo(2, A)).unwrap();
        e.push(echo(3, B)).unwrap();
        set(&mut e, 1, SendState::Sent { id: MsgId(10) });
        set(&mut e, 3, SendState::Sent { id: MsgId(10) });
        assert!(!e.resolve(A, |id| id == MsgId(11)));
        assert!(e.resolve(A, |id| id == MsgId(10)));
        assert_eq!(states(&e, A), [SendState::Sending]);
        assert_eq!(e.count(B), 1, "other chats are untouched");
    }

    #[test]
    fn reconcile_drops_deleted_sends_but_not_unconfirmed_ones() {
        let mut e = Echoes::default();
        for n in 1..=4 {
            e.push(echo(n, A)).unwrap();
        }
        set(&mut e, 1, SendState::SentUnconfirmed);
        set(&mut e, 2, SendState::Sent { id: MsgId(5) });
        set(&mut e, 3, SendState::Sent { id: MsgId(50) });
        let shown = |_: MsgId| false;
        assert!(e.reconciled(A, Some(MsgId(20)), Some(&shown)));
        assert!(!e.reconciled(A, Some(MsgId(20)), None), "not on screen");
        for _ in 0..3 {
            e.reconciled(A, Some(MsgId(20)), Some(&shown));
        }
        assert_eq!(
            states(&e, A),
            [
                SendState::SentUnconfirmed,
                SendState::Sent { id: MsgId(50) },
                SendState::Sending
            ],
            "5 was deleted; reconciles alone never prove an unconfirmed send"
        );
        assert!(!e.covered(&[LocalId(3)]), "only unconfirmed sends");
        assert!(e.covered(&[LocalId(1)]));
        assert_eq!(e.count(A), 2);
    }

    #[test]
    fn bounded() {
        let mut e = Echoes::default();
        for n in 0..MAX_ECHOES {
            e.push(echo(u64::try_from(n).unwrap(), A)).unwrap();
        }
        assert!(e.is_full());
        assert_eq!(e.push(echo(9999, A)), Err(Full));
        // An accepted send whose id is unknown gives way at the cap.
        set(&mut e, 7, SendState::SentUnconfirmed);
        assert!(!e.is_full());
        e.push(echo(9999, A)).unwrap();
        assert!(e.get(LocalId(7)).is_none());
        assert_eq!(e.count(A), MAX_ECHOES);
        assert!(e.is_full());
    }
}
