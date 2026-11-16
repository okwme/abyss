//! The chat list as the UI holds it: core's dialogs in display order. Task 9
//! replaces the placeholder shell around it.

use abyss_core::model::{ChatId, ChatSummary};
use abyss_core::store::MAX_DIALOGS;

#[derive(Debug, Default)]
pub(crate) struct ChatList {
    chats: Vec<ChatSummary>,
}

impl ChatList {
    pub(crate) fn as_slice(&self) -> &[ChatSummary] {
        &self.chats
    }

    pub(crate) fn get(&self, id: ChatId) -> Option<&ChatSummary> {
        self.chats.iter().find(|c| c.id == id)
    }

    pub(crate) fn reset(&mut self, chats: Vec<ChatSummary>) {
        self.chats = chats;
        self.chats.truncate(MAX_DIALOGS);
    }

    pub(crate) fn append(&mut self, chats: Vec<ChatSummary>) {
        for chat in chats {
            if self.chats.len() >= MAX_DIALOGS {
                break;
            }
            if self.get(chat.id).is_none() {
                self.chats.push(chat);
            }
        }
    }

    /// A changed chat moves to the top of its section (pinned chats first).
    pub(crate) fn bump(&mut self, chat: ChatSummary) {
        self.chats.retain(|c| c.id != chat.id);
        let at = if chat.pinned {
            0
        } else {
            self.chats.iter().take_while(|c| c.pinned).count()
        };
        self.chats.insert(at, chat);
        self.chats.truncate(MAX_DIALOGS);
    }
}

#[cfg(test)]
mod tests {
    use abyss_core::model::{ChatKind, PeerKind};

    use super::*;

    fn chat(id: i64) -> ChatSummary {
        ChatSummary {
            id: ChatId {
                kind: PeerKind::User,
                id,
            },
            title: format!("c{id}"),
            kind: ChatKind::Private,
            pinned: false,
            unread: 0,
            unread_mentions: 0,
            last: None,
            last_date: 0,
            can_send: true,
        }
    }

    fn pinned(id: i64) -> ChatSummary {
        ChatSummary {
            pinned: true,
            ..chat(id)
        }
    }

    fn ids(list: &ChatList) -> Vec<i64> {
        list.as_slice().iter().map(|c| c.id.id).collect()
    }

    #[test]
    fn reset_append_bump() {
        let mut list = ChatList::default();
        list.reset(vec![pinned(1), chat(2), chat(3)]);
        list.append(vec![chat(3), chat(4)]);
        assert_eq!(ids(&list), [1, 2, 3, 4]);
        list.bump(chat(4));
        assert_eq!(ids(&list), [1, 4, 2, 3]);
        list.bump(pinned(3));
        assert_eq!(ids(&list), [3, 1, 4, 2]);
    }

    #[test]
    fn bounded() {
        let mut list = ChatList::default();
        list.reset(
            (0..i64::try_from(MAX_DIALOGS).unwrap() + 5)
                .map(chat)
                .collect(),
        );
        assert_eq!(list.as_slice().len(), MAX_DIALOGS);
        list.append(vec![chat(-1)]);
        list.bump(chat(-2));
        assert_eq!(list.as_slice().len(), MAX_DIALOGS);
    }
}
