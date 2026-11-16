//! Startup and shutdown of the core thread (review round 8, task 6c).

use std::time::{Duration, Instant};

use super::{CoreConfig, SendError, start_with};
use crate::SHUTDOWN_DEADLINE;
use crate::api::{Command, SaveStatus};
use crate::backend::fake::{FakeBackend, FakeScript};
use crate::secret::{FaultyStore, MemoryStore};

/// Finding 4: a stalled Keychain read at startup (a prompt nobody answers)
/// must not hold up the shutdown acknowledgement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_a_stalled_session_load_acks_promptly() {
    let (fake, _h) = FakeBackend::new(FakeScript::default());
    let (store, control) = FaultyStore::new(MemoryStore::new());
    control.hold();
    let mut core = start_with(CoreConfig::default(), move |_| fake, store, || {}).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    let done = core.begin_shutdown().unwrap();
    let report = tokio::time::timeout(Duration::from_secs(3), done)
        .await
        .expect("the ack waited for the stalled store")
        .unwrap();
    control.release();
    assert_eq!(report.save, SaveStatus::Unconfirmed);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// Finding 5: shutdown while the *command* queue is full. Nobody reads
/// events, so the actor blocks emitting and commands pile up; shutdown runs
/// on the control plane and still acknowledges within its deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_with_a_full_command_queue_acks_within_the_deadline() {
    let (fake, _h) = FakeBackend::new(FakeScript::default());
    let (store, _control) = FaultyStore::new(MemoryStore::new());
    let mut core = start_with(CoreConfig::default(), move |_| fake, store, || {}).unwrap();
    let mut full = false;
    for _ in 0..100_000 {
        // Each invalid phone number is answered with an auth event.
        match core.send(Command::SubmitPhone("x".into())) {
            Ok(()) => {}
            Err(SendError::Full(_)) => {
                full = true;
                break;
            }
            Err(SendError::Closed(_)) => break,
        }
        tokio::task::yield_now().await;
    }
    assert!(full, "the command queue never filled");
    let started = Instant::now();
    let report = tokio::time::timeout(
        SHUTDOWN_DEADLINE + Duration::from_secs(2),
        core.begin_shutdown().unwrap(),
    )
    .await
    .expect("shutdown was blocked by the full queue")
    .unwrap();
    assert!(started.elapsed() <= SHUTDOWN_DEADLINE + Duration::from_secs(1));
    assert_eq!(report.logout, None);
}
