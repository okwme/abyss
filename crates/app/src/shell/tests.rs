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

use super::bridge::{CoreLink, DRAIN_MAX, SharedCore, ShutdownAck, drain};
use super::shutdown::Closing;
use super::{Failure, Root, State};
use crate::launch::{API_HASH_VAR, API_ID_VAR, LaunchError};

/// Commands the UI sent, and whether the queue reports full.
#[derive(Default)]
struct Sent {
    commands: Vec<Command>,
    full: bool,
}

struct TestLink {
    events: mpsc::UnboundedReceiver<Event>,
    sent: Rc<RefCell<Sent>>,
    ack: Option<oneshot::Receiver<ShutdownReport>>,
}

impl CoreLink for TestLink {
    fn send(&self, cmd: Command) -> Result<(), SendError> {
        let mut sent = self.sent.borrow_mut();
        if sent.full {
            return Err(SendError::Full(cmd));
        }
        sent.commands.push(cmd);
        Ok(())
    }

    fn poll_recv(&mut self, task: &mut task::Context<'_>) -> Poll<Option<Event>> {
        self.events.poll_next_unpin(task)
    }

    fn try_recv(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    fn begin_shutdown(&mut self) -> Option<ShutdownAck> {
        let ack = self.ack.take()?;
        Some(Box::pin(async move { ack.await.ok() }))
    }
}

#[cfg_attr(
    not(feature = "fake"),
    expect(dead_code, reason = "only the fake-login tests send commands and acks")
)]
struct Harness {
    events: mpsc::UnboundedSender<Event>,
    sent: Rc<RefCell<Sent>>,
    ack: Option<oneshot::Sender<ShutdownReport>>,
}

fn link() -> (Box<dyn CoreLink>, Harness) {
    let (events_tx, events) = mpsc::unbounded();
    let (ack_tx, ack) = oneshot::channel();
    let sent = Rc::new(RefCell::new(Sent::default()));
    let link = TestLink {
        events,
        sent: Rc::clone(&sent),
        ack: Some(ack),
    };
    let harness = Harness {
        events: events_tx,
        sent,
        ack: Some(ack_tx),
    };
    (Box::new(link), harness)
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
    let (link, harness) = link();
    let core: SharedCore = Rc::new(RefCell::new(link));
    for _ in 0..DRAIN_MAX + 10 {
        harness
            .events
            .unbounded_send(Event::DialogsReset(Vec::new()))
            .unwrap();
    }
    let first = core.borrow_mut().try_recv().unwrap();
    assert_eq!(drain(&core, first).len(), DRAIN_MAX);
    let first = core.borrow_mut().try_recv().unwrap();
    assert_eq!(drain(&core, first).len(), 10);
}

#[cfg(feature = "fake")]
mod fake {
    use abyss_core::api::UserError;
    use abyss_core::backend::fake::{FakeScript, summary};
    use abyss_core::model::{ChatId, PeerKind};

    use super::*;
    use crate::shell::auth::AuthView;
    use crate::shell::session::{LoginDriver, Started};
    use crate::ui::text_input;

    fn open(cx: &mut TestAppContext) -> (Entity<Root>, &mut VisualTestContext, Harness) {
        cx.update(|cx| cx.bind_keys(text_input::key_bindings()));
        let (core, harness) = link();
        let started = Started {
            core,
            driver: LoginDriver::Demo,
            fake: None,
        };
        let (root, cx) =
            cx.add_window_view(|window, cx| Root::with_started(Ok(started), window, cx));
        cx.run_until_parked();
        (root, cx, harness)
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

    fn sign_in(root: &Entity<Root>, cx: &mut VisualTestContext) {
        assert_eq!(
            read(root, cx, |s| s.auth.clone()),
            AuthView::Phone { error: None }
        );
        cx.simulate_input("+15550100");
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            read(root, cx, |s| s.auth.clone()),
            AuthView::Code { .. }
        ));
        cx.simulate_input("12345");
        cx.simulate_keystrokes("enter");
        assert!(matches!(
            read(root, cx, |s| s.auth.clone()),
            AuthView::Password { .. }
        ));
        cx.simulate_input("hunter2");
        cx.simulate_keystrokes("enter");
        assert_eq!(read(root, cx, |s| s.auth.clone()), AuthView::SignedIn);
    }

    #[gpui::test]
    fn demo_login_then_chats_then_send(cx: &mut TestAppContext) {
        let (root, cx, harness) = open(cx);
        sign_in(&root, cx);
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
        sign_in(&root, cx);
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
            .send(ShutdownReport::Unconfirmed)
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
