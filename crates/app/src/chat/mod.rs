//! Chat list, message view and sending (plan sections 7–9): the state the UI
//! keeps and the decisions it makes (what to load, what to mark read, what a
//! row shows), apart from rendering so they can be tested without a window.
//! The shell (`shell::chats`) feeds it events and sends the commands it
//! returns; `shell::chat_view` renders it.

pub(crate) mod copy;
#[cfg(feature = "fake")]
pub(crate) mod demo;
pub(crate) mod format;
pub(crate) mod list;
pub(crate) mod messages;
pub(crate) mod rich;
pub(crate) mod sends;

use std::collections::HashSet;

use abyss_core::api::{Command, Event, UserError, WindowState};
use abyss_core::model::{ChatId, ChatSummary, LocalId, Message, MsgId};
use gpui::{FollowMode, ListAlignment, ListState, px};

use self::list::ChatList;
use self::messages::{Applied, Batch, MessageWindow, RowOp};
use self::sends::{Echo, Echoes, MAX_ECHOES};

/// Rows rendered beyond the viewport, so scrolling doesn't pop in.
const OVERDRAW: f32 = 800.;
/// Messages whose spoilers the user revealed, per open chat.
const MAX_REVEALED: usize = 256;

/// One row of the message list: the window's messages, then the open chat's
/// echoes (own sends without a server copy yet).
#[derive(Debug)]
pub(crate) enum Row<'a> {
    Message(&'a Message),
    Echo(&'a Echo),
}

impl Row<'_> {
    pub(crate) fn date(&self) -> i64 {
        match self {
            Row::Message(m) => m.date,
            Row::Echo(e) => e.date,
        }
    }
}

/// What the last frame showed of the message list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    /// First row at least partly visible.
    pub(crate) first_row: usize,
    /// Last message row (not echo) at least partly visible.
    pub(crate) last_message: Option<usize>,
}

pub(crate) struct ChatShell {
    pub(crate) list: ChatList,
    selected: Option<ChatId>,
    window: Option<MessageWindow>,
    echoes: Echoes,
    /// The message list's GPUI state. Always `window.len() + shown_echoes` rows.
    rows: ListState,
    shown_echoes: usize,
    revealed: HashSet<MsgId>,
    /// Highest id sent as read for the open chat.
    read_sent: Option<MsgId>,
    next_local: u64,
    focus: WindowState,
}

impl Default for ChatShell {
    fn default() -> Self {
        let rows = ListState::new(0, ListAlignment::Bottom, px(OVERDRAW));
        rows.set_follow_mode(FollowMode::Tail);
        Self {
            list: ChatList::default(),
            selected: None,
            window: None,
            echoes: Echoes::default(),
            rows,
            shown_echoes: 0,
            revealed: HashSet::new(),
            read_sent: None,
            next_local: 1,
            focus: WindowState::Active,
        }
    }
}

impl std::fmt::Debug for ChatShell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatShell")
            .field("chats", &self.list.len())
            .field("selected", &self.selected)
            .field("rows", &self.rows.item_count())
            .finish_non_exhaustive()
    }
}

impl ChatShell {
    pub(crate) fn selected(&self) -> Option<ChatId> {
        self.selected
    }

    pub(crate) fn selected_chat(&self) -> Option<&ChatSummary> {
        self.list.get(self.selected?)
    }

    pub(crate) fn window(&self) -> Option<&MessageWindow> {
        self.window.as_ref()
    }

    pub(crate) fn rows(&self) -> &ListState {
        &self.rows
    }

    pub(crate) fn row_count(&self) -> usize {
        self.window.as_ref().map_or(0, MessageWindow::len) + self.shown_echoes
    }

    pub(crate) fn row(&self, ix: usize) -> Option<Row<'_>> {
        let w = self.window.as_ref()?;
        match w.get(ix) {
            Some(m) => Some(Row::Message(m)),
            None => self
                .echoes
                .for_chat(w.chat())
                .nth(ix.checked_sub(w.len())?)
                .map(Row::Echo),
        }
    }

    /// Whether the user revealed `id`'s spoilers.
    pub(crate) fn spoilers(&self, id: MsgId) -> rich::Spoilers {
        if self.revealed.contains(&id) {
            rich::Spoilers::Revealed
        } else {
            rich::Spoilers::Hidden
        }
    }

    pub(crate) fn reveal(&mut self, id: MsgId) {
        if self.revealed.len() >= MAX_REVEALED {
            self.revealed.clear();
        }
        self.revealed.insert(id);
        if let Some(ix) = self.row_of(id) {
            self.rows.remeasure_items(ix..ix + 1);
        }
    }

    fn row_of(&self, id: MsgId) -> Option<usize> {
        let w = self.window.as_ref()?;
        (0..w.len()).find(|ix| w.get(*ix).is_some_and(|m| m.id == id))
    }

    /// Applies a chat event from core. Other events are ignored.
    pub(crate) fn apply(&mut self, event: Event) {
        match event {
            Event::DialogsReset(chats) => self.list.reset(chats),
            Event::DialogsAppended(chats) => self.list.upsert_page(chats),
            Event::ChatBumped(chat) => self.list.bump(chat),
            Event::History {
                chat,
                view,
                dir,
                messages,
                removed,
                has_more,
            } => self.on_history(
                chat,
                Batch {
                    view,
                    dir,
                    messages,
                    removed,
                    has_more,
                },
            ),
            Event::ChatInvalidated(chat) => {
                let applied = self.open_window(chat).map(MessageWindow::invalidate);
                self.sync_rows(applied);
            }
            Event::Edited(m) => {
                let applied = self.open_window(m.thread.chat).map(|w| w.edit(m));
                self.sync_rows(applied);
            }
            Event::Deleted { chat, ids } => {
                let applied = self.open_window(chat).map(|w| w.delete(&ids));
                self.sync_rows(applied);
            }
            Event::SendState { local_id, state } => {
                if self.echoes.set_state(local_id, state).is_some() {
                    self.after_echo_change();
                }
            }
            // Task 10b consumes `Reconciled` (echo reconciliation) and
            // `DialogsPaging` (pagination state).
            Event::Auth(_)
            | Event::CredentialSource(_)
            | Event::Connection(_)
            | Event::Error(_)
            | Event::Reconciled { .. }
            | Event::DialogsPaging { .. } => {}
        }
    }

    fn open_window(&mut self, chat: ChatId) -> Option<&mut MessageWindow> {
        self.window.as_mut().filter(|w| w.chat() == chat)
    }

    fn on_history(&mut self, chat: ChatId, batch: Batch) {
        let Some(w) = self.open_window(chat) else {
            return;
        };
        let applied = w.apply(batch);
        let reloaded = applied == Applied::Reset;
        self.sync_rows(Some(applied));
        if reloaded {
            let newest = self.window.as_ref().and_then(MessageWindow::newest);
            self.echoes.reconciled(chat, newest);
        }
        self.after_echo_change();
    }

    /// Echoes changed: drop the ones whose server copy is shown, then
    /// refresh the echo rows.
    fn after_echo_change(&mut self) {
        if let Some(w) = &self.window {
            self.echoes.resolve(w.chat(), |id| w.contains(id));
        }
        self.sync_echoes();
    }

    /// Mirrors a window change into the list state.
    fn sync_rows(&mut self, applied: Option<Applied>) {
        match applied {
            None | Some(Applied::Ignored) => {}
            Some(Applied::Reset) => {
                self.shown_echoes = self.echo_count();
                self.rows.reset(self.row_count());
            }
            Some(Applied::Changed(ops)) => {
                for op in ops {
                    let next = match op {
                        RowOp::Splice { range, count } => {
                            let next = range.start + count;
                            self.rows.splice(range, count);
                            next
                        }
                        RowOp::Remeasure(ix) => ix,
                    };
                    if next < self.rows.item_count() {
                        self.rows.remeasure_items(next..next + 1);
                    }
                }
                // The first row carries the start-of-history marker.
                if self.rows.item_count() > 0 {
                    self.rows.remeasure_items(0..1);
                }
            }
        }
        debug_assert_eq!(self.rows.item_count(), self.row_count());
    }

    fn echo_count(&self) -> usize {
        self.window
            .as_ref()
            .map_or(0, |w| self.echoes.count(w.chat()))
    }

    /// Replaces the echo rows at the end of the list.
    fn sync_echoes(&mut self) {
        let start = self.window.as_ref().map_or(0, MessageWindow::len);
        let count = self.echo_count();
        self.rows.splice(start..start + self.shown_echoes, count);
        self.shown_echoes = count;
        debug_assert_eq!(self.rows.item_count(), self.row_count());
    }

    /// Opens `chat`. Core closes the previous one.
    pub(crate) fn select(&mut self, chat: ChatId) -> Option<Command> {
        if self.selected == Some(chat) {
            return None;
        }
        self.selected = Some(chat);
        self.window = Some(MessageWindow::new(chat));
        self.revealed.clear();
        self.read_sent = None;
        self.shown_echoes = self.echo_count();
        self.rows.reset(self.shown_echoes);
        self.rows.set_follow_mode(FollowMode::Tail);
        Some(Command::OpenChat(chat))
    }

    /// Where a composer submission would go: `Ok(None)` when nothing is open
    /// or the chat is read-only, `Busy` when the send table is full.
    pub(crate) fn send_target(&self) -> Result<Option<ChatId>, UserError> {
        let Some(chat) = self.selected_chat().filter(|c| c.can_send) else {
            return Ok(None);
        };
        if self.echoes.count_all() >= MAX_ECHOES {
            return Err(UserError::Busy);
        }
        Ok(Some(chat.id))
    }

    /// Sends `text` to `chat` (from [`Self::send_target`]) and shows its echo.
    pub(crate) fn submit(&mut self, chat: ChatId, text: String, now: i64) -> Option<Command> {
        if text.trim().is_empty() {
            return None;
        }
        let local_id = LocalId(self.next_local);
        let echo = Echo {
            local_id,
            chat,
            text: text.clone(),
            state: abyss_core::api::SendState::Sending,
            date: now,
        };
        self.echoes.push(echo).ok()?;
        self.next_local = self.next_local.wrapping_add(1);
        self.sync_echoes();
        self.rows.set_follow_mode(FollowMode::Tail);
        Some(Command::SendText {
            chat,
            text,
            local_id,
        })
    }

    pub(crate) fn retry(&mut self, local_id: LocalId) -> Option<Command> {
        let cmd = self.echoes.retry(local_id)?;
        self.sync_echoes();
        Some(cmd)
    }

    pub(crate) fn discard(&mut self, local_id: LocalId) -> Option<Command> {
        let cmd = self.echoes.discard(local_id)?;
        self.sync_echoes();
        Some(cmd)
    }

    /// Back to the live tail from far up the history: core reopens the chat
    /// at its tail.
    pub(crate) fn jump_to_latest(&mut self) -> Option<Command> {
        let chat = self.window.as_ref()?.chat();
        self.rows.set_follow_mode(FollowMode::Tail);
        Some(Command::OpenChat(chat))
    }

    /// Chat list row `last_visible` is on screen: maybe load the next page.
    pub(crate) fn dialogs_seen(&mut self, last_visible: usize) -> Option<Command> {
        self.list
            .wants_more(last_visible)
            .then_some(Command::LoadDialogs)
    }

    /// After a frame: load more history near either end, and mark read up to
    /// the newest message actually displayed while the window has focus.
    pub(crate) fn frame_seen(&mut self, seen: Seen) -> Vec<Command> {
        let mut commands = Vec::new();
        let focus = self.focus;
        let Some(w) = self.window.as_mut() else {
            return commands;
        };
        let chat = w.chat();
        if w.wants_older(seen.first_row) {
            commands.push(Command::LoadOlder(chat));
        }
        let Some(last) = seen.last_message else {
            return commands;
        };
        if w.wants_newer(last) {
            commands.push(Command::LoadNewer(chat));
        }
        let shown = w.get(last).map(|m| m.id);
        if let Some(max_id) = shown.filter(|id| self.read_sent.is_none_or(|r| *id > r))
            && focus == WindowState::Active
        {
            self.read_sent = Some(max_id);
            commands.push(Command::MarkRead { chat, max_id });
        }
        commands
    }

    /// The window gained or lost focus.
    pub(crate) fn set_focus(&mut self, state: WindowState) -> Option<Command> {
        if self.focus == state {
            return None;
        }
        self.focus = state;
        Some(Command::Window(state))
    }

    /// A core error: an outstanding history load may never be answered.
    pub(crate) fn load_failed(&mut self) {
        if let Some(w) = self.window.as_mut() {
            w.clear_pending();
        }
    }
}

#[cfg(test)]
mod tests;
