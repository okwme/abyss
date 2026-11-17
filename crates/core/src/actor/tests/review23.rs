//! Round 23 (review of the exit SIGSEGV fix): shutdown closes core's
//! caches alongside network retirement, not after it, and admits no new
//! cache open while it runs.

use std::time::Duration;

use super::{CacheDir, Setup, chat, start};
use crate::api::{Command, HistoryDir};
use crate::backend::fake::{FakeBackend, FakeScript, QuitBehavior};
use crate::cache::{Cache, CacheError, CacheKey, Refused, wait_closed};

/// Finding 3: with a runner that ignores quit, retirement runs to its 2 s
/// cap. The cache must be closed well before that, while admission to its
/// directory is closed; once the actor is done the directory reopens.
/// With the caches closed only after retirement, the census is still
/// non-empty after 1 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_the_cache_while_retirement_still_runs() {
    let dir = CacheDir::new();
    let (backend, fake) = FakeBackend::new(FakeScript::demo(1, 50));
    let setup = Setup {
        cache_dir: Some(dir.path().to_path_buf()),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    h.send(Command::OpenChat(chat(1))).await;
    h.history(chat(1), HistoryDir::Tail).await;
    h.fake.set_quit(QuitBehavior::Ignore);

    h.cancel.cancel();
    let path = dir.path().to_path_buf();
    let drained = tokio::task::spawn_blocking(move || {
        let drained = wait_closed(Some(&path), Duration::from_secs(1));
        let key = CacheKey::generate().unwrap();
        (drained, Cache::open(&path, &key))
    });
    let (drained, reopened) = drained.await.unwrap();
    assert!(drained, "the cache closed while retirement ran");
    assert!(!h.join.is_finished(), "retirement is still running");
    assert!(matches!(
        reopened,
        Err(CacheError::Refused(Refused::Closing))
    ));
    h.join.await.unwrap();

    let path = dir.path().to_path_buf();
    let key = CacheKey::generate().unwrap();
    let after = tokio::task::spawn_blocking(move || Cache::open(&path, &key));
    // Admitted again (the key is not this cache's, so it cannot read it).
    assert!(matches!(after.await.unwrap(), Err(CacheError::WrongKey)));
}
