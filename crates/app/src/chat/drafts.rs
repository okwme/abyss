//! Per-chat composer drafts (task 6k): the message box belongs to the open
//! chat, so text typed or put back in chat A never goes to chat B. The
//! shell stashes the box when it switches chats and restores the new
//! chat's draft. Cleared with the account (`ChatShell::reset`).

use std::collections::VecDeque;

use abyss_core::model::ChatId;

/// Drafts kept at once. Past it the draft stashed longest ago goes: a
/// bound on memory, far above how many chats a person drafts in at once.
pub(crate) const MAX_DRAFTS: usize = 64;

#[derive(Default)]
pub(crate) struct Drafts {
    /// Oldest stash first; at most one per chat, none empty.
    list: VecDeque<(ChatId, String)>,
}

/// Task 6c: no draft text reaches `Debug`.
impl std::fmt::Debug for Drafts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Drafts")
            .field("len", &self.list.len())
            .finish_non_exhaustive()
    }
}

impl Drafts {
    /// Keeps `text` as `chat`'s draft, replacing an older one; an empty
    /// text clears it.
    pub(crate) fn stash(&mut self, chat: ChatId, text: String) {
        self.list.retain(|(c, _)| *c != chat);
        if text.is_empty() {
            return;
        }
        if self.list.len() >= MAX_DRAFTS {
            drop(self.list.pop_front());
        }
        self.list.push_back((chat, text));
        debug_assert!(self.list.len() <= MAX_DRAFTS);
    }

    /// Takes `chat`'s draft out.
    pub(crate) fn take(&mut self, chat: ChatId) -> Option<String> {
        let at = self.list.iter().position(|(c, _)| *c == chat)?;
        self.list.remove(at).map(|(_, text)| text)
    }
}

#[cfg(test)]
mod tests {
    use abyss_core::model::PeerKind;

    use super::*;

    fn chat(id: i64) -> ChatId {
        ChatId {
            kind: PeerKind::User,
            id,
        }
    }

    #[test]
    fn one_draft_per_chat_bounded() {
        let mut d = Drafts::default();
        d.stash(chat(1), "a".into());
        d.stash(chat(1), "a2".into());
        d.stash(chat(2), String::new());
        assert_eq!(d.take(chat(2)), None, "empty clears");
        assert_eq!(d.take(chat(1)).as_deref(), Some("a2"));
        assert_eq!(d.take(chat(1)), None, "taken");
        for n in 0..=i64::try_from(MAX_DRAFTS).unwrap() {
            d.stash(chat(n), format!("t{n}"));
        }
        assert_eq!(d.take(chat(0)), None, "oldest went at the bound");
        assert_eq!(d.take(chat(1)).as_deref(), Some("t1"));
        assert!(!format!("{d:?}").contains("t2"), "no text in Debug");
    }
}
