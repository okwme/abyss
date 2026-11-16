//! Glue between the session and the chat shell (`crate::chat`): routes chat
//! events in, sends the commands the shell decides on, and wires window focus,
//! system wake and the message list's scroll handler to it.

use abyss_core::api::{Event, WindowState};
use abyss_core::model::{ChatId, LocalId};
use gpui::{Context, Focusable, ListState, Subscription, Window};

use super::Root;
use super::auth::AuthView;
use super::session::Session;
use crate::chat::{Seen, format};
use crate::ui::copy;

/// Subscriptions that feed the chat shell: window focus and system wake.
pub(super) fn wire(
    rows: &ListState,
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> [Subscription; 2] {
    let this = cx.weak_entity();
    rows.set_scroll_handler(move |_, window, _| {
        // The list's state is borrowed while this runs: only schedule.
        let this = this.clone();
        window.on_next_frame(move |window, cx| {
            this.update(cx, |root, cx| {
                root.with_session(|s, cx| s.check_frame(window, cx), cx);
            })
            .ok();
        });
    });
    let activation = cx.observe_window_activation(window, |root, window, cx| {
        root.with_session(|s, cx| s.on_activation(window, cx), cx);
    });
    let this = cx.weak_entity();
    let wake = cx.on_system_wake(move |cx| {
        this.update(cx, |root, cx| {
            root.with_session(|s, _| s.notify_wake(), cx);
        })
        .ok();
    });
    [activation, wake]
}

/// What the last frame showed of the message list.
fn seen(rows: &ListState, messages: usize) -> Seen {
    let first_row = rows.logical_scroll_top().item_ix;
    // From the bottom up, the first message row not entirely below the
    // viewport. Rows that were never laid out answer `None` and are skipped.
    let last_message = (0..messages)
        .rev()
        .find(|ix| rows.item_is_below_viewport(*ix) == Some(false));
    Seen {
        first_row,
        last_message,
    }
}

impl Session {
    pub(super) fn apply_chat_event(
        &mut self,
        event: Event,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let rows_changed = matches!(
            event,
            Event::History { .. }
                | Event::Edited(_)
                | Event::Deleted { .. }
                | Event::ChatInvalidated(_)
                | Event::SendState { .. }
        );
        self.shell.apply(event);
        if rows_changed {
            self.schedule_frame_check(window, cx);
        }
    }

    /// After the next frame, check what it showed (load more, mark read).
    fn schedule_frame_check(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.shell.window().is_none() {
            return;
        }
        cx.on_next_frame(window, |root, window, cx| {
            root.with_session(|s, cx| s.check_frame(window, cx), cx);
        });
    }

    pub(super) fn check_frame(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let messages = self
            .shell
            .window()
            .map_or(0, crate::chat::messages::MessageWindow::len);
        let seen = seen(self.shell.rows(), messages);
        for cmd in self.shell.frame_seen(seen) {
            self.send(cmd, window, cx);
        }
    }

    fn on_activation(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let state = if window.is_window_active() {
            WindowState::Active
        } else {
            WindowState::Inactive
        };
        if let Some(cmd) = self.shell.set_focus(state) {
            self.send(cmd, window, cx);
        }
        if state == WindowState::Active {
            self.schedule_frame_check(window, cx);
        }
    }

    /// The Mac woke from sleep: core rebuilds rather than trust old sockets.
    fn notify_wake(&self) {
        tracing::info!("system woke");
        if let Ok(core) = self.core.try_borrow() {
            core.notify_wake();
        }
    }

    pub(super) fn select_chat(
        &mut self,
        chat: ChatId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        if let Some(cmd) = self.shell.select(chat) {
            self.send(cmd, window, cx);
        }
        if self.auth == AuthView::SignedIn {
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.schedule_frame_check(window, cx);
        cx.notify();
    }

    pub(super) fn submit_message(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.auth != AuthView::SignedIn || self.composer.read(cx).text().trim().is_empty() {
            return;
        }
        let chat = match self.shell.send_target() {
            Ok(Some(chat)) => chat,
            Ok(None) => return,
            Err(error) => {
                // The text stays in the composer.
                self.show_notice(copy::user_error(error), window, cx);
                return;
            }
        };
        let text = self.composer.update(cx, |c, cx| c.take_text(window, cx));
        if let Some(cmd) = self.shell.submit(chat, text, format::now()) {
            self.send(cmd, window, cx);
        }
        cx.notify();
    }

    pub(super) fn retry_send(
        &mut self,
        local_id: LocalId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        if let Some(cmd) = self.shell.retry(local_id) {
            self.send(cmd, window, cx);
        }
        cx.notify();
    }

    pub(super) fn discard_send(
        &mut self,
        local_id: LocalId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        if let Some(cmd) = self.shell.discard(local_id) {
            self.send(cmd, window, cx);
        }
        cx.notify();
    }

    pub(super) fn jump_to_latest(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if let Some(cmd) = self.shell.jump_to_latest() {
            self.send(cmd, window, cx);
        }
        cx.notify();
    }

    /// Chat list row `last_visible` was rendered: maybe load the next page.
    /// Called during render, so the command goes out after it.
    pub(super) fn dialogs_seen(
        &mut self,
        last_visible: usize,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        if let Some(cmd) = self.shell.dialogs_seen(last_visible) {
            cx.defer_in(window, move |root, window, cx| {
                root.with_session(|s, cx| s.send(cmd, window, cx), cx);
            });
        }
    }
}
