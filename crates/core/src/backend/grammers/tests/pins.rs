//! The pinned-chat set: bounded, union across pages, priority order.

use crate::backend::MAX_CARRIED_PEERS;
use crate::backend::grammers::pins::{Pins, Recency};
use crate::model::{ChatId, PeerKind};
use crate::store::MAX_DIALOGS;

fn chat(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

#[test]
fn recency_evicts_the_least_recently_touched() {
    let mut r = Recency::new(3);
    assert!(r.touch(chat(1)));
    assert!(r.touch(chat(2)));
    assert!(r.touch(chat(3)));
    assert!(!r.touch(chat(1)), "a re-touch changes no membership");
    assert!(r.touch(chat(4)));
    assert_eq!(r.len(), 3);
    let order: Vec<i64> = r.newest_first().map(|c| c.id).collect();
    assert_eq!(order, vec![4, 1, 3], "2 was the oldest");
}

#[test]
fn pages_accumulate_and_carried_and_used_chats_come_first() {
    let mut pins = Pins::new(&[chat(100)], [chat(50), chat(100)]);
    assert!(pins.list([chat(1), chat(2)].into_iter()));
    assert!(pins.list([chat(3)].into_iter()));
    // A refreshed first page does not drop the second page.
    assert!(!pins.list([chat(1), chat(2)].into_iter()));
    assert!(pins.use_chat(chat(9)));
    assert!(!pins.use_chat(chat(9)));
    let all: Vec<i64> = pins.all().iter().map(|c| c.id).collect();
    assert_eq!(all, vec![100, 9, 1, 2, 3, 50]);
}

#[test]
fn every_part_is_bounded() {
    let carried: Vec<ChatId> = (0..1_000).map(chat).collect();
    let mut pins = Pins::new(&carried, (0..5_000).map(chat));
    let mut id = 10_000;
    for _ in 0..30 {
        let page: Vec<ChatId> = (0..100).map(|i| chat(id + i)).collect();
        id += 100;
        pins.list(page.into_iter());
    }
    for i in 0..1_000 {
        pins.use_chat(chat(50_000 + i));
    }
    let all = pins.all();
    assert_eq!(all.len(), MAX_CARRIED_PEERS * 2 + MAX_DIALOGS);
    // The newest page survived; the oldest pages were evicted.
    assert!(all.contains(&chat(id - 1)));
    assert!(!all.contains(&chat(10_000)));
}
