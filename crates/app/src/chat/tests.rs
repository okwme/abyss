use abyss_core::api::{HistoryDir, SendState};
use abyss_core::model::{Body, ChatKind, PeerKind, ThreadKey};
use abyss_core::rich_text::RichText;
use abyss_core::store::ViewGen;

use super::*;

const CHAT: ChatId = ChatId {
    kind: PeerKind::User,
    id: 1,
};

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

/// A shell with `CHAT` open and messages 51..=100 loaded.
fn open() -> ChatShell {
    let mut shell = ChatShell::default();
    shell.apply(Event::DialogsReset(vec![summary()]));
    assert!(matches!(shell.select(CHAT), Some(Command::OpenChat(CHAT))));
    assert!(shell.select(CHAT).is_none(), "already open");
    shell.apply(history(1, HistoryDir::Tail, 51..=100));
    shell
}

fn send(shell: &mut ChatShell, text: &str) -> LocalId {
    let chat = shell.send_target().unwrap().unwrap();
    match shell.submit(chat, text.into(), 0) {
        Some(Command::SendText { local_id, .. }) => local_id,
        other => panic!("expected SendText, got {other:?}"),
    }
}

fn state_of(shell: &ChatShell, ix: usize) -> Option<SendState> {
    match shell.row(ix)? {
        Row::Echo(e) => Some(e.state),
        Row::Message(_) => None,
    }
}

#[test]
fn echo_lifecycle_sent_then_replaced_by_the_server_copy() {
    let mut shell = open();
    let id = send(&mut shell, "hello");
    assert_eq!(shell.row_count(), 51);
    assert_eq!(shell.rows().item_count(), 51);
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));
    shell.apply(Event::SendState {
        local_id: id,
        state: SendState::Sent { id: MsgId(101) },
    });
    assert_eq!(
        state_of(&shell, 50),
        Some(SendState::Sent { id: MsgId(101) })
    );
    shell.apply(history(1, HistoryDir::Newer, [101]));
    assert_eq!(shell.row_count(), 51, "the message replaced the echo");
    assert_eq!(shell.rows().item_count(), 51);
    assert!(matches!(shell.row(50), Some(Row::Message(m)) if m.id == MsgId(101)));
}

#[test]
fn failed_send_retries_then_is_discarded() {
    let mut shell = open();
    let id = send(&mut shell, "x");
    assert!(shell.discard(id).is_none(), "in flight");
    shell.apply(Event::SendState {
        local_id: id,
        state: SendState::Failed,
    });
    assert!(matches!(shell.retry(id), Some(Command::RetrySend(r)) if r == id));
    assert_eq!(state_of(&shell, 50), Some(SendState::Sending));
    shell.apply(Event::SendState {
        local_id: id,
        state: SendState::Unknown { attempts: 3 },
    });
    assert!(matches!(shell.discard(id), Some(Command::DiscardSend(r)) if r == id));
    assert_eq!(shell.row_count(), 50);
    assert_eq!(shell.rows().item_count(), 50);
}

#[test]
fn unconfirmed_echo_leaves_on_reload() {
    let mut shell = open();
    let id = send(&mut shell, "x");
    shell.apply(Event::SendState {
        local_id: id,
        state: SendState::SentUnconfirmed,
    });
    shell.apply(Event::ChatInvalidated(CHAT));
    assert_eq!(
        shell.rows().item_count(),
        1,
        "only the echo while reloading"
    );
    shell.apply(history(2, HistoryDir::Tail, 60..=101));
    assert_eq!(shell.row_count(), 42, "server history replaces the echo");
    assert_eq!(shell.rows().item_count(), 42);
}

#[test]
fn read_only_and_empty_submissions_send_nothing() {
    let mut shell = open();
    assert!(shell.submit(CHAT, "  \n".into(), 0).is_none());
    shell.apply(Event::DialogsReset(vec![read_only()]));
    assert_eq!(shell.send_target(), Ok(None));
}

#[test]
fn mark_read_follows_the_newest_displayed_message_while_focused() {
    let mut shell = open();
    let seen = |last| Seen {
        first_row: 40,
        last_message: Some(last),
    };
    let reads = |cmds: Vec<Command>| -> Vec<i32> {
        cmds.into_iter()
            .filter_map(|c| match c {
                Command::MarkRead { max_id, .. } => Some(max_id.0),
                _ => None,
            })
            .collect()
    };
    assert_eq!(reads(shell.frame_seen(seen(45))), [96]);
    assert!(reads(shell.frame_seen(seen(45))).is_empty(), "already sent");
    assert!(
        reads(shell.frame_seen(seen(30))).is_empty(),
        "never backwards"
    );
    assert!(matches!(
        shell.set_focus(WindowState::Inactive),
        Some(Command::Window(WindowState::Inactive))
    ));
    assert!(shell.set_focus(WindowState::Inactive).is_none());
    assert!(
        reads(shell.frame_seen(seen(49))).is_empty(),
        "not while unfocused"
    );
    shell.set_focus(WindowState::Active);
    assert_eq!(reads(shell.frame_seen(seen(49))), [100]);
}

#[test]
fn scrolling_near_the_top_loads_older_once() {
    let mut shell = open();
    let at = |first_row| Seen {
        first_row,
        last_message: None,
    };
    assert!(shell.frame_seen(at(20)).is_empty());
    let cmds = shell.frame_seen(at(2));
    assert!(matches!(cmds.as_slice(), [Command::LoadOlder(CHAT)]));
    assert!(shell.frame_seen(at(0)).is_empty(), "in flight");
    shell.apply(history(1, HistoryDir::Older, 1..=50));
    assert_eq!(shell.rows().item_count(), 100);
    assert!(matches!(
        shell.frame_seen(at(0)).as_slice(),
        [Command::LoadOlder(CHAT)]
    ));
    shell.load_failed();
    assert!(matches!(
        shell.frame_seen(at(0)).as_slice(),
        [Command::LoadOlder(CHAT)]
    ));
}

#[test]
fn other_chats_events_do_not_touch_the_open_window() {
    let mut shell = open();
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
    let mut shell = open();
    send(&mut shell, "x");
    let other = ChatId {
        kind: PeerKind::User,
        id: 2,
    };
    assert!(matches!(shell.select(other), Some(Command::OpenChat(c)) if c == other));
    assert_eq!(shell.rows().item_count(), 0);
    shell.select(CHAT);
    assert_eq!(
        shell.rows().item_count(),
        1,
        "the pending send is still shown"
    );
    assert!(matches!(
        shell.jump_to_latest(),
        Some(Command::OpenChat(CHAT))
    ));
}
