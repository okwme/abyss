use abyss_core::api::{Command, HistoryDir, SendState, WindowState};
use abyss_core::model::{Body, ChatKind, LocalId, PeerKind, ThreadKey};
use abyss_core::rich_text::RichText;
use abyss_core::store::ViewGen;
use gpui::{
    AppContext as _, Context, IntoElement, ListOffset, Render, Styled, TestAppContext, Window, div,
    list, point, px, size,
};

use super::viewport::Seen;
use super::*;

const CHAT: ChatId = ChatId {
    kind: PeerKind::User,
    id: 1,
};

/// Core's command queue: records what it admits, refuses all while full.
#[derive(Default)]
struct Core {
    sent: Vec<Command>,
    full: bool,
}

impl Core {
    fn admit(&mut self) -> impl FnMut(Command) -> Result<(), Refused> + '_ {
        move |cmd| {
            if self.full {
                return Err(Refused::Busy);
            }
            self.sent.push(cmd);
            Ok(())
        }
    }

    fn take(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.sent)
    }
}

fn summary() -> ChatSummary {
    ChatSummary {
        id: CHAT,
        title: "c".into(),
        kind: ChatKind::Private,
        pinned: false,
        unread: 2,
        unread_mentions: 0,
        last: None,
        last_date: 0,
        last_id: None,
        can_send: true,
    }
}

fn read_only() -> ChatSummary {
    ChatSummary {
        can_send: false,
        ..summary()
    }
}

fn msg(id: i32) -> Message {
    Message {
        id: MsgId(id),
        thread: ThreadKey {
            chat: CHAT,
            topic: None,
        },
        sender: None,
        date: i64::from(id),
        edit_date: None,
        outgoing: false,
        reply_to: None,
        body: Body::Text(RichText::plain("m")),
    }
}

fn history(view: u64, dir: HistoryDir, ids: impl IntoIterator<Item = i32>) -> Event {
    Event::History {
        chat: CHAT,
        view: ViewGen(view),
        dir,
        messages: ids.into_iter().map(msg).collect(),
        removed: Vec::new(),
        has_more: true,
    }
}

/// A live message appended to the attached view 1.
fn live(id: i32) -> Event {
    Event::History {
        chat: CHAT,
        view: ViewGen(1),
        dir: HistoryDir::Newer,
        messages: vec![msg(id)],
        removed: Vec::new(),
        has_more: false,
    }
}

fn reconciled(view: u64, newest: i32) -> Event {
    Event::Reconciled {
        chat: CHAT,
        view: ViewGen(view),
        newest: Some(MsgId(newest)),
    }
}

/// A shell with `CHAT` open and messages 51..=100 loaded.
fn open(core: &mut Core) -> ChatShell {
    let mut shell = ChatShell::default();
    shell.apply(Event::DialogsReset(vec![summary()]));
    shell.select(CHAT, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::OpenChat(CHAT)]));
    shell.select(CHAT, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "already open");
    shell.apply(history(1, HistoryDir::Tail, 51..=100));
    shell
}

fn send(shell: &mut ChatShell, core: &mut Core, text: &str) -> LocalId {
    let chat = shell.send_target().unwrap().unwrap();
    shell
        .submit(chat, text, 0, &mut core.admit())
        .unwrap()
        .unwrap()
}

fn set(shell: &mut ChatShell, local_id: LocalId, state: SendState) {
    shell.apply(Event::SendState { local_id, state });
}

fn state_of(shell: &ChatShell, ix: usize) -> Option<SendState> {
    match shell.row(ix)? {
        Row::Echo(e) => Some(e.state),
        Row::Message(_) => None,
    }
}

fn reads(cmds: &[Command]) -> Vec<i32> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::MarkRead { max_id, .. } => Some(max_id.0),
            _ => None,
        })
        .collect()
}

fn viewing(cmds: &[Command]) -> Vec<Option<ChatId>> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::ViewingTail(chat) => Some(*chat),
            _ => None,
        })
        .collect()
}

#[test]
fn echo_lifecycle_sent_then_replaced_by_the_server_copy() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "hello");
    assert!(matches!(
        core.take().as_slice(),
        [Command::SendText { text, local_id, .. }] if text == "hello" && *local_id == id
    ));
    assert_eq!(shell.row_count(), 51);
    assert_eq!(shell.rows().item_count(), 51);
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));
    set(&mut shell, id, SendState::Sent { id: MsgId(101) });
    assert_eq!(
        state_of(&shell, 50),
        Some(SendState::Sent { id: MsgId(101) })
    );
    shell.apply(history(1, HistoryDir::Newer, [101]));
    assert_eq!(shell.row_count(), 51, "the message replaced the echo");
    assert_eq!(shell.rows().item_count(), 51);
    assert!(matches!(shell.row(50), Some(Row::Message(m)) if m.id == MsgId(101)));
}

/// Finding 1 (HIGH): a full queue must not strand a send.
#[test]
fn full_queue_creates_no_echo_and_changes_nothing() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    core.full = true;
    let refused = shell.submit(CHAT, "draft", 0, &mut core.admit());
    assert_eq!(refused, Err(Refused::Busy));
    assert_eq!(shell.row_count(), 50, "no echo for a send core never took");
    assert_eq!(shell.echoes().count(CHAT), 0);
    core.full = false;
    let id = send(&mut shell, &mut core, "draft");
    assert_eq!(id, LocalId(1), "the refused send used up no id");

    // Retry and Discard change the echo only once core admits them.
    set(&mut shell, id, SendState::Failed);
    core.full = true;
    assert_eq!(shell.retry(id, &mut core.admit()), Err(Refused::Busy));
    assert_eq!(state_of(&shell, 50), Some(SendState::Failed), "still Retry");
    assert_eq!(shell.discard(id, &mut core.admit()), Err(Refused::Busy));
    assert_eq!(shell.row_count(), 51, "a refused discard keeps the echo");
    core.full = false;
    shell.retry(id, &mut core.admit()).unwrap();
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));

    // Opening another chat: refused, the open chat stays.
    let other = ChatId {
        kind: PeerKind::User,
        id: 2,
    };
    core.full = true;
    assert_eq!(shell.select(other, &mut core.admit()), Err(Refused::Busy));
    assert_eq!(shell.selected(), Some(CHAT));
}

#[test]
fn failed_send_retries_then_is_discarded() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "x");
    core.take();
    shell.discard(id, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "in flight: nothing to discard");
    set(&mut shell, id, SendState::Failed);
    shell.retry(id, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::RetrySend(r)] if *r == id));
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));
    set(&mut shell, id, SendState::Unknown { attempts: 3 });
    shell.discard(id, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::DiscardSend(r)] if *r == id));
    assert_eq!(shell.row_count(), 50);
    assert_eq!(shell.rows().item_count(), 50);
}

/// Finding 7: a flood-waiting send waits; Discard works, Retry is core's.
#[test]
fn flood_waiting_send_can_be_discarded_but_not_retried() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "x");
    core.take();
    set(&mut shell, id, SendState::FloodWait { seconds: 30 });
    assert!(shell.start_countdown(), "a countdown starts");
    assert!(!shell.start_countdown(), "only one ticker");
    shell.retry(id, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "core resends it by itself");
    set(&mut shell, id, SendState::FloodWait { seconds: 5 });
    assert!(shell.countdown_tick(), "still waiting");
    shell.discard(id, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::DiscardSend(r)] if *r == id));
    assert_eq!(shell.row_count(), 50);
    assert!(!shell.countdown_tick(), "nothing waits: the ticker stops");
}

/// Finding 4: echoes are reconciled on `Reconciled`, not on view resets,
/// and an unconfirmed one outlives a reconcile that may predate its send.
#[test]
fn unconfirmed_echo_outlives_view_resets_and_one_reconcile() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let id = send(&mut shell, &mut core, "x");
    set(&mut shell, id, SendState::SentUnconfirmed);
    shell.apply(Event::ChatInvalidated(CHAT));
    shell.apply(history(2, HistoryDir::Tail, 60..=101));
    assert_eq!(shell.row_count(), 43, "a new view keeps the echo");
    shell.apply(reconciled(2, 101));
    assert_eq!(shell.row_count(), 43, "this tail may predate the send");
    shell.apply(reconciled(2, 101));
    assert_eq!(
        shell.row_count(),
        42,
        "a later reconcile shows the server copy"
    );
    assert_eq!(shell.rows().item_count(), 42);
}

#[test]
fn reconcile_drops_a_sent_echo_whose_message_was_deleted() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let kept = send(&mut shell, &mut core, "a");
    let gone = send(&mut shell, &mut core, "b");
    send(&mut shell, &mut core, "c");
    // 99 was deleted on the server before its id reached us.
    shell.apply(Event::Deleted {
        chat: CHAT,
        ids: vec![MsgId(99)],
    });
    set(&mut shell, kept, SendState::Sent { id: MsgId(150) });
    set(&mut shell, gone, SendState::Sent { id: MsgId(99) });
    assert_eq!(shell.row_count(), 52);
    shell.apply(reconciled(7, 100));
    assert_eq!(shell.row_count(), 52, "another view is not on screen");
    shell.apply(reconciled(1, 100));
    assert_eq!(shell.row_count(), 51);
    assert_eq!(
        state_of(&shell, 49),
        Some(SendState::Sent { id: MsgId(150) })
    );
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));
}

/// A reopened retained chat arrives as one full `Tail` of a new view, which
/// replaces the rows.
#[test]
fn reopening_a_chat_replaces_the_view_with_its_full_tail() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    shell.jump_to_latest(&mut core.admit()).unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [.., Command::OpenChat(CHAT)]
    ));
    shell.apply(history(3, HistoryDir::Tail, 41..=100));
    assert_eq!(shell.row_count(), 60, "replaced, not appended");
    assert_eq!(shell.rows().item_count(), 60);
    assert!(matches!(shell.row(0), Some(Row::Message(m)) if m.id == MsgId(41)));
}

#[test]
fn read_only_and_empty_submissions_send_nothing() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    assert_eq!(shell.submit(CHAT, "  \n", 0, &mut core.admit()), Ok(None));
    assert!(core.take().is_empty());
    shell.apply(Event::DialogsReset(vec![read_only()]));
    assert_eq!(shell.send_target(), Ok(None));
}

#[test]
fn mark_read_and_live_tail_follow_what_is_on_screen() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let at = |last| Seen {
        first_row: 40,
        last_message: Some(last),
    };
    shell.frame_seen(Some(at(45)), &mut core.admit()).unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [96]);
    assert!(viewing(&cmds).is_empty(), "scrolled up: not the tail");
    shell.frame_seen(Some(at(45)), &mut core.admit()).unwrap();
    shell.frame_seen(Some(at(30)), &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "already sent; never backwards");
    shell.frame_seen(Some(at(49)), &mut core.admit()).unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [100]);
    assert_eq!(viewing(&cmds), [Some(CHAT)]);
    shell
        .set_focus(WindowState::Inactive, &mut core.admit())
        .unwrap();
    assert!(matches!(
        core.take().as_slice(),
        [
            Command::Window(WindowState::Inactive),
            Command::ViewingTail(None)
        ]
    ));
    shell
        .set_focus(WindowState::Inactive, &mut core.admit())
        .unwrap();
    assert!(core.take().is_empty());
    shell.apply(live(101));
    shell.frame_seen(Some(at(50)), &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "not while the window is inactive");
    shell
        .set_focus(WindowState::Active, &mut core.admit())
        .unwrap();
    shell.frame_seen(Some(at(50)), &mut core.admit()).unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [101]);
    assert_eq!(viewing(&cmds), [Some(CHAT)]);
    // A refused mark is asked again on the next frame.
    shell.apply(live(102));
    core.full = true;
    assert!(shell.frame_seen(Some(at(51)), &mut core.admit()).is_err());
    core.full = false;
    shell.frame_seen(Some(at(51)), &mut core.admit()).unwrap();
    assert_eq!(reads(&core.take()), [102]);
}

#[test]
fn scrolling_near_the_top_loads_older_once() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let at = |first_row| {
        Some(Seen {
            first_row,
            last_message: None,
        })
    };
    let older = |core: &mut Core| {
        core.take()
            .into_iter()
            .filter(|c| matches!(c, Command::LoadOlder(CHAT)))
            .count()
    };
    shell.frame_seen(at(20), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 0);
    core.full = true;
    assert!(shell.frame_seen(at(2), &mut core.admit()).is_err());
    core.full = false;
    shell.frame_seen(at(2), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 1, "a refused load is asked again");
    shell.frame_seen(at(0), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 0, "in flight");
    shell.apply(history(1, HistoryDir::Older, 1..=50));
    assert_eq!(shell.rows().item_count(), 100);
    shell.frame_seen(at(0), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 1);
    shell.load_failed();
    shell.frame_seen(at(0), &mut core.admit()).unwrap();
    assert_eq!(older(&mut core), 1);
}

/// Finding 2: paging follows `DialogsPaging`, through the shell.
#[test]
fn dialogs_paging_follows_core() {
    let mut core = Core::default();
    let mut shell = ChatShell::default();
    shell.apply(Event::DialogsReset(Vec::new()));
    shell.request_dialogs(0, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "core has not said more exist");
    shell.apply(Event::DialogsPaging { has_more: true });
    core.full = true;
    assert!(shell.request_dialogs(0, &mut core.admit()).is_err());
    core.full = false;
    shell.request_dialogs(0, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::LoadDialogs]));
    shell.request_dialogs(0, &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "in flight");
    shell.apply(Event::DialogsPaging { has_more: true });
    shell.request_dialogs(0, &mut core.admit()).unwrap();
    assert!(
        matches!(core.take().as_slice(), [Command::LoadDialogs]),
        "an all-filtered page leaves the list empty and asks again"
    );
}

#[test]
fn other_chats_events_do_not_touch_the_open_window() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let other = ChatId {
        kind: PeerKind::User,
        id: 2,
    };
    shell.apply(Event::Deleted {
        chat: other,
        ids: vec![MsgId(60)],
    });
    shell.apply(Event::ChatInvalidated(other));
    assert_eq!(shell.row_count(), 50);
    shell.apply(Event::Deleted {
        chat: CHAT,
        ids: vec![MsgId(60)],
    });
    assert_eq!(shell.rows().item_count(), 49);
}

#[test]
fn switching_chats_keeps_echoes_with_their_chat() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    send(&mut shell, &mut core, "x");
    let other = ChatId {
        kind: PeerKind::User,
        id: 2,
    };
    core.take();
    shell.select(other, &mut core.admit()).unwrap();
    assert!(matches!(core.take().as_slice(), [Command::OpenChat(c)] if *c == other));
    assert_eq!(shell.rows().item_count(), 0);
    shell.select(CHAT, &mut core.admit()).unwrap();
    assert_eq!(
        shell.rows().item_count(),
        1,
        "the pending send is still shown"
    );
}

/// The message list with fixed 20 px rows, as a view (a list needs one),
/// each row tracked as the chat pane tracks it.
struct Rows(ListState, viewport::Painted);

impl Render for Rows {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let painted = self.1.clone();
        list(self.0.clone(), move |ix, _, _| {
            painted
                .track(ix, div().h(px(20.)))
                .w_full()
                .into_any_element()
        })
        .size_full()
    }
}

/// Lays the message list out in a 200 px viewport with 20 px rows, the way
/// the chat pane does, and reports what it shows.
fn lay_out(cx: &mut TestAppContext, shell: &ChatShell) -> Option<Seen> {
    let rows = shell.rows().clone();
    let painted = shell.painted().clone();
    let window = cx.add_empty_window();
    let view = window.update(|_, cx| cx.new(|_| Rows(rows, painted)));
    window.draw(point(px(0.), px(0.)), size(px(300.), px(200.)), |_, _| {
        view.into_any_element()
    });
    shell.seen()
}

/// Finding 3: what counts as seen comes from the real list layout. Rows
/// above the viewport are not seen: with echoes filling the screen, the
/// newest message is above it and is neither marked read nor the tail.
#[gpui::test]
fn only_rows_inside_the_viewport_count_as_seen(cx: &mut TestAppContext) {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    for n in 0..15 {
        send(&mut shell, &mut core, &format!("e{n}"));
    }
    core.take();
    let seen = lay_out(cx, &shell).unwrap();
    assert_eq!(seen.first_row, 55, "following the tail: the last 10 rows");
    assert_eq!(seen.last_message, None, "only echoes on screen");
    shell.frame_seen(Some(seen), &mut core.admit()).unwrap();
    assert!(core.take().is_empty(), "message 100 is above the viewport");

    shell.rows().scroll_to(ListOffset {
        item_ix: 40,
        offset_in_item: px(0.),
    });
    let seen = lay_out(cx, &shell).unwrap();
    assert_eq!((seen.first_row, seen.last_message), (40, Some(49)));
    shell.frame_seen(Some(seen), &mut core.admit()).unwrap();
    let cmds = core.take();
    assert_eq!(reads(&cmds), [100]);
    assert_eq!(viewing(&cmds), [Some(CHAT)]);
    assert_eq!(shell.viewing(), Some(CHAT));

    shell.rows().scroll_to(ListOffset {
        item_ix: 20,
        offset_in_item: px(10.),
    });
    let seen = lay_out(cx, &shell).unwrap();
    assert_eq!((seen.first_row, seen.last_message), (20, Some(30)));
    shell.frame_seen(Some(seen), &mut core.admit()).unwrap();
    assert_eq!(viewing(&core.take()), [None], "scrolled away from the tail");
}
