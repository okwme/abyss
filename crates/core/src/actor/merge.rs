//! Light refresh of an open window: merges a server tail into the window in
//! place, so the user's view is not reset every few minutes. Falls back to
//! replacing the window when the tail reveals messages missing inside it (a
//! silent gap), which a merge cannot place.

use std::collections::VecDeque;

use super::Actor;
use super::apply::{HistoryReply, MIN_ID};
use crate::api::{Event, HistoryDir};
use crate::backend::{Backend, HistoryPage};
use crate::model::{Message, MsgId};
use crate::store::Change;

/// How a server tail differs from the window it refreshes.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct TailDiff {
    /// The tail holds messages the window lacks inside its range.
    pub(super) gap: bool,
    /// Window messages inside the tail's range that the server no longer has.
    pub(super) deleted: Vec<MsgId>,
    /// Server copies newer (later edit) than the window's.
    pub(super) edited: Vec<Message>,
    /// Messages newer than the window's newest.
    pub(super) appended: Vec<Message>,
}

/// Compares a tail `page` with an attached `window`, both ascending by id. An
/// empty window or page is reported as a gap: replace instead of merging.
pub(super) fn tail_diff(window: &VecDeque<Message>, page: &HistoryPage) -> TailDiff {
    let mut diff = TailDiff::default();
    let (Some(oldest), Some(newest), Some(hi)) = (
        window.front().map(|m| m.id),
        window.back().map(|m| m.id),
        page.messages.last().map(|m| m.id),
    ) else {
        diff.gap = true;
        return diff;
    };
    let lo = match page.messages.first() {
        Some(first) if page.has_more => first.id,
        _ => MIN_ID,
    };
    for m in &page.messages {
        match window.binary_search_by_key(&m.id, |w| w.id) {
            Ok(i) => {
                if window.get(i).is_some_and(|w| m.version() > w.version()) {
                    diff.edited.push(m.clone());
                }
            }
            Err(_) if m.id > newest => diff.appended.push(m.clone()),
            Err(_) if m.id > oldest => diff.gap = true,
            // Older than the window: history the user has not loaded.
            Err(_) => {}
        }
    }
    let mut ids: Vec<MsgId> = page.messages.iter().map(|m| m.id).collect();
    ids.sort_unstable();
    diff.deleted = window
        .iter()
        .map(|w| w.id)
        .filter(|id| (lo..=hi).contains(id) && ids.binary_search(id).is_err())
        .collect();
    diff
}

impl<B: Backend> Actor<B> {
    /// Applies a refresh tail to the open window (see the module docs).
    pub(super) async fn merge_tail(&mut self, p: HistoryReply) {
        let Some(w) = self.windows.get(p.chat) else {
            return;
        };
        if w.has_newer() {
            return; // detached: scrolling back down loads current pages
        }
        let diff = tail_diff(w.messages(), &p.page);
        if diff.gap {
            return self.on_tail(p).await;
        }
        let (chat, view) = (p.chat, p.view);
        if let Some(hi) = p.page.messages.last().map(|m| m.id) {
            let lo = match p.page.messages.first() {
                Some(first) if p.page.has_more => first.id,
                _ => MIN_ID,
            };
            self.reconcile(chat, lo, hi, &p.page.messages).await;
        }
        let removed = self.windows.delete(chat, &diff.deleted);
        if !removed.is_empty() {
            self.emit(Event::Deleted { chat, ids: removed }).await;
        }
        for m in diff.edited {
            if self.windows.replace(m.clone()) {
                self.emit(Event::Edited(m)).await;
            }
        }
        let mut change = Change::default();
        for m in diff.appended {
            let c = self.windows.upsert(m);
            change.inserted.extend(c.inserted);
            change.removed.extend(c.removed);
        }
        if !change.inserted.is_empty() {
            self.emit_history(chat, view, HistoryDir::Newer, change)
                .await;
        }
    }
}

#[cfg(all(test, feature = "fake"))]
mod tests {
    use super::*;
    use crate::backend::fake::text_message;
    use crate::model::{ChatId, PeerKind};

    const CHAT: ChatId = ChatId {
        kind: PeerKind::User,
        id: 1,
    };

    fn msgs(ids: &[i32]) -> Vec<Message> {
        ids.iter().map(|i| text_message(CHAT, *i, "m")).collect()
    }

    fn ids(v: &[Message]) -> Vec<i32> {
        v.iter().map(|m| m.id.0).collect()
    }

    #[test]
    fn merge_finds_deletions_edits_and_appends() {
        let window: VecDeque<Message> = msgs(&[1, 2, 3, 4, 5]).into();
        let mut page = msgs(&[3, 5, 6]);
        page[1].edit_date = Some(9);
        let diff = tail_diff(
            &window,
            &HistoryPage {
                messages: page,
                has_more: true,
            },
        );
        assert!(!diff.gap);
        assert_eq!(diff.deleted, vec![MsgId(4)], "1 and 2 are below the page");
        assert_eq!(ids(&diff.edited), [5]);
        assert_eq!(ids(&diff.appended), [6]);
    }

    #[test]
    fn missing_message_inside_the_window_is_a_gap() {
        let window: VecDeque<Message> = msgs(&[1, 3, 5]).into();
        let page = HistoryPage {
            messages: msgs(&[1, 2, 3, 5]),
            has_more: false,
        };
        assert!(tail_diff(&window, &page).gap);
    }
}
