//! Chat list, message view and sending (plan sections 7–9): the state the UI
//! keeps and the decisions it makes (what to load, what to mark read, what a
//! row shows), apart from rendering so they can be tested without a window.
//! The shell (`shell::chats`) feeds it events and lends it an [`Admit`] to
//! queue commands with core (`commands.rs`); `shell::chat_view` renders it.

mod commands;
pub(crate) mod copy;
#[cfg(feature = "fake")]
pub(crate) mod demo;
pub(crate) mod format;
pub(crate) mod list;
pub(crate) mod messages;
pub(crate) mod retry;
pub(crate) mod rich;
pub(crate) mod sends;
pub(crate) mod viewport;

use std::collections::HashSet;
use std::time::Instant;

use abyss_core::api::{Event, LoadTarget, UserError, WindowState};
use abyss_core::model::{ChatId, ChatSummary, Message, MsgId};
use abyss_core::store::ViewGen;
use gpui::{FollowMode, ListAlignment, ListState, px};

pub(crate) use self::commands::{Admit, Refused};
use self::list::{ChatList, Pages};
use self::messages::{Applied, Batch, MessageWindow, RowOp};
use self::retry::{Backoff, earliest};
use self::sends::{Echo, Echoes};
use self::viewport::Seen;

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

pub(crate) struct ChatShell {
    pub(crate) list: ChatList,
    selected: Option<ChatId>,
    window: Option<MessageWindow>,
    echoes: Echoes,
    /// The message list's GPUI state. Always `window.len() + shown_echoes` rows.
    rows: ListState,
    shown_echoes: usize,
    revealed: HashSet<MsgId>,
    /// Highest id core admitted as read for the open chat.
    read_sent: Option<MsgId>,
    /// The chat core was last told shows its live tail (`ViewingTail`).
    viewing: Option<ChatId>,
    next_local: u64,
    /// Whether the window is active, as the OS last said. Reads and the
    /// live tail follow this at once, whatever core was told.
    active: WindowState,
    /// The window state core last admitted (`Command::Window`).
    focus_sent: WindowState,
    /// Bringing core in line with `active` after a refusal.
    focus_retry: Backoff,
    /// Frame-driven commands (loads, read marks, the live tail) core
    /// refused: the next frame retries, and a wake-up makes sure one comes.
    frame_retry: Backoff,
    /// When the pending wake-up for retries fires, if one is scheduled.
    retry_timer: Option<Instant>,
    countdown: Countdown,
    /// Where the message rows were painted this frame.
    painted: viewport::Painted,
}

/// Whether a flood-wait countdown ticker runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Countdown {
    Idle,
    Running,
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
            viewing: None,
            next_local: 1,
            active: WindowState::Active,
            focus_sent: WindowState::Active,
            focus_retry: Backoff::default(),
            frame_retry: Backoff::default(),
            retry_timer: None,
            countdown: Countdown::Idle,
            painted: viewport::Painted::default(),
        }
    }
}

impl std::fmt::Debug for ChatShell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatShell")
            .field("chats", &self.list.len())
            .field("selected", &self.selected)
            .field("rows", &self.rows.item_count())
            .field("viewing", &self.viewing)
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

    /// Where message rows record their painted bounds.
    pub(crate) fn painted(&self) -> &viewport::Painted {
        &self.painted
    }

    /// What the frame just painted showed of the message list.
    pub(crate) fn seen(&self) -> Option<Seen> {
        let messages = self.window.as_ref().map_or(0, MessageWindow::len);
        viewport::seen(&self.painted.take(), self.rows.viewport_bounds(), messages)
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

    /// The chat core was last told shows its live tail.
    #[cfg(test)]
    pub(crate) fn viewing(&self) -> Option<ChatId> {
        self.viewing
    }

    /// Starts the flood-wait countdown ticker unless it runs. Returns
    /// whether the caller should start one.
    pub(crate) fn start_countdown(&mut self) -> bool {
        let start = self.countdown == Countdown::Idle && self.echoes.any_waiting();
        if start {
            self.countdown = Countdown::Running;
        }
        start
    }

    /// One second passed: redraws the echo rows. Returns whether to keep
    /// ticking (an echo still counts down).
    pub(crate) fn countdown_tick(&mut self) -> bool {
        self.remeasure_echoes();
        let more = self.echoes.any_waiting();
        if !more {
            self.countdown = Countdown::Idle;
        }
        more
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
            Event::DialogsPaging { has_more } => {
                self.list
                    .paged(if has_more { Pages::More } else { Pages::Done });
            }
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
            Event::Reconciled { chat, view, newest } => self.on_reconciled(chat, view, newest),
            Event::SendsCovered { sends, .. } => {
                if self.echoes.covered(&sends) {
                    self.sync_echoes();
                }
            }
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
                if self
                    .echoes
                    .set_state(local_id, state, Instant::now())
                    .is_some()
                {
                    self.after_echo_change();
                }
            }
            // `LoadEnded` needs the time: see [`Self::load_ended`]. Errors
            // never end a load: core says which load ended.
            Event::Auth(_)
            | Event::CredentialSource(_)
            | Event::Connection(_)
            | Event::Error(_)
            | Event::LoadEnded { .. } => {}
        }
    }

    /// A load the UI asked for ended without its answer
    /// (`Event::LoadEnded`) at `now`.
    pub(crate) fn load_ended(
        &mut self,
        target: LoadTarget,
        error: Option<UserError>,
        now: Instant,
    ) {
        match target {
            LoadTarget::Dialogs => self.list.load_ended(error, now),
            LoadTarget::History { chat, dir } => {
                if let Some(w) = self.open_window(chat) {
                    w.load_ended(dir, error, now);
                }
            }
        }
    }

    /// When the shell next wants a wake-up to retry something refused or
    /// failed, if anything waits past `now`.
    pub(crate) fn retry_due(&self, now: Instant) -> Option<Instant> {
        let focus = self
            .focus_out_of_sync()
            .then(|| self.focus_retry.due(now))
            .flatten();
        let window = self.window.as_ref().and_then(|w| w.retry_due(now));
        [self.list.retry_due(now), window, self.frame_retry.due(now)]
            .into_iter()
            .fold(focus, earliest)
    }

    /// A wake-up for `due` is to be scheduled unless an earlier one already
    /// is; returns whether to schedule it. At most one is outstanding per
    /// distinct earlier instant, so wake-ups never outnumber failures.
    pub(crate) fn schedule_retry(&mut self, due: Instant) -> bool {
        if self.retry_timer.is_some_and(|t| t <= due) {
            return false;
        }
        self.retry_timer = Some(due);
        true
    }

    /// The wake-up for `due` fired.
    pub(crate) fn retry_fired(&mut self, due: Instant) {
        if self.retry_timer == Some(due) {
            self.retry_timer = None;
        }
    }

    fn open_window(&mut self, chat: ChatId) -> Option<&mut MessageWindow> {
        self.window.as_mut().filter(|w| w.chat() == chat)
    }

    /// A history event. A new view (a reopened chat's full `Tail`, a
    /// reload) replaces the rows; echoes are reconciled only by
    /// `Reconciled`, never by a view change.
    fn on_history(&mut self, chat: ChatId, batch: Batch) {
        let Some(w) = self.open_window(chat) else {
            return;
        };
        let applied = w.apply(batch);
        self.sync_rows(Some(applied));
        self.after_echo_change();
    }

    /// Core merged the server's tail of `chat` into `view`. It also ends
    /// any history load asked of that view: core answers a load asked while
    /// the tail loaded after it, and asking again is harmless.
    fn on_reconciled(&mut self, chat: ChatId, view: ViewGen, newest: Option<MsgId>) {
        if let Some(w) = self.open_window(chat) {
            w.reconciled(view);
        }
        let on_screen = self
            .window
            .as_ref()
            .filter(|w| w.chat() == chat && w.view() == Some(view));
        let changed = match on_screen {
            Some(w) => {
                let shown = |id: MsgId| w.contains(id);
                self.echoes.reconciled(chat, newest, Some(&shown))
            }
            None => self.echoes.reconciled(chat, newest, None),
        };
        if changed {
            self.sync_echoes();
        }
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

    /// Echo rows show a countdown that changed: redraw them.
    fn remeasure_echoes(&self) {
        let start = self.window.as_ref().map_or(0, MessageWindow::len);
        if self.shown_echoes > 0 {
            self.rows.remeasure_items(start..start + self.shown_echoes);
        }
    }
}

#[cfg(test)]
mod tests;
