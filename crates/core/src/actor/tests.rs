//! Actor tests against the fake backend. Paused time unless the disk cache is
//! involved (its worker is an OS thread, which paused time cannot see).

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test harness helpers are not #[test] items, so allow-*-in-tests does not cover them"
)]

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{Actor, CALL_DEADLINE, Internal, MAX_CONCURRENT};
use crate::api::{Command, Event, HistoryDir, SendState, UserError};
use crate::backend::fake::{FakeBackend, FakeHandle, FakeScript, Fault, Op, text_message};
use crate::backend::{BackendError, Update};
use crate::cache::{CacheHandle, CacheKey};
use crate::events::{self, EventReceiver};
use crate::model::{ChatId, LocalId, MsgId, PeerKind};

const WAIT: Duration = Duration::from_secs(120);

struct Harness {
    cmd: mpsc::Sender<Command>,
    internal: mpsc::Sender<Internal>,
    events: EventReceiver,
    fake: FakeHandle,
    cancel: CancellationToken,
    join: JoinHandle<()>,
}

fn chat(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

fn harness(script: FakeScript, cache: Option<CacheHandle>) -> Harness {
    let (backend, fake) = FakeBackend::new(script);
    let (cmd, cmd_rx) = mpsc::channel(256);
    let (internal, internal_rx) = mpsc::channel(8);
    let (events_tx, events) = events::channel();
    let (actor, channels) = Actor::new(Arc::new(backend), events_tx, cache, cmd_rx, internal_rx);
    let cancel = CancellationToken::new();
    let join = tokio::spawn(actor.run(channels, cancel.clone()));
    Harness {
        cmd,
        internal,
        events,
        fake,
        cancel,
        join,
    }
}

impl Harness {
    async fn send(&self, c: Command) {
        self.cmd.send(c).await.unwrap();
    }

    /// Waits for an event matching `pred`, returning it and everything skipped.
    async fn until(&mut self, pred: impl Fn(&Event) -> bool) -> (Event, Vec<Event>) {
        let mut skipped = Vec::new();
        let found = tokio::time::timeout(WAIT, async {
            loop {
                let ev = self.events.recv().await.expect("actor stopped");
                if pred(&ev) {
                    return ev;
                }
                skipped.push(ev);
            }
        })
        .await
        .expect("timed out waiting for event");
        (found, skipped)
    }

    async fn history(&mut self, c: ChatId, d: HistoryDir) -> Event {
        self.until(|e| matches!(e, Event::History { chat, dir, .. } if *chat == c && *dir == d))
            .await
            .0
    }

    async fn stop(self) {
        self.cancel.cancel();
        self.join.await.unwrap();
    }
}

fn history_ids(ev: &Event) -> Vec<i32> {
    match ev {
        Event::History { messages, .. } => messages.iter().map(|m| m.id.0).collect(),
        _ => Vec::new(),
    }
}

#[tokio::test(start_paused = true)]
async fn loads_dialogs_and_tail() {
    let mut h = harness(FakeScript::demo(3, 120), None);
    let (ev, _) = h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    assert!(matches!(ev, Event::DialogsReset(v) if v.len() == 3));
    h.send(Command::OpenChat(chat(1))).await;
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    assert_eq!(history_ids(&ev), (71..=120).collect::<Vec<_>>());
    assert!(matches!(ev, Event::History { has_more: true, .. }));
    h.send(Command::LoadOlder(chat(1))).await;
    let ev = h.history(chat(1), HistoryDir::Older).await;
    assert_eq!(history_ids(&ev), (21..=70).collect::<Vec<_>>());
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn admission_caps_in_flight_and_parks_timeouts_until_retire() {
    let mut h = harness(FakeScript::default(), None);
    let total = MAX_CONCURRENT + 4;
    for _ in 0..total {
        h.fake.inject(Op::Read, Fault::Hang);
    }
    for i in 0..total {
        let id = i64::try_from(i).unwrap() + 1;
        h.send(Command::MarkRead {
            chat: chat(id),
            max_id: MsgId(1),
        })
        .await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        h.fake.calls(Op::Read),
        MAX_CONCURRENT,
        "17th call must wait for a permit"
    );
    assert_eq!(h.fake.max_in_flight(), MAX_CONCURRENT);
    // Every call times out; the permits are parked, so nothing else is admitted.
    tokio::time::sleep(CALL_DEADLINE * 3).await;
    assert_eq!(h.fake.in_flight(), 0);
    assert_eq!(
        h.fake.calls(Op::Read),
        MAX_CONCURRENT,
        "timed-out permits must stay parked"
    );
    h.internal.send(Internal::Retire).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        h.fake.calls(Op::Read),
        total,
        "retire recycles parked permits"
    );
    let _ = &mut h.events;
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn stale_completion_after_close_is_dropped() {
    let mut script = FakeScript::demo(3, 10);
    script.latency = Duration::from_secs(1);
    let mut h = harness(script, None);
    h.send(Command::OpenChat(chat(1))).await;
    h.send(Command::CloseChat(chat(1))).await;
    h.send(Command::OpenChat(chat(2))).await;
    let (_, skipped) = h
        .until(|e| matches!(e, Event::History { chat: c, .. } if *c == chat(2)))
        .await;
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::History { chat: c, .. } if *c == chat(1)))
    );
    assert_eq!(h.fake.calls(Op::History), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn client_generation_bump_drops_completions() {
    let mut script = FakeScript::demo(3, 10);
    script.latency = Duration::from_secs(1);
    let mut h = harness(script, None);
    h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    h.send(Command::OpenChat(chat(1))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.internal.send(Internal::Retire).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    while let Some(ev) = h.events.try_recv() {
        assert!(
            !matches!(ev, Event::History { .. }),
            "stale generation completion leaked: {ev:?}"
        );
    }
    assert_eq!(h.fake.calls(Op::History), 1);
    h.send(Command::OpenChat(chat(1))).await;
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    assert_eq!(history_ids(&ev).len(), 10);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn panic_in_backend_is_contained() {
    let mut h = harness(FakeScript::demo(2, 10), None);
    h.fake.inject(Op::History, Fault::Panic);
    h.send(Command::OpenChat(chat(1))).await;
    h.until(|e| matches!(e, Event::Error(UserError::Internal)))
        .await;
    h.send(Command::OpenChat(chat(1))).await;
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    assert_eq!(history_ids(&ev).len(), 10);
    h.stop().await;
}

fn send_state(id: LocalId) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::SendState { local_id, state } if *local_id == id && *state != SendState::Sending)
}

#[tokio::test(start_paused = true)]
async fn send_retries_transient_with_same_random_id() {
    let mut h = harness(FakeScript::demo(1, 1), None);
    for _ in 0..2 {
        h.fake
            .inject(Op::Send, Fault::Error(BackendError::Transient));
    }
    h.send(Command::SendText {
        chat: chat(1),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    let (ev, _) = h.until(send_state(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { id: MsgId(2) },
            ..
        }
    ));
    let sent = h.fake.sent();
    assert_eq!(sent.len(), 1, "only the successful attempt is recorded");
    assert_eq!(h.fake.calls(Op::Send), 3);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn send_unknown_after_retries_then_manual_retry() {
    let mut h = harness(FakeScript::demo(1, 1), None);
    for _ in 0..3 {
        h.fake.inject(Op::Send, Fault::Hang);
    }
    h.send(Command::SendText {
        chat: chat(1),
        text: "hi".into(),
        local_id: LocalId(7),
    })
    .await;
    let (ev, _) = h.until(send_state(LocalId(7))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Unknown { attempts: 3 },
            ..
        }
    ));
    h.send(Command::RetrySend(LocalId(7))).await;
    let (ev, _) = h.until(send_state(LocalId(7))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Sent { .. },
            ..
        }
    ));
    h.send(Command::RetrySend(LocalId(7))).await;
    h.until(|e| matches!(e, Event::Error(UserError::NotFound)))
        .await;
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn rejected_send_fails_and_empty_text_is_refused() {
    let mut h = harness(FakeScript::demo(1, 1), None);
    h.fake
        .inject(Op::Send, Fault::Error(BackendError::Rejected));
    h.send(Command::SendText {
        chat: chat(1),
        text: "hi".into(),
        local_id: LocalId(1),
    })
    .await;
    let (ev, _) = h.until(send_state(LocalId(1))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Failed,
            ..
        }
    ));
    h.send(Command::SendText {
        chat: chat(1),
        text: "  ".into(),
        local_id: LocalId(2),
    })
    .await;
    let (ev, _) = h.until(send_state(LocalId(2))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Failed,
            ..
        }
    ));
    assert_eq!(h.fake.calls(Op::Send), 1);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn live_messages_extend_open_window_and_bump_others() {
    let mut h = harness(FakeScript::demo(2, 10), None);
    h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    h.send(Command::OpenChat(chat(1))).await;
    h.history(chat(1), HistoryDir::Tail).await;
    h.fake
        .push_update(Update::NewMessage(text_message(chat(1), 11, "new")))
        .await;
    let ev = h.history(chat(1), HistoryDir::Newer).await;
    assert_eq!(history_ids(&ev), vec![11]);
    h.fake
        .push_update(Update::NewMessage(text_message(chat(2), 11, "other")))
        .await;
    let (ev, _) = h
        .until(|e| matches!(e, Event::ChatBumped(s) if s.id == chat(2)))
        .await;
    assert!(matches!(ev, Event::ChatBumped(s) if s.unread == 1));
    h.fake
        .push_update(Update::Deleted {
            chat: Some(chat(1)),
            ids: vec![MsgId(11), MsgId(99)],
        })
        .await;
    let (ev, _) = h.until(|e| matches!(e, Event::Deleted { .. })).await;
    assert!(matches!(ev, Event::Deleted { ids, .. } if ids == vec![MsgId(11)]));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn mark_read_coalesces_per_flush() {
    let h = harness(FakeScript::demo(1, 10), None);
    for id in [3, 7, 5] {
        h.send(Command::MarkRead {
            chat: chat(1),
            max_id: MsgId(id),
        })
        .await;
    }
    tokio::time::sleep(super::READ_FLUSH * 3).await;
    assert_eq!(h.fake.read_calls(), vec![(chat(1), MsgId(7))]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn auth_commands_are_unsupported_for_now() {
    let mut h = harness(FakeScript::default(), None);
    h.send(Command::Logout).await;
    h.until(|e| matches!(e, Event::Error(UserError::Unsupported)))
        .await;
    h.stop().await;
}

// ---- disk cache (real time) ----

fn cache_at(dir: &std::path::Path, key: &CacheKey) -> CacheHandle {
    CacheHandle::spawn(dir.to_path_buf(), key).unwrap()
}

/// Opens chat 1, scrolls up `older` pages, then stops and flushes the cache.
async fn session(script: FakeScript, cache: CacheHandle, older: usize) -> FakeHandle {
    let mut h = harness(script, Some(cache.clone()));
    h.send(Command::OpenChat(chat(1))).await;
    h.until(|e| matches!(e, Event::History { dir: HistoryDir::Tail, view, .. } if view.0 > 1))
        .await;
    for _ in 0..older {
        h.send(Command::LoadOlder(chat(1))).await;
        h.history(chat(1), HistoryDir::Older).await;
    }
    let fake = h.fake.clone();
    h.stop().await;
    cache.barrier().await.unwrap();
    fake
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_serves_history_offline_then_reconciles() {
    let dir = tempfile::tempdir().unwrap();
    let key = CacheKey::generate().unwrap();
    session(FakeScript::demo(1, 200), cache_at(dir.path(), &key), 2).await;

    // Second run: the server hangs, so only the cache can answer.
    let cache = cache_at(dir.path(), &key);
    let mut h = harness(FakeScript::demo(1, 200), Some(cache.clone()));
    h.fake.inject(Op::History, Fault::Hang);
    h.send(Command::OpenChat(chat(1))).await;
    let ev = h.history(chat(1), HistoryDir::Tail).await;
    assert_eq!(history_ids(&ev), (151..=200).collect::<Vec<_>>());
    h.stop().await;

    // Third run: the server deleted message 190; the tail reconcile removes it.
    let mut script = FakeScript::demo(1, 200);
    script
        .history
        .get_mut(&chat(1))
        .unwrap()
        .retain(|m| m.id != MsgId(190));
    let cache = cache_at(dir.path(), &key);
    session(script, cache.clone(), 0).await;
    let tail = cache.tail(chat(1), 50).await.unwrap();
    assert!(!tail.iter().any(|m| m.id == MsgId(190)));
    assert_eq!(tail.last().map(|m| m.id), Some(MsgId(200)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contiguous_cache_serves_older_pages_without_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let key = CacheKey::generate().unwrap();
    session(FakeScript::demo(1, 300), cache_at(dir.path(), &key), 3).await;

    let cache = cache_at(dir.path(), &key);
    let mut h = harness(FakeScript::demo(1, 300), Some(cache));
    h.send(Command::OpenChat(chat(1))).await;
    h.until(|e| matches!(e, Event::History { dir: HistoryDir::Tail, view, .. } if view.0 > 1))
        .await;
    let server_calls = h.fake.calls(Op::History);
    h.send(Command::LoadOlder(chat(1))).await;
    let ev = h.history(chat(1), HistoryDir::Older).await;
    assert_eq!(history_ids(&ev), (201..=250).collect::<Vec<_>>());
    assert_eq!(
        h.fake.calls(Op::History),
        server_calls,
        "older page must come from the cache"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_in_cache_is_replaced_by_server_tail() {
    let dir = tempfile::tempdir().unwrap();
    let key = CacheKey::generate().unwrap();
    session(FakeScript::demo(1, 100), cache_at(dir.path(), &key), 1).await;

    // 200 more messages arrived while offline: the new tail does not overlap.
    let cache = cache_at(dir.path(), &key);
    session(FakeScript::demo(1, 300), cache.clone(), 0).await;
    let older = cache.before(chat(1), MsgId(251), 50).await.unwrap();
    assert!(
        older.is_empty(),
        "stale pre-gap history must be dropped, got {}",
        older.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn erase_runs_after_queued_writes() {
    let dir = tempfile::tempdir().unwrap();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    for i in 1..=100 {
        cache
            .write(crate::cache::CacheCmd::PutMessages(
                chat(1),
                vec![text_message(chat(1), i, "x")],
            ))
            .await;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let res = cache
        .erase(
            CacheKey::from_bytes(zeroize::Zeroizing::new(*key.as_bytes())),
            Box::new(move || tx.send(()).map_err(Into::into)),
        )
        .await;
    assert!(res.is_ok(), "erase failed: {res:?}");
    assert!(rx.try_recv().is_ok(), "key deletion ran");
    assert!(
        cache.tail(chat(1), 10).await.is_err(),
        "worker stops after erase"
    );
    assert!(std::fs::read_dir(dir.path()).map_or(true, |d| d.count() == 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_chat_list_keeps_every_loaded_page() {
    let dir = tempfile::tempdir().unwrap();
    let key = CacheKey::generate().unwrap();
    let cache = cache_at(dir.path(), &key);
    let mut h = harness(FakeScript::demo(150, 1), Some(cache.clone()));
    h.until(|e| matches!(e, Event::DialogsReset(_))).await;
    h.send(Command::LoadDialogs).await;
    let (ev, _) = h.until(|e| matches!(e, Event::DialogsAppended(_))).await;
    assert!(matches!(ev, Event::DialogsAppended(v) if v.len() == 50));
    h.stop().await;
    cache.barrier().await.unwrap();
    assert_eq!(cache.chats().await.unwrap().len(), 150);
}
