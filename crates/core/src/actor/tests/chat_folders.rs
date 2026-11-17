//! Chat folders: loaded after the first dialogs page, refetched on change.

use super::harness;
use crate::api::Event;
use crate::backend::Update;
use crate::backend::fake::{FakeScript, Op};
use crate::model::{ChatFolder, FolderRules};

fn folder(id: i32, title: &str) -> ChatFolder {
    ChatFolder {
        id,
        title: title.into(),
        rules: FolderRules::GROUPS,
        pinned: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        shared: false,
    }
}

#[tokio::test(start_paused = true)]
async fn folders_follow_the_first_page_and_every_change() {
    let mut script = FakeScript::demo(3, 1);
    script.folders = vec![folder(2, "Work")];
    let mut h = harness(script, None).await;
    let (ev, skipped) = h.until(|e| matches!(e, Event::ChatFolders(_))).await;
    assert!(matches!(ev, Event::ChatFolders(f) if f == vec![folder(2, "Work")]));
    assert!(
        skipped.iter().any(|e| matches!(e, Event::DialogsReset(_))),
        "after the first page"
    );
    h.fake
        .set_folders(vec![folder(2, "Work"), folder(3, "Home")]);
    h.fake.push_update(Update::ChatFoldersChanged).await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatFolders(_))).await;
    assert!(matches!(ev, Event::ChatFolders(f) if f.len() == 2));
    assert_eq!(h.fake.calls(Op::ChatFolders), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn archiving_applies_once_the_server_accepts() {
    use crate::api::Command;
    use crate::model::PeerFolder;
    let mut h = harness(FakeScript::demo(3, 1), None).await;
    h.online().await;
    h.until(|e| matches!(e, Event::DialogsReset(c) if c.len() == 3))
        .await;
    let chat = super::chat(2);
    h.send(Command::SetPeerFolder {
        chat,
        folder: PeerFolder::Archive,
    })
    .await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatBumped(_))).await;
    assert!(matches!(ev, Event::ChatBumped(s) if s.id == chat && s.folder == PeerFolder::Archive));
    assert_eq!(h.fake.calls(Op::SetPeerFolder), 1);
    let server = h.fake.dialogs();
    assert!(
        server
            .iter()
            .any(|c| c.id == chat && c.folder == PeerFolder::Archive)
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn folder_edits_refetch_and_bad_titles_never_leave() {
    use crate::api::{Command, UserError};
    use crate::model::FolderEdit;
    let mut script = FakeScript::demo(3, 1);
    script.folders = vec![folder(2, "Work")];
    let mut h = harness(script, None).await;
    h.until(|e| matches!(e, Event::ChatFolders(_))).await;
    let chat = super::chat(1);
    h.send(Command::EditChatFolder(FolderEdit::AddChat {
        folder: 2,
        chat,
    }))
    .await;
    let (ev, _) = h.until(|e| matches!(e, Event::ChatFolders(_))).await;
    assert!(matches!(ev, Event::ChatFolders(f) if f[0].include == vec![chat]));
    h.send(Command::EditChatFolder(FolderEdit::Create {
        title: "   ".into(),
        chat,
    }))
    .await;
    h.until(|e| matches!(e, Event::Error(UserError::Rejected)))
        .await;
    assert_eq!(
        h.fake.calls(Op::EditChatFolder),
        1,
        "the blank title was not sent"
    );
    h.send(Command::EditChatFolder(FolderEdit::Delete { folder: 9 }))
        .await;
    h.until(|e| matches!(e, Event::Error(UserError::Rejected)))
        .await;
    h.stop().await;
}
