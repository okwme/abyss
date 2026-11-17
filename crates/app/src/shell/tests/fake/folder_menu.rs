//! The archive and folder menu (folders plan, part C) through the real
//! window: each choice sends one command and closes the menu; a new folder
//! takes its title from the field; deleting asks first.

use abyss_core::model::{ChatFolder, FolderRules, PeerFolder};
use gpui::{point, px};

use super::*;

fn chat(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

fn listed(root: &Entity<Root>, cx: &mut VisualTestContext, harness: &Harness) {
    sign_in(root, cx, harness);
    let script = FakeScript::demo(3, 0);
    harness
        .events
        .unbounded_send(Event::DialogsReset(script.dialogs))
        .unwrap();
    let work = ChatFolder {
        id: 2,
        title: "Work".into(),
        rules: FolderRules::default(),
        pinned: Vec::new(),
        include: vec![chat(1)],
        exclude: Vec::new(),
        shared: false,
    };
    harness
        .events
        .unbounded_send(Event::ChatFolders(vec![work]))
        .unwrap();
    cx.run_until_parked();
}

fn sent_since(harness: &Harness, from: usize) -> Vec<String> {
    harness
        .sent
        .borrow()
        .commands
        .iter()
        .skip(from)
        .map(|c| format!("{c:?}"))
        .collect()
}

fn menu_open(root: &Entity<Root>, cx: &mut VisualTestContext) -> bool {
    read(root, cx, |s| s.menu.is_some())
}

#[gpui::test]
fn archiving_sends_one_command_and_closes_the_menu(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness);
    let from = harness.sent.borrow().commands.len();
    root.update_in(cx, |root, window, cx| {
        root.with_session(
            |s, cx| {
                s.open_chat_menu(chat(1), point(px(100.), px(100.)), window, cx);
                s.set_peer_folder(chat(1), PeerFolder::Archive, window, cx);
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert!(!menu_open(&root, cx));
    let sent = sent_since(&harness, from);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].starts_with("SetPeerFolder") && sent[0].contains("Archive"));
    // Nothing changes before core says so.
    assert_eq!(
        read(&root, cx, |s| s.shell.list.get(chat(1)).map(|c| c.folder)),
        Some(PeerFolder::Main)
    );
}

#[gpui::test]
fn a_new_folder_takes_the_typed_title(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness);
    root.update_in(cx, |root, window, cx| {
        root.with_session(
            |s, cx| {
                s.open_chat_menu(chat(1), point(px(100.), px(100.)), window, cx);
                s.start_naming(window, cx);
            },
            cx,
        );
    });
    cx.run_until_parked();
    let from = harness.sent.borrow().commands.len();
    cx.simulate_input("Family");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let sent = sent_since(&harness, from);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].starts_with("EditChatFolder(Create"));
    assert!(!sent[0].contains("Family"), "titles stay out of logs");
    assert!(!menu_open(&root, cx));
}

#[gpui::test]
fn deleting_a_folder_asks_first(cx: &mut TestAppContext) {
    let (root, cx, harness) = open(cx);
    listed(&root, cx, &harness);
    let from = harness.sent.borrow().commands.len();
    let delete = |root: &Entity<Root>, cx: &mut VisualTestContext| {
        root.update_in(cx, |root, window, cx| {
            root.with_session(|s, cx| s.delete_folder(window, cx), cx);
        });
        cx.run_until_parked();
    };
    root.update_in(cx, |root, window, cx| {
        root.with_session(
            |s, cx| s.open_folder_menu(2, point(px(50.), px(60.)), window, cx),
            cx,
        );
    });
    delete(&root, cx);
    assert!(
        sent_since(&harness, from).is_empty(),
        "the first click asks"
    );
    assert!(menu_open(&root, cx));
    delete(&root, cx);
    let sent = sent_since(&harness, from);
    assert_eq!(sent, ["EditChatFolder(Delete { folder: 2 })"]);
    assert!(!menu_open(&root, cx));
}
