//! The call layer over a scripted invoker: retry classes (paused time),
//! `random_id` stability, peer resolution and request shapes.

use std::collections::VecDeque;
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
use crate::session::AbyssSession;

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
    Calls::new(Arc::clone(mock), session, Arc::from([]))
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
