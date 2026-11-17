//! The main window through GPUI's test platform, against a same-thread core
//! double (GPUI's test scheduler rejects wakeups from core's real threads).

use std::cell::RefCell;
use std::rc::Rc;
use std::task::{self, Poll};

use abyss_core::api::{Command, Event};
use abyss_core::{SendError, ShutdownReport};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{Entity, TestAppContext, VisualTestContext};

use super::bridge::{CoreLink, DRAIN_MAX, EventSource, ShutdownAck, drain};
use super::shutdown::Closing;
use super::{Failure, Root, State};
use crate::launch::{API_HASH_VAR, API_ID_VAR, LaunchError};

/// Commands the UI sent, and whether the queue reports full.
#[derive(Default)]
struct Sent {
    commands: Vec<Command>,
    full: bool,
}

/// The command half of the double.
struct TestLink {
    sent: Rc<RefCell<Sent>>,
    ack: Option<oneshot::Receiver<ShutdownReport>>,
}

/// The event half of the double, fed by [`Harness::events`].
struct TestEvents(mpsc::UnboundedReceiver<Event>);

impl CoreLink for TestLink {
    fn send(&self, cmd: Command) -> Result<(), SendError> {
        let mut sent = self.sent.borrow_mut();
        if sent.full {
            return Err(SendError::Full(cmd));
        }
        sent.commands.push(cmd);
        Ok(())
    }

    fn begin_shutdown(&mut self) -> Option<ShutdownAck> {
        let ack = self.ack.take()?;
        Some(Box::pin(async move { ack.await.ok() }))
    }
}

impl EventSource for TestEvents {
    fn poll_recv(&mut self, task: &mut task::Context<'_>) -> Poll<Option<Event>> {
        self.0.poll_next_unpin(task)
    }

    fn try_recv(&mut self) -> Option<Event> {
        self.0.try_recv().ok()
    }
}

#[cfg_attr(
    not(feature = "fake"),
    expect(
        dead_code,
        reason = "only the fake-backed tests send commands and acks"
    )
)]
struct Harness {
    events: mpsc::UnboundedSender<Event>,
    sent: Rc<RefCell<Sent>>,
    ack: Option<oneshot::Sender<ShutdownReport>>,
}

fn link() -> (Box<dyn CoreLink>, Box<dyn EventSource>, Harness) {
    let (events_tx, events) = mpsc::unbounded();
    let (ack_tx, ack) = oneshot::channel();
    let sent = Rc::new(RefCell::new(Sent::default()));
    let link = TestLink {
        sent: Rc::clone(&sent),
        ack: Some(ack),
    };
    let harness = Harness {
        events: events_tx,
        sent,
        ack: Some(ack_tx),
    };
    (Box::new(link), Box::new(TestEvents(events)), harness)
}

fn closing(root: &Entity<Root>, cx: &mut VisualTestContext) -> Option<Closing> {
    root.read_with(cx, |root, _| match &root.state {
        State::Running(s) => Some(s.closing),
        State::Failed(_) => None,
    })
}

#[gpui::test]
fn missing_credentials_show_the_error_screen_and_close_freely(cx: &mut TestAppContext) {
    let missing = LaunchError::MissingCredentials(vec![API_ID_VAR, API_HASH_VAR]);
    let (root, cx) = cx.add_window_view(|window, cx| Root::new(Err(missing), window, cx));
    cx.run_until_parked();
    root.read_with(cx, |root, _| {
        assert!(matches!(
            root.state,
            State::Failed(Failure::Launch(LaunchError::MissingCredentials(_)))
        ));
    });
    assert_eq!(closing(&root, cx), None);
    assert!(
        cx.simulate_close(),
        "nothing to wait for: the window closes"
    );
}

#[test]
fn drain_takes_at_most_drain_max_per_wake() {
    let (_link, mut events, harness) = link();
    for _ in 0..DRAIN_MAX + 10 {
        harness
            .events
            .unbounded_send(Event::DialogsReset(Vec::new()))
            .unwrap();
    }
    let first = events.try_recv().unwrap();
    assert_eq!(drain(events.as_mut(), first).len(), DRAIN_MAX);
    let first = events.try_recv().unwrap();
    assert_eq!(drain(events.as_mut(), first).len(), 10);
}

/// The panel works in any state, here on the error screen. No store is set
/// up in tests, so the choice stays in memory.
#[gpui::test]
fn appearance_choice_rethemes_the_window_and_reset_restores_it(cx: &mut TestAppContext) {
    use crate::ui::theme::{Choice, Hue, Mode, Settings, Themes, Tint};

    let missing = LaunchError::MissingCredentials(vec![API_ID_VAR]);
    let (root, cx) = cx.add_window_view(|window, cx| Root::new(Err(missing), window, cx));
    cx.run_until_parked();
    let showing = cx.update(|window, _| Mode::of(window));
    let before = cx.update(|window, cx| Themes::of(window, cx));
    assert_eq!(cx.update(|_, cx| Themes::settings(cx)), Settings::default());

    root.update(cx, Root::toggle_appearance);
    cx.run_until_parked();
    root.read_with(cx, |root, _| assert!(root.appearance_open));

    let choice = Choice {
        hue: Hue::all().next().unwrap(),
        tint: Tint::all().last().unwrap(),
    };
    cx.update(|_, cx| Themes::set(Settings::default().with(showing, choice), cx));
    cx.run_until_parked();
    assert_ne!(cx.update(|window, cx| Themes::of(window, cx)), before);
    assert_eq!(cx.update(|_, cx| Themes::settings(cx)).get(showing), choice);

    cx.update(|_, cx| Themes::set(Settings::default(), cx));
    cx.run_until_parked();
    assert_eq!(cx.update(|window, cx| Themes::of(window, cx)), before);

    root.update(cx, Root::toggle_appearance);
    root.read_with(cx, |root, _| assert!(!root.appearance_open));
}

#[cfg(feature = "fake")]
mod fake {
    use abyss_core::api::{AuthState, PasswordBuf, UserError};
    use abyss_core::backend::fake::{FakeScript, summary};
    use abyss_core::model::{ChatId, PeerKind};

    use super::*;
    use crate::shell::auth::AuthView;
    use crate::shell::session::Started;
    use crate::ui::text_input;

    /// A window past the Welcome screen, which has its own test.
    fn open(cx: &mut TestAppContext) -> (Entity<Root>, &mut VisualTestContext, Harness) {
        let (root, cx, harness) = open_at_welcome(cx);
        root.update(cx, |root, cx| {
            root.with_session(|s, _| s.welcomed = true, cx);
        });
        (root, cx, harness)
    }

    fn open_at_welcome(cx: &mut TestAppContext) -> (Entity<Root>, &mut VisualTestContext, Harness) {
        cx.update(|cx| {
            cx.bind_keys(text_input::key_bindings());
            cx.bind_keys(crate::shell::key_bindings());
        });
        let (commands, events, harness) = link();
        let started = Started {
            commands,
            events,
            fake: None,
        };
        let (root, cx) =
            cx.add_window_view(|window, cx| Root::with_started(Ok(started), window, cx));
        cx.run_until_parked();
        (root, cx, harness)
    }

    /// A signed-out run shows the Welcome screen first. Typing does nothing
    /// there; Enter leaves it, and the phone field then has focus.
    #[gpui::test]
    fn welcome_comes_before_the_phone_step(cx: &mut TestAppContext) {
        let (root, cx, harness) = open_at_welcome(cx);
        core_says(&harness, cx, AuthState::Phone { error: None });
        assert!(!read(&root, cx, |s| s.welcomed));
        cx.simulate_input("+15550100");
        cx.simulate_keystrokes("enter");
        assert!(read(&root, cx, |s| s.welcomed), "Enter leaves Welcome");
        assert!(
            last_command(&harness).is_none(),
            "nothing typed on Welcome reaches core"
        );
        cx.simulate_input("+15550100");
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            last_command(&harness),
            Some(Command::SubmitPhone(p)) if p == "+15550100"
        ));
    }

    fn read<R>(
        root: &Entity<Root>,
        cx: &mut VisualTestContext,
        f: impl FnOnce(&super::super::Session) -> R,
    ) -> R {
        root.read_with(cx, |root, _| match &root.state {
            State::Running(s) => f(s),
            State::Failed(failure) => panic!("failed: {failure:?}"),
        })
    }

    fn auth(root: &Entity<Root>, cx: &mut VisualTestContext) -> AuthView {
        read(root, cx, |s| s.auth.clone())
    }

    /// Core moves to `state`.
    fn core_says(harness: &Harness, cx: &mut VisualTestContext, state: AuthState) {
        harness.events.unbounded_send(Event::Auth(state)).unwrap();
        cx.run_until_parked();
    }

    fn last_command(harness: &Harness) -> Option<Command> {
        let sent = harness.sent.borrow();
        sent.commands.last().map(|c| match c {
            // `Command` is not `Clone` (it may carry a password).
            Command::SubmitPhone(p) => Command::SubmitPhone(p.clone()),
            Command::SubmitCode(c) => Command::SubmitCode(c.clone()),
            Command::SubmitPassword(p) => {
                Command::SubmitPassword(PasswordBuf::from_slice(p.as_bytes()).unwrap())
            }
            Command::RetryPasswordParams => Command::RetryPasswordParams,
            _ => Command::Logout,
        })
    }

    /// The router follows core's states; each form sends its command.
    fn sign_in(root: &Entity<Root>, cx: &mut VisualTestContext, harness: &Harness) {
        assert_eq!(auth(root, cx), AuthView::Connecting);
        core_says(harness, cx, AuthState::Phone { error: None });
        assert_eq!(auth(root, cx), AuthView::Phone { error: None });
        cx.simulate_input("+15550100");
        cx.simulate_keystrokes("enter");
        assert!(matches!(last_command(harness), Some(Command::SubmitPhone(p)) if p == "+15550100"));
        let phone_hint = abyss_core::api::mask_phone("+15550100");
        core_says(
            harness,
            cx,
            AuthState::Code {
                phone_hint: phone_hint.clone(),
                error: None,
            },
        );
        assert_eq!(
            auth(root, cx),
            AuthView::Code {
                phone_hint: phone_hint.into(),
                error: None
            }
        );
        cx.simulate_input("12345");
        cx.simulate_keystrokes("enter");
        assert!(matches!(last_command(harness), Some(Command::SubmitCode(c)) if c == "12345"));
        core_says(
            harness,
            cx,
            AuthState::Password {
                hint: Some("demo".into()),
                error: None,
            },
        );
        assert!(matches!(auth(root, cx), AuthView::Password { .. }));
        cx.simulate_input("hunter2");
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            last_command(harness),
            Some(Command::SubmitPassword(p)) if p.as_bytes() == b"hunter2"
        ));
        core_says(
            harness,
            cx,
            AuthState::SignedIn {
                display_name: "Demo User".into(),
            },
        );
        assert_eq!(auth(root, cx), AuthView::SignedIn);
    }

    #[gpui::test]
    fn used_up_password_token_asks_for_new_params_first(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        core_says(
            &harness,
            cx,
            AuthState::Password {
                hint: Some("h".into()),
                error: None,
            },
        );
        core_says(
            &harness,
            cx,
            AuthState::PasswordTokenUnavailable {
                error: UserError::Network,
            },
        );
        assert_eq!(
            auth(&root, cx),
            AuthView::Password {
                hint: Some("h".into()),
                error: Some(UserError::Network)
            }
        );
        cx.simulate_input("pw");
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            last_command(&harness),
            Some(Command::RetryPasswordParams)
        ));
        core_says(
            &harness,
            cx,
            AuthState::Password {
                hint: Some("h".into()),
                error: None,
            },
        );
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            last_command(&harness),
            Some(Command::SubmitPassword(p)) if p.as_bytes() == b"pw"
        ));
    }

    #[gpui::test]
    fn states_without_a_screen_show_a_notice(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        core_says(
            &harness,
            cx,
            AuthState::SignedInUnsaved {
                display_name: String::new(),
            },
        );
        assert_eq!(auth(&root, cx), AuthView::SignedIn);
        assert_eq!(
            read(&root, cx, |s| s.notice.clone()),
            Some(crate::ui::copy::SIGNED_IN_UNSAVED.into())
        );
        core_says(&harness, cx, AuthState::SignUpRequired);
        assert_eq!(
            auth(&root, cx),
            AuthView::Phone {
                error: Some(UserError::SignUpRequired)
            }
        );
    }

    #[gpui::test]
    fn demo_login_then_chats_then_send(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        sign_in(&root, cx, &harness);
        let script = FakeScript::demo(3, 0);
        harness
            .events
            .unbounded_send(Event::DialogsReset(script.dialogs))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(read(&root, cx, |s| s.shell.list.as_slice().len()), 3);

        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.select_chat(chat, window, cx), cx);
        });
        cx.run_until_parked();
        cx.simulate_input("hello");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("there");
        cx.simulate_keystrokes("enter");
        let sent = harness.sent.borrow();
        // The short list also asks for the next dialogs page (LoadDialogs).
        let [
            ..,
            Command::OpenChat(_),
            Command::SendText { chat: to, text, .. },
        ] = sent.commands.as_slice()
        else {
            panic!("expected OpenChat, then one SendText");
        };
        assert_eq!(*to, chat);
        assert_eq!(text, "hello\nthere");
        drop(sent);
        // Live updates apply.
        let mut bumped = summary(
            ChatId {
                kind: PeerKind::User,
                id: 2,
            },
            "Bumped",
        );
        bumped.unread = 4;
        // A new message moves the chat up; read-state changes alone do not.
        bumped.last_date = i64::from(i32::MAX);
        harness
            .events
            .unbounded_send(Event::ChatBumped(bumped))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            read(&root, cx, |s| s.shell.list.as_slice()[0].title.clone()),
            "Bumped"
        );
    }

    #[gpui::test]
    fn full_queue_shows_busy(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        sign_in(&root, cx, &harness);
        harness.sent.borrow_mut().full = true;
        let script = FakeScript::demo(1, 0);
        harness
            .events
            .unbounded_send(Event::DialogsReset(script.dialogs))
            .unwrap();
        cx.run_until_parked();
        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.select_chat(chat, window, cx), cx);
        });
        cx.simulate_input("hi");
        cx.simulate_keystrokes("enter");
        let notice = read(&root, cx, |s| s.notice.clone());
        assert_eq!(notice, Some(crate::ui::copy::user_error(UserError::Busy)));
    }

    #[gpui::test]
    fn close_waits_for_core_then_reports_unconfirmed(cx: &mut TestAppContext) {
        let (root, cx, mut harness) = open(cx);
        assert_eq!(closing(&root, cx), Some(Closing::Open));
        assert!(
            !cx.simulate_close(),
            "the window stays open until core acks"
        );
        assert_eq!(closing(&root, cx), Some(Closing::Waiting));
        assert!(
            !cx.simulate_close(),
            "a second close while closing is refused"
        );
        harness
            .ack
            .take()
            .unwrap()
            .send(ShutdownReport::UNCONFIRMED)
            .unwrap();
        cx.run_until_parked();
        assert_eq!(closing(&root, cx), Some(Closing::Unconfirmed));
        assert!(cx.simulate_close(), "after the message, closing is allowed");
    }

    #[gpui::test]
    fn ui_timeout_ends_a_stalled_shutdown(cx: &mut TestAppContext) {
        let (root, cx, _harness) = open(cx);
        assert!(!cx.simulate_close());
        cx.executor()
            .advance_clock(crate::shell::shutdown::UI_TIMEOUT);
        cx.run_until_parked();
        // Timed out: the app quits (the test platform ignores the quit), and
        // the UI never claims the save was confirmed.
        assert_eq!(closing(&root, cx), Some(Closing::Waiting));
    }
}

#[cfg(feature = "snapshots")]
mod snapshots;
