//! Peer folders: chats moving into and out of the archive live.

use super::{chat, harness};
use crate::api::Event;
use crate::backend::Update;
use crate::backend::fake::FakeScript;
use crate::model::PeerFolder;

/// A live `updateFolderPeers` changes listed chats in place, once, and
/// ignores chats the list does not hold.
#[tokio::test(start_paused = true)]
async fn archive_moves_update_listed_chats_in_place() {
    let mut h = harness(FakeScript::demo(3, 1), None).await;
    h.online().await;
    h.until(|e| matches!(e, Event::DialogsReset(c) if c.len() == 3))
        .await;
    let moves = vec![
        (chat(2), PeerFolder::Archive),
        (chat(99), PeerFolder::Archive),
    ];
    h.fake.push_update(Update::Folders(moves.clone())).await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatBumped(_))).await;
    let Event::ChatBumped(summary) = ev else {
        unreachable!()
    };
    assert_eq!((summary.id, summary.folder), (chat(2), PeerFolder::Archive));
    // The same move again changes nothing; a read marks the end.
    h.fake.push_update(Update::Folders(moves)).await;
    h.fake
        .push_update(Update::ReadInbox {
            chat: chat(1),
            still_unread: 0,
        })
        .await;
    let (ev, skipped) = h.until(|e| matches!(e, Event::ChatBumped(_))).await;
    assert!(matches!(ev, Event::ChatBumped(s) if s.id == chat(1)));
    assert!(!skipped.iter().any(|e| matches!(e, Event::ChatBumped(_))));
    h.fake
        .push_update(Update::Folders(vec![(chat(2), PeerFolder::Main)]))
        .await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatBumped(_))).await;
    assert!(matches!(ev, Event::ChatBumped(s) if s.id == chat(2) && s.folder == PeerFolder::Main));
    h.stop().await;
}
