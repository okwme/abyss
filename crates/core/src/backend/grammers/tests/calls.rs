//! The call layer over a scripted invoker: retry classes (paused time),
//! `random_id` stability, peer resolution and request shapes.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grammers_client::InvocationError;
use grammers_client::sender::{ReadError, RpcError, SenderPool};
use grammers_client::session::Session as _;
use grammers_client::session::types::{PeerAuth, PeerInfo};
use grammers_client::tl::{self, Deserializable as _, Serializable as _};

use super::{dialog, message, peer_user, user, users};
use crate::backend::grammers::calls::{Calls, history_offsets};
use crate::backend::grammers::invoke::Invoke;
use crate::backend::grammers::{GrammersControl, connection_params};
use crate::backend::{BackendError, HistoryQuery, InstanceControl, SessionSnapshot};
use crate::model::{ChatId, MsgId, PeerKind};
use crate::session::{AbyssSession, MAX_PINNED_PEERS};

type Reply = Result<Vec<u8>, InvocationError>;

/// Answers each request with the next scripted reply and records the request
/// bytes. An exhausted script answers `Dropped`.
#[derive(Default)]
struct Mock {
    script: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Vec<u8>>>,
}

impl Mock {
    fn with(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Mock {
            script: Mutex::new(replies.into()),
            requests: Mutex::default(),
        })
    }

    fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Invoke for Arc<Mock> {
    fn invoke<R>(
        &self,
        request: &R,
    ) -> impl Future<Output = Result<R::Return, InvocationError>> + Send
    where
        R: tl::RemoteCall + Send + Sync,
        R::Return: Send,
    {
        self.requests.lock().unwrap().push(request.to_bytes());
        let reply = self.script.lock().unwrap().pop_front();
        async move {
            match reply {
                Some(Ok(bytes)) => R::Return::from_bytes(&bytes)
                    .map_err(|e| InvocationError::from(ReadError::from(e))),
                Some(Err(e)) => Err(e),
                None => Err(InvocationError::Dropped("script exhausted")),
            }
        }
    }
}

fn flood(seconds: u32) -> Reply {
    Err(InvocationError::Rpc(RpcError {
        code: 420,
        name: "FLOOD_WAIT".into(),
        value: Some(seconds),
        caused_by: None,
    }))
}

fn rpc(code: i32, name: &str) -> Reply {
    Err(InvocationError::Rpc(RpcError {
        code,
        name: name.into(),
        value: None,
        caused_by: None,
    }))
}

fn io() -> Reply {
    Err(InvocationError::Io(std::io::Error::other("reset")))
}

fn history_reply(ids: &[i32]) -> Vec<u8> {
    tl::enums::messages::Messages::Messages(tl::types::messages::Messages {
        messages: ids
            .iter()
            .map(|&id| tl::enums::Message::Message(message(peer_user(7), id, "m")))
            .collect(),
        topics: vec![],
        chats: vec![],
        users: vec![],
    })
    .to_bytes()
}

fn short_sent(id: i32) -> Vec<u8> {
    tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
        out: true,
        id,
        pts: 1,
        pts_count: 1,
        date: 0,
        media: None,
        entities: None,
        ttl_period: None,
    })
    .to_bytes()
}

const ANN: ChatId = ChatId {
    kind: PeerKind::User,
    id: 7,
};
const GROUP: ChatId = ChatId {
    kind: PeerKind::Channel,
    id: 10,
};

fn session() -> Arc<AbyssSession> {
    Arc::new(AbyssSession::from_snapshot(SessionSnapshot::detached()).unwrap())
}

/// A session that knows Ann's and the group's access hashes.
async fn setup(mock: &Arc<Mock>) -> Calls<Arc<Mock>> {
    let session = session();
    session
        .cache_peer(PeerInfo::User {
            id: 7,
            auth: Some(PeerAuth::from_hash(49)),
            bot: Some(false),
            is_self: Some(false),
        })
        .await
        .unwrap();
    session
        .cache_peer(PeerInfo::Channel {
            id: 10,
            auth: Some(PeerAuth::from_hash(110)),
            kind: None,
        })
        .await
        .unwrap();
    Calls::new(Arc::clone(mock), session)
}

#[tokio::test(start_paused = true)]
async fn read_class_sleeps_short_floods_at_most_twice() {
    let mock = Mock::with(vec![flood(3), flood(3), Ok(history_reply(&[1, 2]))]);
    let calls = setup(&mock).await;
    let start = tokio::time::Instant::now();
    let page = calls.history(ANN, HistoryQuery::Tail, 10).await.unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(start.elapsed(), Duration::from_secs(6));

    let mock = Mock::with(vec![flood(2), flood(2), flood(2), Ok(history_reply(&[1]))]);
    let calls = setup(&mock).await;
    let err = calls
        .history(ANN, HistoryQuery::Tail, 10)
        .await
        .unwrap_err();
    assert_eq!(err, BackendError::FloodWait { seconds: 2 });
    assert_eq!(mock.requests().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn read_class_does_not_sleep_long_floods() {
    let mock = Mock::with(vec![flood(6), Ok(history_reply(&[1]))]);
    let calls = setup(&mock).await;
    let start = tokio::time::Instant::now();
    let err = calls
        .history(ANN, HistoryQuery::Tail, 10)
        .await
        .unwrap_err();
    assert_eq!(err, BackendError::FloodWait { seconds: 6 });
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn read_class_retries_io_once() {
    let mock = Mock::with(vec![io(), Ok(history_reply(&[1]))]);
    assert!(
        setup(&mock)
            .await
            .history(ANN, HistoryQuery::Tail, 10)
            .await
            .is_ok()
    );
    assert_eq!(mock.requests().len(), 2);

    let mock = Mock::with(vec![io(), io(), Ok(history_reply(&[1]))]);
    let err = setup(&mock)
        .await
        .history(ANN, HistoryQuery::Tail, 10)
        .await
        .unwrap_err();
    assert_eq!(err, BackendError::Transient);
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn sends_are_never_retried_by_the_adapter_and_keep_their_random_id() {
    let mock = Mock::with(vec![io(), flood(1), Ok(short_sent(99))]);
    let calls = setup(&mock).await;
    let err = calls.send_text(ANN, "hi".into(), 42).await.unwrap_err();
    assert_eq!(
        err,
        BackendError::Transient,
        "unknown outcome: core retries"
    );
    assert_eq!(mock.requests().len(), 1);
    let err = calls.send_text(ANN, "hi".into(), 42).await.unwrap_err();
    assert_eq!(err, BackendError::FloodWait { seconds: 1 });
    assert_eq!(mock.requests().len(), 2);
    // Core's retry of the same logical send.
    assert_eq!(calls.send_text(ANN, "hi".into(), 42).await, Ok(MsgId(99)));

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| *r == requests[0]), "byte-identical");
    let sent = tl::functions::messages::SendMessage::from_boxed_bytes(&requests[0]).unwrap();
    assert_eq!(sent.random_id, 42);
    assert!(sent.no_webpage);
    assert_eq!(sent.message, "hi");
    assert!(matches!(
        sent.peer,
        tl::enums::InputPeer::User(p) if p.user_id == 7 && p.access_hash == 49
    ));
}

#[tokio::test]
async fn accepted_sends_without_an_id_are_unconfirmed() {
    let no_mapping = Ok(tl::enums::Updates::Updates(tl::types::Updates {
        updates: vec![],
        users: vec![],
        chats: vec![],
        date: 0,
        seq: 0,
    })
    .to_bytes());
    let mock = Mock::with(vec![no_mapping, rpc(400, "RANDOM_ID_DUPLICATE")]);
    let calls = setup(&mock).await;
    for _ in 0..2 {
        let err = calls.send_text(ANN, "hi".into(), 7).await.unwrap_err();
        assert_eq!(err, BackendError::SentUnconfirmed);
    }
}

#[tokio::test]
async fn unknown_peers_are_rejected_without_a_request() {
    let mock = Mock::with(vec![]);
    let calls = setup(&mock).await;
    let stranger = ChatId {
        kind: PeerKind::User,
        id: 8,
    };
    assert_eq!(
        calls.send_text(stranger, "x".into(), 1).await,
        Err(BackendError::Rejected)
    );
    assert_eq!(
        calls
            .history(stranger, HistoryQuery::Tail, 1)
            .await
            .unwrap_err(),
        BackendError::Rejected
    );
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn read_history_sends_the_displayed_max_id() {
    let affected = Ok(tl::enums::messages::AffectedMessages::Messages(
        tl::types::messages::AffectedMessages {
            pts: 1,
            pts_count: 1,
        },
    )
    .to_bytes());
    let mock = Mock::with(vec![affected, Ok(true.to_bytes())]);
    let calls = setup(&mock).await;
    calls.read_history(ANN, MsgId(41)).await.unwrap();
    calls.read_history(GROUP, MsgId(42)).await.unwrap();
    let requests = mock.requests();
    let user = tl::functions::messages::ReadHistory::from_boxed_bytes(&requests[0]).unwrap();
    assert_eq!(user.max_id, 41);
    let channel = tl::functions::channels::ReadHistory::from_boxed_bytes(&requests[1]).unwrap();
    assert_eq!(channel.max_id, 42);
    assert!(matches!(
        channel.channel,
        tl::enums::InputChannel::Channel(c) if c.channel_id == 10 && c.access_hash == 110
    ));
}

#[test]
fn history_offsets_per_query() {
    assert_eq!(history_offsets(HistoryQuery::Tail, 50), (0, 0, 0));
    assert_eq!(
        history_offsets(HistoryQuery::Before(MsgId(10)), 50),
        (10, 0, 0)
    );
    assert_eq!(
        history_offsets(HistoryQuery::After(MsgId(10)), 50),
        (10, -50, 10)
    );
}

#[tokio::test]
async fn dialogs_teach_the_session_hashes_for_later_calls() {
    let dialogs = tl::enums::messages::Dialogs::Dialogs(tl::types::messages::Dialogs {
        dialogs: vec![tl::enums::Dialog::Dialog(dialog(peer_user(8), 1))],
        messages: vec![tl::enums::Message::Message(message(peer_user(8), 1, "x"))],
        chats: vec![],
        users: users(vec![user(8, "Bob", "")]),
    });
    let mock = Mock::with(vec![Ok(dialogs.to_bytes()), Ok(history_reply(&[]))]);
    let calls = setup(&mock).await;
    let page = calls.dialogs(None, 20).await.unwrap();
    assert_eq!(page.chats.len(), 1);
    assert_eq!(page.next, None);
    let bob = ChatId {
        kind: PeerKind::User,
        id: 8,
    };
    calls.history(bob, HistoryQuery::Tail, 20).await.unwrap();
    let requests = mock.requests();
    let first = tl::functions::messages::GetDialogs::from_boxed_bytes(&requests[0]).unwrap();
    assert_eq!((first.limit, first.offset_id), (20, 0));
    let history = tl::functions::messages::GetHistory::from_boxed_bytes(&requests[1]).unwrap();
    assert!(matches!(
        history.peer,
        tl::enums::InputPeer::User(p) if p.user_id == 8 && p.access_hash == 56
    ));
}

fn uid(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

/// A full dialogs slice listing users `ids` (top message id = user id).
fn dialogs_slice(ids: &[i64]) -> Vec<u8> {
    let top = |id: i64| i32::try_from(id).unwrap();
    tl::enums::messages::Dialogs::Slice(tl::types::messages::DialogsSlice {
        count: 1_000,
        dialogs: ids
            .iter()
            .map(|&id| tl::enums::Dialog::Dialog(dialog(peer_user(id), top(id))))
            .collect(),
        messages: ids
            .iter()
            .map(|&id| tl::enums::Message::Message(message(peer_user(id), top(id), "m")))
            .collect(),
        chats: vec![],
        users: users(ids.iter().map(|&id| user(id, "U", "")).collect()),
    })
    .to_bytes()
}

fn retire(session: &Arc<AbyssSession>) -> SessionSnapshot {
    let pool = SenderPool::with_configuration(Arc::clone(session), 1, connection_params());
    GrammersControl {
        handle: pool.handle.thin,
        session: Arc::clone(session),
    }
    .retire()
}

/// The peer a rebuilt instance addresses `chat` with, if it can.
async fn rebuilt_peer(snapshot: SessionSnapshot, chat: ChatId) -> Option<i64> {
    let session = Arc::new(AbyssSession::from_snapshot(snapshot).unwrap());
    let mock = Mock::with(vec![Ok(history_reply(&[]))]);
    let calls = Calls::new(Arc::clone(&mock), session);
    calls.history(chat, HistoryQuery::Tail, 5).await.ok()?;
    let request = mock.requests().pop()?;
    match tl::functions::messages::GetHistory::from_boxed_bytes(&request)
        .unwrap()
        .peer
    {
        tl::enums::InputPeer::User(p) if p.user_id == chat.id => Some(p.access_hash),
        _ => None,
    }
}

#[tokio::test]
async fn a_refreshed_first_page_keeps_later_pages_and_open_chats_across_a_rebuild() {
    let mock = Mock::with(vec![
        Ok(dialogs_slice(&[21, 22])),
        Ok(dialogs_slice(&[23, 24])),
        Ok(history_reply(&[])),
        // The refresh: page one again, with a newcomer on top.
        Ok(dialogs_slice(&[25, 21])),
    ]);
    let calls = setup(&mock).await;
    let first = calls.dialogs(None, 2).await.unwrap();
    let second = calls.dialogs(first.next, 2).await.unwrap();
    assert_eq!(second.chats.len(), 2);
    // Open a chat from page two, then refresh page one.
    calls.history(uid(24), HistoryQuery::Tail, 5).await.unwrap();
    calls.dialogs(None, 2).await.unwrap();

    // Core's required set: the open chat, then its merged list.
    calls.retain(&[uid(24), uid(25), uid(21), uid(22), uid(23)]);

    let session = Arc::clone(calls.session());
    let snapshot = retire(&session);
    for (id, hash) in [(24, 24 * 7), (23, 23 * 7), (22, 22 * 7), (25, 25 * 7)] {
        assert_eq!(
            rebuilt_peer(snapshot.clone(), uid(id)).await,
            Some(hash),
            "chat {id} lost its access hash"
        );
    }
}

/// The user ids a snapshot carries pinned.
fn carried_users(snapshot: &SessionSnapshot) -> HashSet<i64> {
    snapshot
        .carried
        .iter()
        .filter_map(|info| match info {
            PeerInfo::User { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

fn uids(ids: impl IntoIterator<Item = i64>) -> Vec<ChatId> {
    ids.into_iter().map(uid).collect()
}

/// Calls over a session that knows users `ids` (hash = 7 × id), unpinned.
async fn knowing(ids: impl IntoIterator<Item = i64>, replies: Vec<Reply>) -> Calls<Arc<Mock>> {
    Calls::new(Mock::with(replies), knowing_session(ids).await)
}

async fn knowing_session(ids: impl IntoIterator<Item = i64>) -> Arc<AbyssSession> {
    let session = session();
    for id in ids {
        session
            .cache_peer(PeerInfo::User {
                id,
                auth: Some(PeerAuth::from_hash(id * 7)),
                bot: Some(false),
                is_self: Some(false),
            })
            .await
            .unwrap();
    }
    session
}

/// An invoker whose requests never complete.
struct Hang;

impl Invoke for Hang {
    fn invoke<R>(
        &self,
        _request: &R,
    ) -> impl Future<Output = Result<R::Return, InvocationError>> + Send
    where
        R: tl::RemoteCall + Send + Sync,
        R::Return: Send,
    {
        std::future::pending()
    }
}

/// Review round 7: at the 1,000-dialog boundary, a refresh with one newcomer
/// made the page-recency tracker evict chat 50, which core keeps (core drops
/// chat 1000 instead). The pins now follow core's list exactly.
#[tokio::test]
async fn capacity_boundary_refresh_keeps_what_core_keeps_across_a_rebuild() {
    let mut replies: Vec<Reply> = (0..20_i64)
        .map(|p| {
            Ok(dialogs_slice(
                &(p * 50 + 1..=p * 50 + 50).collect::<Vec<_>>(),
            ))
        })
        .collect();
    let mut refresh: Vec<i64> = vec![1001];
    refresh.extend(1..=49);
    replies.push(Ok(dialogs_slice(&refresh)));
    let calls = knowing([], replies).await;
    let mut cursor = None;
    for _ in 0..20 {
        cursor = calls.dialogs(cursor, 50).await.unwrap().next;
    }
    calls.retain(&uids(1..=1000));
    calls.dialogs(None, 50).await.unwrap();
    // `store::Dialogs::merge_first_page`: the page heads the list, the
    // loaded chats below it are kept, and the list stays at 1,000.
    calls.retain(&uids(std::iter::once(1001).chain(1..=999)));

    let snapshot = retire(calls.session());
    let carried = carried_users(&snapshot);
    assert!(carried.contains(&50) && carried.contains(&1001));
    assert!(!carried.contains(&1000), "core dropped it");
    assert_eq!(carried.len(), 1000);
    assert_eq!(rebuilt_peer(snapshot, uid(50)).await, Some(50 * 7));
}

/// Review round 7: a rebuild funnelled up to 1,264 inherited peers into the
/// 1,000-slot dialog set, so the next repin lost 264 of them.
#[tokio::test]
async fn consecutive_rebuilds_keep_more_than_1000_inherited_peers() {
    let all: Vec<ChatId> = uids(1..=1264);
    let calls = knowing(1..=1264, vec![]).await;
    calls.retain(&all);
    let mut snapshot = retire(calls.session());
    for round in 0..3 {
        let session = Arc::new(AbyssSession::from_snapshot(snapshot).unwrap());
        let calls = Calls::new(Mock::with(vec![Ok(history_reply(&[]))]), session);
        // A call before core reconciles repins (the chat is already pinned).
        calls
            .history(uid(1264), HistoryQuery::Tail, 5)
            .await
            .unwrap();
        if round == 2 {
            calls.retain(&all);
        }
        snapshot = retire(calls.session());
        assert_eq!(carried_users(&snapshot).len(), 1264, "round {round}");
    }
}

#[tokio::test]
async fn the_pinned_set_never_exceeds_the_session_cap() {
    let calls = knowing(1..=2000, vec![Ok(history_reply(&[]))]).await;
    calls.retain(&uids(1..=2000));
    let (selected, unknown) = calls.pin_state();
    assert_eq!((selected.len(), unknown), (MAX_PINNED_PEERS, 0));
    assert_eq!(
        carried_users(&retire(calls.session())).len(),
        MAX_PINNED_PEERS
    );
}

/// Review round 7: when only the priority of already-tracked chats changed,
/// no repin ran, so the truncated union kept a stale membership.
#[tokio::test]
async fn a_priority_change_at_capacity_repins() {
    let calls = knowing(1..=1300, vec![]).await;
    calls.retain(&uids(1..=1264));
    // Chat 1300 becomes an open window: core lists it first, which pushes
    // the oldest dialog (1264) out of the cap.
    calls.retain(&uids(std::iter::once(1300).chain(1..=1264)));
    let carried = carried_users(&retire(calls.session()));
    assert!(carried.contains(&1300));
    assert!(!carried.contains(&1264));
    assert_eq!(carried.len(), MAX_PINNED_PEERS);
}

#[tokio::test(start_paused = true)]
async fn in_flight_chats_stay_pinned_until_the_call_ends() {
    let session = knowing_session([7, 8, 9]).await;
    let calls = Calls::new(Hang, Arc::clone(&session));
    calls.retain(&[uid(7)]);
    let mut call = Box::pin(calls.history(uid(9), HistoryQuery::Tail, 5));
    let waited = tokio::time::timeout(Duration::from_secs(1), call.as_mut()).await;
    assert!(waited.is_err(), "the call is still in flight");
    // Core has not listed 9 yet; a rebuild now must still carry it.
    assert_eq!(carried_users(&session.snapshot()), HashSet::from([7, 9]));
    drop(call);
    // Cancelled: the extra pin is released; core's set remains.
    assert_eq!(carried_users(&session.snapshot()), HashSet::from([7]));
}

#[tokio::test]
async fn listed_peers_without_a_hash_are_counted_then_pinned_once_learned() {
    let calls = knowing([7], vec![Ok(dialogs_slice(&[8]))]).await;
    calls.retain(&[uid(7), uid(8)]);
    assert_eq!(calls.pin_state().1, 1, "8 is unknown");
    assert_eq!(
        carried_users(&calls.session().snapshot()),
        HashSet::from([7])
    );
    calls.dialogs(None, 20).await.unwrap();
    assert_eq!(calls.pin_state().1, 0);
    assert_eq!(
        carried_users(&calls.session().snapshot()),
        HashSet::from([7, 8])
    );
}

#[tokio::test]
async fn later_dialog_pages_exclude_pinned_chats() {
    let mut pinned = dialog(peer_user(21), 21);
    pinned.pinned = true;
    let page_one = tl::enums::messages::Dialogs::Slice(tl::types::messages::DialogsSlice {
        count: 1_000,
        dialogs: vec![
            tl::enums::Dialog::Dialog(pinned),
            tl::enums::Dialog::Dialog(dialog(peer_user(22), 22)),
        ],
        messages: vec![
            tl::enums::Message::Message(message(peer_user(21), 21, "p")),
            tl::enums::Message::Message(message(peer_user(22), 22, "q")),
        ],
        chats: vec![],
        users: users(vec![user(21, "P", ""), user(22, "Q", "")]),
    });
    let mock = Mock::with(vec![Ok(page_one.to_bytes()), Ok(dialogs_slice(&[23, 24]))]);
    let calls = setup(&mock).await;
    let first = calls.dialogs(None, 2).await.unwrap();
    assert!(first.chats[0].pinned);
    let next = first.next.expect("a full page continues");
    let second = calls.dialogs(Some(next), 2).await.unwrap();
    assert!(second.chats.iter().all(|c| !c.pinned && c.id != uid(21)));

    let requests = mock.requests();
    let one = tl::functions::messages::GetDialogs::from_boxed_bytes(&requests[0]).unwrap();
    assert!(
        !one.exclude_pinned,
        "the first page carries the pinned chats"
    );
    let two = tl::functions::messages::GetDialogs::from_boxed_bytes(&requests[1]).unwrap();
    assert!(two.exclude_pinned, "later pages must not repeat them");
    assert_eq!((two.offset_id, two.offset_date), (22, 1_700_000_022));
    assert!(matches!(
        two.offset_peer,
        tl::enums::InputPeer::User(p) if p.user_id == 22 && p.access_hash == 22 * 7
    ));
}

#[tokio::test]
async fn oversized_responses_are_backend_errors() {
    let many: Vec<i32> = (1..=200).collect();
    let mock = Mock::with(vec![Ok(history_reply(&many))]);
    let calls = setup(&mock).await;
    assert_eq!(
        calls
            .history(ANN, HistoryQuery::Tail, 10)
            .await
            .unwrap_err(),
        BackendError::Rejected
    );
}

#[tokio::test]
async fn retire_revokes_before_snapshotting() {
    let session = session();
    let pool = SenderPool::with_configuration(Arc::clone(&session), 1, connection_params());
    let control = GrammersControl {
        handle: pool.handle.thin.clone(),
        session: Arc::clone(&session),
    };
    session.set_home_dc_id(5).await.unwrap();
    let snapshot = control.retire();
    let rev = snapshot.rev().unwrap();
    assert_eq!(rev, 1);
    // A late write by the retired instance (e.g. its runner) is dropped.
    session.set_home_dc_id(4).await.unwrap();
    assert_eq!(snapshot.rev().unwrap(), rev);
    assert_eq!(session.home_dc_id().unwrap(), 5);
}

/// Collects formatted log output.
#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn logs_never_carry_text_phones_or_payloads() {
    let buf = LogBuf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut bob = user(8, "Secret Name", "");
    bob.phone = Some("+447700900123".into());
    let dialogs = tl::enums::messages::Dialogs::Slice(tl::types::messages::DialogsSlice {
        count: 5,
        dialogs: vec![tl::enums::Dialog::Dialog(dialog(peer_user(8), 1))],
        messages: vec![tl::enums::Message::Message(message(
            peer_user(8),
            1,
            "secret body",
        ))],
        chats: vec![],
        users: users(vec![bob]),
    });
    let mock = Mock::with(vec![
        Err(InvocationError::Io(std::io::Error::other(
            "secret io detail",
        ))),
        Ok(dialogs.to_bytes()),
        rpc(400, "+447700900123 secret"),
        io(),
    ]);
    let calls = setup(&mock).await;
    calls.dialogs(None, 1).await.unwrap();
    let _ = calls.send_text(ANN, "secret outgoing".into(), 1).await;
    let _ = calls.send_text(ANN, "secret outgoing".into(), 1).await;

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        out.contains("telegram call failed"),
        "events are logged: {out}"
    );
    for needle in ["secret", "Secret", "+44", "7700900123"] {
        assert!(!out.contains(needle), "{needle:?} leaked into: {out}");
    }
}
