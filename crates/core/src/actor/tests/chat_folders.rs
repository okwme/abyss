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
