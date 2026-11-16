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

/// What core's `DialogsPaging` said about further pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pages {
    More,
    Done,
}

/// Dialogs pagination, driven by core's `DialogsPaging` rather than by list
/// length: a page whose chats were all filtered (broadcast channels) or
/// already listed leaves the length unchanged, yet more pages may follow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Paging {
    /// Core said more pages exist. Core loads the first page itself.
    has_more: bool,
    /// A `LoadDialogs` was admitted and core has not answered it yet.
    in_flight: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ChatList {
    chats: Vec<ChatSummary>,
    paging: Paging,
}

/// Whether `new` describes a newer last message than `old`: by message id,
/// or by date when either id is unknown. Same-second messages with the same
/// preview still order by id.
fn is_newer(new: &ChatSummary, old: &ChatSummary) -> bool {
    match (new.last_id, old.last_id) {
        (Some(n), Some(o)) => n > o,
        _ => new.last_date > old.last_date,
    }
}

impl ChatList {
    pub(crate) fn as_slice(&self) -> &[ChatSummary] {
        &self.chats
    }

    pub(crate) fn len(&self) -> usize {
        self.chats.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.chats.is_empty()
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

    /// A chat's summary changed. A newer last message (by `last_id`) moves
    /// it to the top of its section (pinned chats keep their place), as core
    /// does; anything else (an unread count, a read elsewhere, a repeated
    /// bump) updates it in place.
    pub(crate) fn bump(&mut self, chat: ChatSummary) {
        let Some(pos) = self.chats.iter().position(|c| c.id == chat.id) else {
            if self.chats.len() < MAX_DIALOGS {
                let at = self.section_start(&chat);
                self.chats.insert(at, chat);
            }
            return;
        };
        let moved = self.chats.get(pos).is_some_and(|old| is_newer(&chat, old));
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
    /// on screen (any row, for an empty list): core said more exist, none is
    /// outstanding, and the cap is not reached.
    pub(crate) fn wants_more(&self, last_visible: usize) -> bool {
        let len = self.chats.len();
        let near_end = len == 0 || last_visible.saturating_add(LOAD_MARGIN) >= len;
        near_end && self.paging.has_more && !self.paging.in_flight && len < MAX_DIALOGS
    }

    /// Core admitted a `LoadDialogs`.
    pub(crate) fn requested(&mut self) {
        self.paging.in_flight = true;
    }

    /// Core applied a dialogs page (`DialogsPaging`).
    pub(crate) fn paged(&mut self, pages: Pages) {
        self.paging = Paging {
            has_more: pages == Pages::More,
            in_flight: false,
        };
    }

    /// A core error: an outstanding page may never be answered.
    pub(crate) fn load_failed(&mut self) {
        self.paging.in_flight = false;
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

/// A stable hue in `[0, 1)` for a chat or sender, from its id.
pub(crate) fn hue(id: ChatId) -> f32 {
    // FNV-1a over the id; display only, not security relevant.
    let mut h: u32 = 0x811c_9dc5;
    for b in id.id.to_le_bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    f32::from(u16::try_from(h % 360).unwrap_or(0)) / 360.
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
            last_id: None,
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
            last_id: None,
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

    /// Same second, same preview: only the message id tells them apart.
    fn with_id(n: i64, msg: i32) -> ChatSummary {
        ChatSummary {
            last_id: Some(abyss_core::model::MsgId(msg)),
            ..with_message(n, 100)
        }
    }

    #[test]
    fn identical_previews_in_one_second_order_by_id() {
        let mut list = ChatList::default();
        list.reset(vec![with_id(1, 10), with_id(2, 20), with_id(3, 30)]);
        list.bump(with_id(3, 31));
        assert_eq!(ids(&list), [3, 1, 2]);
        list.bump(with_id(2, 21));
        assert_eq!(ids(&list), [2, 3, 1], "same date and preview, newer id");
        let mut stale = with_id(3, 30);
        stale.unread = 5;
        list.bump(stale);
        assert_eq!(ids(&list), [2, 3, 1], "an older id never moves a chat");
        assert_eq!(list.get(id(3)).unwrap().unread, 5, "but updates it");
        list.bump(with_id(2, 21));
        assert_eq!(ids(&list), [2, 3, 1], "a repeated bump is deduped");
    }

    #[test]
    fn pagination_follows_core_paging_not_list_length() {
        let mut list = ChatList::default();
        assert!(!list.wants_more(0), "nothing loaded yet: core loads page 1");
        // A first page made only of broadcast channels: nothing listed, more
        // pages exist.
        list.reset(Vec::new());
        list.paged(Pages::More);
        assert!(list.wants_more(0), "an all-filtered page still paginates");
        list.requested();
        assert!(!list.wants_more(0), "one page at a time");
        list.paged(Pages::More);
        list.reset((0..100).map(chat).collect());
        assert!(!list.wants_more(10));
        assert!(list.wants_more(85));
        list.requested();
        // Core answers a "load more" with a reset when the page reorders
        // rows (4c): paging stays in flight until `DialogsPaging`.
        list.reset((0..100).rev().map(chat).collect());
        assert!(!list.wants_more(99), "the reset alone ends nothing");
        list.paged(Pages::More);
        list.requested();
        // An overlapping page: the length does not change.
        list.upsert_page((90..100).map(chat).collect());
        list.paged(Pages::More);
        assert!(list.wants_more(99), "the same length asks again");
        list.requested();
        list.load_failed();
        assert!(list.wants_more(99), "a failed page can be asked again");
        list.paged(Pages::Done);
        assert!(!list.wants_more(99), "every page loaded");
    }

    #[test]
    fn bounded() {
        let mut list = ChatList::default();
        let max = i64::try_from(MAX_DIALOGS).unwrap();
        list.paged(Pages::More);
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
        assert!((0.0..1.0).contains(&hue(id(42))));
    }
}
