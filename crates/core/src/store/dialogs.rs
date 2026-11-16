//! Bounded, ordered chat list.

use crate::model::{ChatId, ChatSummary, Preview};

/// Maximum number of dialogs held.
pub const MAX_DIALOGS: usize = 1000;

/// Ordered chat list: pinned chats first in server order, then unpinned chats by
/// recency. Bounded by [`MAX_DIALOGS`].
#[derive(Debug, Default)]
pub struct Dialogs {
    list: Vec<ChatSummary>,
}

/// What a new message did to the chat list.
#[derive(Debug, PartialEq, Eq)]
pub enum Bump {
    /// The chat moved and/or changed; carries its new summary.
    Updated(ChatSummary),
    /// The chat is not in the loaded list. Callers decide whether to fetch it.
    Unknown,
}

/// Whether a new message counts toward the chat's unread badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unread {
    Increment,
    Unchanged,
}

impl Dialogs {
    pub fn as_slice(&self) -> &[ChatSummary] {
        &self.list
    }

    pub fn get(&self, id: ChatId) -> Option<&ChatSummary> {
        self.list.iter().find(|c| c.id == id)
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Replaces the list (reconciliation). Duplicates and overflow are dropped.
    pub fn reset(&mut self, chats: Vec<ChatSummary>) {
        self.list.clear();
        self.extend(chats);
    }

    /// Appends a page in server order. Returns the chats actually added.
    pub fn append(&mut self, chats: Vec<ChatSummary>) -> Vec<ChatSummary> {
        let start = self.list.len();
        self.extend(chats);
        self.list.get(start..).unwrap_or_default().to_vec()
    }

    fn extend(&mut self, chats: Vec<ChatSummary>) {
        for chat in chats {
            if self.list.len() >= MAX_DIALOGS {
                break;
            }
            if self.get(chat.id).is_none() {
                self.list.push(chat);
            }
        }
    }

    /// Records a new message in `chat`: updates the preview, date and unread
    /// count, and moves an unpinned chat to the top of the unpinned section.
    pub fn bump(&mut self, chat: ChatId, preview: Preview, date: i64, unread: Unread) -> Bump {
        let Some(pos) = self.list.iter().position(|c| c.id == chat) else {
            return Bump::Unknown;
        };
        let mut summary = self.list.remove(pos);
        summary.last = Some(preview);
        summary.last_date = summary.last_date.max(date);
        if unread == Unread::Increment {
            summary.unread = summary.unread.saturating_add(1);
        }
        let insert_at = if summary.pinned {
            pos
        } else {
            self.list.iter().take_while(|c| c.pinned).count()
        };
        self.list.insert(insert_at, summary.clone());
        Bump::Updated(summary)
    }

    /// Sets the unread count (local read or read on another device).
    pub fn set_unread(&mut self, chat: ChatId, unread: u32) -> Option<&ChatSummary> {
        let summary = self.list.iter_mut().find(|c| c.id == chat)?;
        summary.unread = unread;
        Some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatKind, PeerKind};

    fn id(id: i64) -> ChatId {
        ChatId {
            kind: PeerKind::User,
            id,
        }
    }

    fn chat(n: i64) -> ChatSummary {
        ChatSummary {
            id: id(n),
            title: format!("c{n}"),
            kind: ChatKind::Private,
            pinned: false,
            unread: 0,
            unread_mentions: 0,
            last: None,
            last_date: 0,
            can_send: true,
        }
    }

    fn pinned(n: i64) -> ChatSummary {
        ChatSummary {
            pinned: true,
            ..chat(n)
        }
    }

    fn ids(d: &Dialogs) -> Vec<i64> {
        d.as_slice().iter().map(|c| c.id.id).collect()
    }

    #[test]
    fn bump_moves_unpinned_below_pinned() {
        let mut d = Dialogs::default();
        d.reset(vec![pinned(1), pinned(2), chat(3), chat(4)]);
        let r = d.bump(id(4), Preview::Service, 10, Unread::Increment);
        assert!(matches!(r, Bump::Updated(ref s) if s.unread == 1));
        assert_eq!(ids(&d), vec![1, 2, 4, 3]);
    }

    #[test]
    fn bump_keeps_pinned_position() {
        let mut d = Dialogs::default();
        d.reset(vec![pinned(1), pinned(2), chat(3)]);
        d.bump(id(2), Preview::Service, 10, Unread::Unchanged);
        assert_eq!(ids(&d), vec![1, 2, 3]);
    }

    #[test]
    fn unknown_chat_is_reported() {
        let mut d = Dialogs::default();
        assert_eq!(
            d.bump(id(9), Preview::Service, 0, Unread::Increment),
            Bump::Unknown
        );
    }

    #[test]
    fn bounded_and_deduplicated() {
        let mut d = Dialogs::default();
        let page: Vec<_> = (0..i64::try_from(MAX_DIALOGS).unwrap() + 50)
            .map(chat)
            .collect();
        d.append(vec![chat(0), chat(0)]);
        let added = d.append(page);
        assert_eq!(d.len(), MAX_DIALOGS);
        assert_eq!(added.len(), MAX_DIALOGS - 1);
        assert_eq!(added.first().map(|c| c.id), Some(id(1)));
    }

    #[test]
    fn unread_saturates() {
        let mut d = Dialogs::default();
        let mut c = chat(1);
        c.unread = u32::MAX;
        d.reset(vec![c]);
        d.bump(id(1), Preview::Service, 0, Unread::Increment);
        assert_eq!(d.as_slice()[0].unread, u32::MAX);
    }
}
