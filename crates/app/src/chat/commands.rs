//! The chat shell's commands to core. Each goes through an [`Admit`] first,
//! and the UI changes that follow from it (an echo, a cleared composer, a
//! pending load, a read mark) are made only once core admitted it. A full
//! queue therefore changes nothing: the next attempt asks again.

use abyss_core::api::{Command, HistoryDir, UserError, WindowState};
use abyss_core::model::{ChatId, LocalId};
use gpui::FollowMode;

use super::ChatShell;
use super::messages::MessageWindow;
use super::sends::Echo;
#[cfg(test)]
use super::sends::Echoes;
use super::viewport::Seen;

/// Core did not take a command: its queue is full, or it is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Busy,
    Closed,
}

/// Queues a command with core, without waiting.
pub(crate) type Admit<'a> = &'a mut dyn FnMut(Command) -> Result<(), Refused>;

impl ChatShell {
    /// Opens `chat` once core admitted `OpenChat` (core closes the previous
    /// one). Refused, the previous chat stays selected.
    pub(crate) fn select(&mut self, chat: ChatId, admit: Admit<'_>) -> Result<(), Refused> {
        if self.selected == Some(chat) {
            return Ok(());
        }
        admit(Command::OpenChat(chat))?;
        self.selected = Some(chat);
        self.window = Some(MessageWindow::new(chat));
        self.revealed.clear();
        self.read_sent = None;
        self.shown_echoes = self.echo_count();
        self.rows.reset(self.shown_echoes);
        self.rows.set_follow_mode(FollowMode::Tail);
        Ok(())
    }

    /// Where a composer submission would go: `Ok(None)` when nothing is open
    /// or the chat is read-only, `Busy` when the send table is full.
    pub(crate) fn send_target(&self) -> Result<Option<ChatId>, UserError> {
        let Some(chat) = self.selected_chat().filter(|c| c.can_send) else {
            return Ok(None);
        };
        if self.echoes.is_full() {
            return Err(UserError::Busy);
        }
        Ok(Some(chat.id))
    }

    /// Sends `text` to `chat` (from [`Self::send_target`]). Only once core
    /// admitted the send does its echo appear; the caller clears the
    /// composer on `Ok(Some(_))`. `Ok(None)`: nothing to send.
    pub(crate) fn submit(
        &mut self,
        chat: ChatId,
        text: &str,
        now: i64,
        admit: Admit<'_>,
    ) -> Result<Option<LocalId>, Refused> {
        if text.trim().is_empty() || self.echoes.is_full() {
            return Ok(None);
        }
        let local_id = LocalId(self.next_local);
        admit(Command::SendText {
            chat,
            text: text.to_owned(),
            local_id,
        })?;
        self.next_local = self.next_local.wrapping_add(1);
        let pushed = self
            .echoes
            .push(Echo::new(local_id, chat, text.to_owned(), now));
        debug_assert!(pushed.is_ok(), "checked not full above");
        self.sync_echoes();
        self.rows.set_follow_mode(FollowMode::Tail);
        Ok(Some(local_id))
    }

    /// Retries a failed or unknown send; it shows "Sending…" only once core
    /// admitted the retry.
    pub(crate) fn retry(&mut self, local_id: LocalId, admit: Admit<'_>) -> Result<(), Refused> {
        if !self.echoes.get(local_id).is_some_and(Echo::can_retry) {
            return Ok(());
        }
        admit(Command::RetrySend(local_id))?;
        self.echoes.retried(local_id);
        self.sync_echoes();
        Ok(())
    }

    /// Drops a failed, unknown or flood-waiting send; the echo goes only
    /// once core admitted the discard, so it never hides a send core keeps.
    pub(crate) fn discard(&mut self, local_id: LocalId, admit: Admit<'_>) -> Result<(), Refused> {
        if !self.echoes.get(local_id).is_some_and(Echo::can_discard) {
            return Ok(());
        }
        admit(Command::DiscardSend(local_id))?;
        self.echoes.discarded(local_id);
        self.sync_echoes();
        Ok(())
    }

    /// Back to the live tail from far up the history: core reopens the chat
    /// at its tail.
    pub(crate) fn jump_to_latest(&mut self, admit: Admit<'_>) -> Result<(), Refused> {
        let Some(chat) = self.window.as_ref().map(MessageWindow::chat) else {
            return Ok(());
        };
        admit(Command::OpenChat(chat))?;
        self.rows.set_follow_mode(FollowMode::Tail);
        Ok(())
    }

    /// Whether chat list row `last_visible` on screen calls for the next
    /// dialogs page (see [`super::list::ChatList::wants_more`]).
    pub(crate) fn wants_dialogs(&self, last_visible: usize) -> bool {
        self.list.wants_more(last_visible)
    }

    /// Asks for the next dialogs page if row `last_visible` calls for it.
    pub(crate) fn request_dialogs(
        &mut self,
        last_visible: usize,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        if self.list.wants_more(last_visible) {
            admit(Command::LoadDialogs)?;
            self.list.requested();
        }
        Ok(())
    }

    /// After a frame was painted: load more history near either end, mark
    /// read up to the newest message on screen while the window is active,
    /// and tell core whether the live tail is on screen. `seen` is `None`
    /// when no row was laid out.
    pub(crate) fn frame_seen(
        &mut self,
        seen: Option<Seen>,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        let Some(w) = self.window.as_mut() else {
            return self.set_viewing(None, admit);
        };
        let chat = w.chat();
        if let Some(seen) = seen {
            if w.wants_older(seen.first_row) {
                admit(Command::LoadOlder(chat))?;
                w.requested(HistoryDir::Older);
            }
            if let Some(last) = seen.last_message
                && w.wants_newer(last)
            {
                admit(Command::LoadNewer(chat))?;
                w.requested(HistoryDir::Newer);
            }
        }
        let active = self.focus == WindowState::Active;
        let shown = seen.and_then(|s| s.last_message).and_then(|ix| w.get(ix));
        if let Some(max_id) = shown
            .map(|m| m.id)
            .filter(|id| active && self.read_sent.is_none_or(|r| *id > r))
        {
            admit(Command::MarkRead { chat, max_id })?;
            self.read_sent = Some(max_id);
        }
        let tail = tail_on_screen(w, seen);
        self.set_viewing((active && tail).then_some(chat), admit)
    }

    /// Tells core which chat's live tail is on screen, when that changed.
    fn set_viewing(&mut self, chat: Option<ChatId>, admit: Admit<'_>) -> Result<(), Refused> {
        if self.viewing != chat {
            admit(Command::ViewingTail(chat))?;
            self.viewing = chat;
        }
        Ok(())
    }

    /// The window gained or lost focus. Losing it also tells core the tail
    /// is no longer watched.
    pub(crate) fn set_focus(
        &mut self,
        state: WindowState,
        admit: Admit<'_>,
    ) -> Result<(), Refused> {
        if self.focus != state {
            admit(Command::Window(state))?;
            self.focus = state;
        }
        if state == WindowState::Inactive {
            self.set_viewing(None, admit)?;
        }
        Ok(())
    }

    /// The echo table, for tests.
    #[cfg(test)]
    pub(crate) fn echoes(&self) -> &Echoes {
        &self.echoes
    }
}

/// Whether the open chat's newest message is on screen and the window is
/// attached to the live tail. An empty loaded chat shows its (empty) tail.
fn tail_on_screen(w: &MessageWindow, seen: Option<Seen>) -> bool {
    if !w.is_loaded() || w.has_newer() {
        return false;
    }
    match w.len().checked_sub(1) {
        None => true,
        Some(newest) => seen.and_then(|s| s.last_message) == Some(newest),
    }
}
