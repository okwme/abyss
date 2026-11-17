//! The chat list as the UI holds it: core's dialogs in display order (plan
//! section 7). Core owns the order; this mirrors its events.

use abyss_core::model::{ChatId, ChatSummary, Preview};
use abyss_core::store::MAX_DIALOGS;
use gpui::SharedString;

use super::copy;

/// Rows before the end of the list at which the next page is requested.
const LOAD_MARGIN: usize = 20;
/// Characters of the last message shown in a row.
const PREVIEW_CHARS: usize = 100;

#[derive(Debug, Default)]
pub(crate) struct ChatList {
    chats: Vec<ChatSummary>,
    /// List length when the last page was requested; no new request until
    /// the list changes length.
    requested_at: Option<usize>,
}

impl ChatList {
    pub(crate) fn as_slice(&self) -> &[ChatSummary] {
        &self.chats
    }

    pub(crate) fn len(&self) -> usize {
        self.chats.len()
    }

    pub(crate) fn get(&self, id: ChatId) -> Option<&ChatSummary> {
        self.chats.iter().find(|c| c.id == id)
    }

    /// Core's whole list (it merges a reloaded first page into later pages).
    pub(crate) fn reset(&mut self, mut chats: Vec<ChatSummary>) {
        chats.truncate(MAX_DIALOGS);
        self.chats = chats;
    }

    /// A later page: listed chats update in place, new ones are appended.
    pub(crate) fn upsert_page(&mut self, chats: Vec<ChatSummary>) {
        for chat in chats {
            if let Some(slot) = self.chats.iter_mut().find(|c| c.id == chat.id) {
                *slot = chat;
            } else if self.chats.len() < MAX_DIALOGS {
                self.chats.push(chat);
            }
        }
    }

    /// A chat's summary changed. A new last message moves it to the top of its
    /// section (pinned chats keep their place), as core does; anything else
    /// (an unread count, read elsewhere) updates it in place.
    pub(crate) fn bump(&mut self, chat: ChatSummary) {
        let Some(pos) = self.chats.iter().position(|c| c.id == chat.id) else {
            if self.chats.len() < MAX_DIALOGS {
                let at = self.section_start(&chat);
                self.chats.insert(at, chat);
            }
            return;
        };
        let moved = self
            .chats
            .get(pos)
            .is_some_and(|old| old.last != chat.last || old.last_date != chat.last_date);
        if !moved || chat.pinned {
            if let Some(slot) = self.chats.get_mut(pos) {
                *slot = chat;
            }
            return;
        }
        self.chats.remove(pos);
        let at = self.section_start(&chat);
        self.chats.insert(at, chat);
    }

    /// Where `chat`'s section (pinned or not) begins.
    fn section_start(&self, chat: &ChatSummary) -> usize {
        if chat.pinned {
            0
        } else {
            self.chats.iter().take_while(|c| c.pinned).count()
        }
    }

    /// Whether to ask core for the next page now that row `last_visible` is
    /// on screen. Asks at most once per list length; core answers nothing
    /// when every page is loaded.
    pub(crate) fn wants_more(&mut self, last_visible: usize) -> bool {
        let len = self.chats.len();
        let near_end = last_visible.saturating_add(LOAD_MARGIN) >= len;
        if !near_end || len == 0 || len >= MAX_DIALOGS || self.requested_at == Some(len) {
            return false;
        }
        self.requested_at = Some(len);
        true
    }
}

/// The row's second line: the last message, media as `[Photo]`.
pub(crate) fn preview_text(preview: Option<&Preview>) -> SharedString {
    match preview {
        Some(Preview::Text(text)) => match text.char_indices().nth(PREVIEW_CHARS) {
            Some((cut, _)) => format!("{}…", text.get(..cut).unwrap_or_default()).into(),
            None => text.clone().into(),
        },
        Some(Preview::Media(kind)) => format!("[{}]", copy::media_label(kind)).into(),
        Some(Preview::Service) => copy::SERVICE_FALLBACK.into(),
        None => SharedString::default(),
    }
}

/// Up to two initials for the avatar placeholder (no images in Stage 1).
pub(crate) fn initials(title: &str) -> SharedString {
    title
        .split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect::<String>()
        .into()
}

#[cfg(test)]
mod tests {
    use abyss_core::model::{ChatKind, MediaKind, PeerKind};

    use super::*;

    fn id(n: i64) -> ChatId {
        ChatId {
            kind: PeerKind::User,
            id: n,
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

    fn with_message(n: i64, date: i64) -> ChatSummary {
        ChatSummary {
            last: Some(Preview::Text("hi".into())),
            last_date: date,
            ..chat(n)
        }
    }

    fn ids(list: &ChatList) -> Vec<i64> {
        list.as_slice().iter().map(|c| c.id.id).collect()
    }

    #[test]
    fn pages_merge_by_id() {
        let mut list = ChatList::default();
        list.reset(vec![pinned(1), chat(2), chat(3)]);
        let mut fresh = chat(3);
        fresh.unread = 7;
        list.upsert_page(vec![fresh, chat(4)]);
        assert_eq!(ids(&list), [1, 2, 3, 4]);
        assert_eq!(list.get(id(3)).unwrap().unread, 7);
        // A reset is core's merged list: it replaces ours wholesale.
        list.reset(vec![chat(4), chat(1)]);
        assert_eq!(ids(&list), [4, 1]);
    }

    #[test]
    fn new_message_moves_but_read_state_does_not() {
        let mut list = ChatList::default();
        list.reset(vec![pinned(1), chat(2), chat(3)]);
        list.bump(with_message(3, 10));
        assert_eq!(ids(&list), [1, 3, 2]);
        let mut read = with_message(3, 10);
        read.unread = 0;
        list.bump(chat(2)); // unchanged last message: in place
        list.bump(read);
        assert_eq!(ids(&list), [1, 3, 2]);
        list.bump(ChatSummary {
            pinned: true,
            ..with_message(1, 20)
        });
        assert_eq!(ids(&list), [1, 3, 2], "pinned chats keep their place");
        list.bump(with_message(9, 30));
        assert_eq!(
            ids(&list),
            [1, 9, 3, 2],
            "unknown chats enter their section"
        );
    }

    #[test]
    fn pagination_asks_once_per_length_near_the_end() {
        let mut list = ChatList::default();
        assert!(!list.wants_more(0), "nothing loaded yet: core loads page 1");
        list.reset((0..100).map(chat).collect());
        assert!(!list.wants_more(10));
        assert!(list.wants_more(85));
        assert!(!list.wants_more(99), "already asked at this length");
        list.upsert_page((100..200).map(chat).collect());
        assert!(list.wants_more(199));
    }

    #[test]
    fn bounded() {
        let mut list = ChatList::default();
        let max = i64::try_from(MAX_DIALOGS).unwrap();
        list.reset((0..max + 5).map(chat).collect());
        assert_eq!(list.len(), MAX_DIALOGS);
        list.upsert_page(vec![chat(-1)]);
        list.bump(with_message(-2, 1));
        assert_eq!(list.len(), MAX_DIALOGS);
        assert!(!list.wants_more(MAX_DIALOGS - 1), "never past the cap");
    }

    #[test]
    fn row_text() {
        let long = "x".repeat(150);
        assert_eq!(
            preview_text(Some(&Preview::Text(long))).chars().count(),
            101
        );
        assert_eq!(
            preview_text(Some(&Preview::Media(MediaKind::Photo))),
            "[Photo]"
        );
        assert_eq!(initials("ada lovelace"), "AL");
        assert_eq!(initials("  ✨ émile  zola bar"), "ÉZ");
        assert_eq!(initials(""), "");
    }
}
