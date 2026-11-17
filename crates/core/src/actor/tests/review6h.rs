//! Regression tests for review round 31 (re-check of task 6g), task 6h.
//! Each one fails on the code before its fix. Finding 2 (paused clock vs.
//! the persistence thread) is proven by the whole suite under
//! `ABYSS_TEST_STORE_DELAY_MS=50` (`secret::ClockGate`); finding 4 is in
//! `core/startup_tests.rs`.

use super::review6g::{logout_stalled, settled, stay_signed_in};
use super::{Harness, chat, harness};
use crate::actor::sends::{MAX_KEPT_SENDS, MAX_PENDING_SENDS};
use crate::api::{AuthState, Command, Event, SendState, UserError};
use crate::backend::fake::FakeScript;
use crate::model::LocalId;

/// Sends `LocalId(id)` and returns the state it settles in.
async fn send_settles(h: &mut Harness, id: u64) -> SendState {
    h.send(Command::SendText {
        chat: chat(1),
        text: format!("m{id}"),
        local_id: LocalId(id),
    })
    .await;
    match h.until(settled(LocalId(id))).await.0 {
        Event::SendState { state, .. } => state,
        _ => unreachable!(),
    }
}

/// Finding 1: more sends are refused behind a stalled logout than the send
/// table holds. Held refusals have their own bound, so after "Stay signed
/// in" an ordinary send still has a slot, and a held one is retried with
/// its payload. One refused past the held bound is answered as unknown.
#[tokio::test(start_paused = true)]
async fn refused_sends_never_take_the_ordinary_send_slots() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    logout_stalled(&mut h).await;
    let refused = u64::try_from(MAX_PENDING_SENDS).unwrap();
    for id in 1..=refused {
        assert_eq!(send_settles(&mut h, id).await, SendState::Failed);
    }
    stay_signed_in(&mut h).await;
    let fresh = refused + 1;
    assert!(
        matches!(send_settles(&mut h, fresh).await, SendState::Sent { .. }),
        "an ordinary send found no slot"
    );
    h.send(Command::RetrySend(LocalId(1))).await;
    let (ev, _) = h.until(settled(LocalId(1))).await;
    assert!(
        matches!(
            ev,
            Event::SendState {
                state: SendState::Sent { .. },
                ..
            }
        ),
        "a held send was not kept: {ev:?}"
    );
    // Past the held bound: no record, so its Retry is answered as unknown.
    let past = u64::try_from(MAX_KEPT_SENDS).unwrap() + 1;
    h.send(Command::RetrySend(LocalId(past))).await;
    let (ev, seen) = h.until(settled(LocalId(past))).await;
    assert!(matches!(
        ev,
        Event::SendState {
            state: SendState::Failed,
            ..
        }
    ));
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Error(UserError::NotFound)))
    );
    h.stop().await;
}

/// Answers a Retry of a send core does not know: `Error(NotFound)`, then
/// `Failed`.
async fn unknown_retry_is_not_found(h: &mut Harness, id: u64) {
    h.send(Command::RetrySend(LocalId(id))).await;
    let (ev, seen) = h.until(settled(LocalId(id))).await;
    assert!(
        matches!(
            ev,
            Event::SendState {
                state: SendState::Failed,
                ..
            }
        ),
        "{ev:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Error(UserError::NotFound))),
        "no NotFound before Failed: {seen:?}"
    );
}

/// Finding 3: a Retry of an unknown send behind the overlay is answered as
/// it is elsewhere, while "Logging out" and while "Couldn't start logout"
/// is shown.
#[tokio::test(start_paused = true)]
async fn an_unknown_retry_behind_the_overlay_is_not_found() {
    let mut h = harness(FakeScript::demo(1, 1), None).await;
    h.online().await;
    logout_stalled(&mut h).await;
    unknown_retry_is_not_found(&mut h, 77).await;
    h.faults.release();
    h.auth(|s| matches!(s, AuthState::LogoutNotStarted { .. }))
        .await;
    unknown_retry_is_not_found(&mut h, 78).await;
    h.stop().await;
}
