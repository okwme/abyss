//! Glue between the session and the chat shell (`crate::chat`): routes chat
//! events in, lends the shell an admission function so its UI changes follow
//! only admitted commands, wires window focus and system wake, checks what
//! the message list showed after each painted frame, and wakes the shell
//! when a refused or failed command's backoff ends (`chat::retry`).

use std::time::{Duration, Instant};

use abyss_core::SendError;
use abyss_core::api::{Command, Event, LoadTarget, SendState, UserError, WindowState};
use abyss_core::model::{ChatId, LocalId};
use gpui::{App, Context, Focusable, Subscription, WeakEntity, Window};

use super::Root;
use super::auth::AuthView;
use super::bridge::SharedCore;
use super::session::Session;
use crate::chat::{ChatShell, Refused, format};
use crate::ui::copy;

/// How often a flood-wait countdown redraws.
const COUNTDOWN_TICK: Duration = Duration::from_secs(1);

/// What to do when core refuses a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnRefusal {
    /// The user asked for it: say so.
    Notice,
    /// Driven by frames or rendering, and asked again on the next one. A
    /// notice would redraw, and the redraw would ask again at frame rate.
    Quiet,
}

/// Queues `cmd` with core without waiting.
pub(super) fn admit(core: &SharedCore, cmd: Command) -> Result<(), Refused> {
    let core = core.try_borrow().map_err(|_| Refused::Busy)?;
    match core.send(cmd) {
        Ok(()) => Ok(()),
        // The rejected command is dropped here (a password in it is
        // zeroized on drop).
        Err(SendError::Full(_)) => Err(Refused::Busy),
        Err(SendError::Closed(_)) => Err(Refused::Closed),
    }
}

/// Subscriptions that feed the chat shell: window focus and system wake.
pub(super) fn wire(window: &mut Window, cx: &mut Context<'_, Root>) -> [Subscription; 2] {
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

/// Called from the message pane's paint: once this frame is drawn, check
/// what the message list shows. Runs at the end of the frame's effect
/// cycle, after layout and paint, so it reads the layout on screen.
pub(super) fn after_paint(root: WeakEntity<Root>, window: &mut Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        root.update(cx, |root, cx| {
            root.with_session(|s, cx| s.check_frame(window, cx), cx);
        })
        .ok();
    });
}

impl Session {
    /// Runs `f` on the chat shell with an admission function, then reports
    /// a refusal as `on_refusal` says.
    fn drive<R>(
        &mut self,
        on_refusal: OnRefusal,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
        f: impl FnOnce(&mut ChatShell, crate::chat::Admit<'_>) -> Result<R, Refused>,
    ) -> Option<R> {
        let core = &self.core;
        let mut admit = |cmd: Command| admit(core, cmd);
        let out = match f(&mut self.shell, &mut admit) {
            Ok(r) => Some(r),
            Err(refused) => {
                tracing::debug!(?refused, "core refused a chat command");
                if on_refusal == OnRefusal::Notice {
                    self.show_refused(refused, window, cx);
                }
                None
            }
        };
        self.schedule_retry(window, cx);
        out
    }

    /// Wakes the shell once when the earliest waiting retry is due (a
    /// refused focus change, dialogs page, frame command or a failed load),
    /// unless an earlier wake-up is already scheduled.
    fn schedule_retry(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let now = cx.background_executor().now();
        let Some(due) = self.shell.retry_due(now) else {
            return;
        };
        if !self.shell.schedule_retry(due) {
            return;
        }
        let wait = due.saturating_duration_since(now);
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update_in(cx, |root, window, cx| {
                root.with_session(|s, cx| s.retry_fired(due, window, cx), cx);
            })
            .ok();
        })
        .detach();
    }

    /// A retry wake-up: re-sends what is due (focus, dialogs) and redraws,
    /// so the frame check retries frame-driven commands.
    fn retry_fired(&mut self, due: Instant, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.shell.retry_fired(due);
        let now = cx.background_executor().now();
        self.drive(OnRefusal::Quiet, window, cx, |s, admit| {
            let focus = if s.focus_out_of_sync() {
                s.sync_focus(now, admit)
            } else {
                Ok(())
            };
            focus.and(s.request_dialogs(now, admit))
        });
        cx.notify();
    }

    pub(super) fn show_refused(
        &mut self,
        refused: Refused,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let text = match refused {
            Refused::Busy => copy::user_error(UserError::Busy),
            Refused::Closed => copy::CORE_GONE.into(),
        };
        self.show_notice(text, window, cx);
    }

    pub(super) fn apply_chat_event(
        &mut self,
        event: Event,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let flood = matches!(
            event,
            Event::SendState {
                state: SendState::FloodWait { .. },
                ..
            }
        );
        let paged = matches!(
            event,
            Event::DialogsPaging { .. }
                | Event::LoadEnded {
                    target: LoadTarget::Dialogs,
                    ..
                }
        );
        let now = cx.background_executor().now();
        if let Event::LoadEnded { target, error } = event {
            self.shell.load_ended(target, error, now);
        } else {
            self.shell.apply(event);
        }
        if flood {
            self.start_countdown(window, cx);
        }
        // The sidebar may want the next page whatever it renders: an empty
        // list renders no row to ask (a first page of broadcast channels
        // only). A refusal retries after a backoff.
        if paged {
            self.drive(OnRefusal::Quiet, window, cx, |s, admit| {
                s.request_dialogs(now, admit)
            });
        }
        self.schedule_retry(window, cx);
    }

    /// After a painted frame: load more, mark read, report the live tail.
    pub(super) fn check_frame(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let seen = self.shell.seen();
        let now = cx.background_executor().now();
        self.drive(OnRefusal::Quiet, window, cx, |s, admit| {
            s.frame_seen(seen, now, admit)
        });
    }

    fn on_activation(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let state = if window.is_window_active() {
            WindowState::Active
        } else {
            WindowState::Inactive
        };
        let now = cx.background_executor().now();
        self.drive(OnRefusal::Quiet, window, cx, |s, admit| {
            s.set_focus(state, now, admit)
        });
        // Redraw, so the frame check sees the active window.
        cx.notify();
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
        let opened = self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.select(chat, admit)
        });
        if opened.is_some() && self.auth == AuthView::SignedIn {
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        cx.notify();
    }

    /// Enter in the composer. The text leaves the composer only once core
    /// admitted the send, together with its echo.
    pub(super) fn submit_message(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.auth != AuthView::SignedIn {
            return;
        }
        let chat = match self.shell.send_target() {
            Ok(Some(chat)) => chat,
            Ok(None) => return,
            Err(error) => {
                self.show_notice(copy::user_error(error), window, cx);
                return;
            }
        };
        let text = self.composer.read(cx).text().to_owned();
        let sent = self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.submit(chat, &text, format::now(), admit)
        });
        if let Some(Some(_)) = sent {
            self.composer
                .update(cx, |c, cx| drop(c.take_text(window, cx)));
        }
        cx.notify();
    }

    pub(super) fn retry_send(
        &mut self,
        local_id: LocalId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.retry(local_id, admit)
        });
        cx.notify();
    }

    pub(super) fn discard_send(
        &mut self,
        local_id: LocalId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.discard(local_id, admit)
        });
        cx.notify();
    }

    pub(super) fn jump_to_latest(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.drive(OnRefusal::Notice, window, cx, |s, admit| {
            s.jump_to_latest(admit)
        });
        cx.notify();
    }

    /// Chat list row `last_visible` was rendered: maybe load the next page.
    /// Called during render, so the request goes out after it.
    pub(super) fn dialogs_seen(
        &mut self,
        last_visible: usize,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let now = cx.background_executor().now();
        if !self.shell.dialogs_seen(last_visible, now) {
            return;
        }
        cx.defer_in(window, move |root, window, cx| {
            root.with_session(
                |s, cx| {
                    s.drive(OnRefusal::Quiet, window, cx, |s, admit| {
                        s.request_dialogs(now, admit)
                    })
                },
                cx,
            );
        });
    }

    /// Redraws flood-wait countdowns every second while one is shown.
    fn start_countdown(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !self.shell.start_countdown() {
            return; // already ticking
        }
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(COUNTDOWN_TICK).await;
                let more = this.update(cx, |root, cx| {
                    let more = root.with_session(|s, _| s.shell.countdown_tick(), cx);
                    cx.notify();
                    more
                });
                if more.ok().flatten() != Some(true) {
                    return;
                }
            }
        })
        .detach();
    }
}
