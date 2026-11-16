//! The peer map's three states (pinned, held, queued) and their bounds.

use grammers_session::types::{PeerAuth, PeerId, PeerInfo};

use super::{
    MAX_FIFO, MAX_HANDOFF_PEERS, MAX_HANDOFFS, MAX_PINNED_PEERS, MAX_UNPINNED_PEERS, PeerMap,
    Pinned,
};

fn user(id: i64) -> PeerInfo {
    PeerInfo::User {
        id,
        auth: Some(PeerAuth::from_hash(id * 7)),
        bot: None,
        is_self: None,
    }
}

fn pid(id: i64) -> PeerId {
    PeerId::user(id).unwrap()
}

fn pids(ids: impl IntoIterator<Item = i64>) -> Vec<PeerId> {
    ids.into_iter().map(pid).collect()
}

fn known(map: &PeerMap, id: i64) -> bool {
    map.get(pid(id)).is_some()
}

fn full_queue() -> PeerMap {
    let mut map = PeerMap::default();
    for id in 1..=i64::try_from(MAX_UNPINNED_PEERS).unwrap() {
        map.insert(user(id));
    }
    map
}

fn bound() -> usize {
    MAX_PINNED_PEERS + MAX_HANDOFFS * MAX_HANDOFF_PEERS + MAX_UNPINNED_PEERS
}

/// Review round 10 (small variant): an update did not refresh a queued
/// peer's position, so with a full queue, a response naming the oldest peer
/// and one newcomer evicted the peer it had just named.
#[test]
fn an_update_moves_a_queued_peer_to_the_back() {
    let mut map = full_queue();
    map.insert(user(1));
    map.insert(user(5000));
    assert!(known(&map, 1), "just touched");
    assert!(!known(&map, 2), "now the oldest");
    assert!(known(&map, 5000));
    assert_eq!(map.len(), MAX_UNPINNED_PEERS);
}

/// The same two-peer response as a dialogs page: both are held for core.
#[test]
fn a_two_peer_handoff_survives_a_full_queue_and_more() {
    let mut map = full_queue();
    map.cache_response(&pids([1, 5000]), vec![user(1), user(5000)]);
    let aux: Vec<PeerInfo> = (10_000..14_096).map(user).collect();
    map.cache_response(&[], aux);
    assert!(known(&map, 1) && known(&map, 5000));
    assert_eq!(
        map.set_pins(&pids([1, 5000])),
        Pinned {
            pinned: 2,
            unknown: 0
        }
    );
}

/// Review round 10: a 4,096-peer dialogs response evicted its own dialogs
/// (directory order is unordered) before core pinned them.
#[test]
fn a_full_size_response_keeps_its_handoff_until_core_pins_it() {
    let mut map = PeerMap::default();
    let page: Vec<i64> = (1..=101).collect();
    // Dialogs last: the worst order for the old FIFO.
    let infos: Vec<PeerInfo> = (1000..1000 + 3995)
        .chain(page.iter().copied())
        .map(user)
        .collect();
    map.cache_response(&pids(page.iter().copied()), infos);
    assert!(map.len() <= bound());
    assert_eq!(
        map.set_pins(&pids(page.iter().copied())),
        Pinned {
            pinned: 101,
            unknown: 0
        }
    );
    assert!(
        map.handoffs.is_empty() && map.held.is_empty(),
        "pinned ends the hold"
    );
}

/// Two pages arrive (and a huge auxiliary response) before core reconciles
/// either: both handoffs survive. A third releases the oldest into the queue
/// as its newest entries.
#[test]
fn concurrent_handoffs_survive_until_reconciled_and_stay_bounded() {
    let mut map = PeerMap::default();
    map.cache_response(&pids(1..=100), (1..=100).map(user).collect());
    map.cache_response(&pids(101..=200), (101..=200).map(user).collect());
    map.cache_response(&[], (1000..5096).map(user).collect());
    assert_eq!(map.set_pins(&pids(1..=200)).unknown, 0);

    let mut map = PeerMap::default();
    for page in 0..3_i64 {
        let ids = page * 100 + 1..=page * 100 + 100;
        map.cache_response(&pids(ids.clone()), ids.map(user).collect());
    }
    assert_eq!(map.handoffs.len(), MAX_HANDOFFS);
    assert!(!map.held.contains_key(&pid(1)), "the oldest was released");
    assert!(known(&map, 1), "released into the queue, not dropped");
}

/// A handoff id core does not pin stays held; one core pins and then drops
/// goes to the queue.
#[test]
fn a_hold_ends_when_core_pins_the_peer() {
    let mut map = PeerMap::default();
    map.cache_response(&pids([1, 2]), vec![user(1), user(2)]);
    map.set_pins(&pids([1]));
    assert!(!map.held.contains_key(&pid(1)));
    assert!(map.held.contains_key(&pid(2)));
    map.set_pins(&[]);
    assert!(map.slots.get(&pid(1)).unwrap().queued.is_some());
    assert!(
        map.slots.get(&pid(2)).unwrap().queued.is_none(),
        "still held"
    );
}

#[test]
fn a_handoff_is_bounded() {
    let mut map = PeerMap::default();
    map.cache_response(&pids(1..=500), (1..=500).map(user).collect());
    assert_eq!(map.held.len(), MAX_HANDOFF_PEERS);
}

/// Review round 10, finding 2: a peer resolved, then evicted by another
/// response, then pinned is merged back under the same lock.
#[test]
fn merge_and_pin_restores_an_evicted_peer() {
    let mut map = PeerMap::default();
    map.insert(user(7));
    let resolved = map.get(pid(7));
    map.cache_response(&[], (1000..3001).map(user).collect());
    assert!(!known(&map, 7), "evicted between lookup and pin");
    assert_eq!(
        map.merge_and_pin(resolved, &pids([7])),
        Pinned {
            pinned: 1,
            unknown: 0
        }
    );
    assert_eq!(map.pinned_infos(), vec![user(7)]);
}

/// Touching never grows the queue's bookkeeping without bound.
#[test]
fn repeated_touches_keep_the_queue_bounded() {
    let mut map = PeerMap::default();
    for round in 0..10_000 {
        map.insert(user(round % 10 + 1));
    }
    assert!(map.fifo.len() <= MAX_FIFO);
    assert_eq!(map.queued, 10);
    let mut map = full_queue();
    map.set_pins(&pids(1..=1264));
    map.set_pins(&[]);
    assert_eq!(map.queued, MAX_UNPINNED_PEERS);
    assert!(map.len() <= bound());
}
