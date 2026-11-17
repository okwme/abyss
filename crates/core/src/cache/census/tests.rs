//! Census and exit-hook tests (round 23). Process exit and `begin_exit` are
//! one-way, so those run in a child process: this test binary, re-run on
//! one test (`child`) with `CHILD` naming the scenario.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{EXIT_CACHE_BUSY, Open, Refused, begin_exit, close_admission, init, wait_closed};
use crate::cache::{Cache, CacheError, CacheKey};

const CHILD: &str = "ABYSS_CENSUS_CHILD";
/// Child status: the exit hook ran while SQLCipher was still initialized.
const HOOK_FIRST: i32 = 42;
/// Child status: the exit hook ran after SQLCipher's teardown.
const HOOK_AFTER_TEARDOWN: i32 = 43;

static PROBE: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    /// SQLCipher's default provider; `NULL` once `sqlcipher_extra_shutdown`
    /// has run (SQLCipher 4.20.0, `sqlcipher.c`).
    fn sqlcipher_get_provider() -> *const std::ffi::c_void;
}

/// Called by the exit hook after its wait. With the probe armed, ends the
/// child with a status saying whether SQLCipher was still up.
pub(super) fn probe_at_exit() {
    if !PROBE.load(Ordering::SeqCst) {
        return;
    }
    // SAFETY: a plain getter of a global pointer, never dereferenced.
    let provider = unsafe { sqlcipher_get_provider() };
    let status = if provider.is_null() {
        HOOK_AFTER_TEARDOWN
    } else {
        HOOK_FIRST
    };
    // SAFETY: ends the child at once with `status`.
    unsafe { libc::_exit(status) }
}

/// Runs `child` in a fresh process for `scenario`; its exit status.
fn run_child(scenario: &str) -> Option<i32> {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cache::census::tests::child", "--test-threads=1"])
        .env(CHILD, scenario)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .code()
}

/// Runs `child` for `scenario` with its stderr piped and never read, and
/// waits at most `limit`. Its exit status (`None` if it had to be killed)
/// and how long it ran.
fn run_child_bounded(scenario: &str, limit: Duration) -> (Option<i32>, Duration) {
    let started = std::time::Instant::now();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cache::census::tests::child", "--test-threads=1"])
        .env(CHILD, scenario)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    while started.elapsed() < limit {
        if let Some(status) = child.try_wait().unwrap() {
            return (status.code(), started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    (None, started.elapsed())
}

/// Holds a census entry on another thread for the rest of the process.
fn hold_an_entry_forever() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let dir = tempfile::tempdir().unwrap();
        let _entry = Open::enter(dir.path()).unwrap();
        tx.send(()).unwrap();
        loop {
            std::thread::park();
        }
    });
    rx.recv().unwrap();
}

/// Fills stderr (a pipe nobody reads) until a write would block, then makes
/// it blocking again: from now on every stderr write blocks forever.
fn fill_stderr() {
    // SAFETY: reads fd 2's status flags.
    let flags = unsafe { libc::fcntl(2, libc::F_GETFL) };
    assert!(flags >= 0);
    // SAFETY: sets fd 2 non-blocking, so the fill below stops when full.
    assert!(unsafe { libc::fcntl(2, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0);
    let junk = [b'x'; 4096];
    let mut full = false;
    for _ in 0..100_000 {
        // SAFETY: `junk` is a live buffer of `junk.len()` bytes.
        let n = unsafe { libc::write(2, junk.as_ptr().cast(), junk.len()) };
        if n < 0 {
            full = true;
            break;
        }
    }
    // SAFETY: restores fd 2's original (blocking) flags.
    assert!(unsafe { libc::fcntl(2, libc::F_SETFL, flags) } >= 0);
    assert!(full, "stderr is a pipe that filled up");
}

/// Once the exit hook has closed admission (it is waiting), takes the
/// census lock and holds it for 6 s, twice `EXIT_WAIT`.
fn hold_the_lock_once_the_hook_waits() {
    std::thread::spawn(|| {
        for _ in 0..1_000_000 {
            let state = super::lock();
            if state.exiting {
                std::thread::sleep(Duration::from_secs(6));
                return;
            }
            drop(state);
            std::thread::sleep(Duration::from_millis(1));
        }
    });
}

/// The child side. Does nothing unless run by `run_child`.
#[test]
fn child() {
    let Some(scenario) = std::env::var_os(CHILD) else {
        return;
    };
    init().unwrap();
    match scenario.to_str().unwrap() {
        "order" => {
            PROBE.store(true, Ordering::SeqCst);
            std::process::exit(0);
        }
        "closed" => {
            let dir = tempfile::tempdir().unwrap();
            begin_exit();
            assert_eq!(Open::enter(dir.path()).err(), Some(Refused::Closing));
            let key = CacheKey::generate().unwrap();
            assert!(matches!(
                Cache::open(dir.path(), &key),
                Err(CacheError::Refused(Refused::Closing))
            ));
            assert!(!dir.path().join("cache.db").exists(), "nothing was opened");
        }
        "busy" => {
            let dir = tempfile::tempdir().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let path = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let _entry = Open::enter(&path).unwrap();
                tx.send(()).unwrap();
                std::thread::park();
            });
            rx.recv().unwrap();
            std::process::exit(0);
        }
        "busy-stderr" => {
            // Logs go to stderr, as in the app.
            let subscriber = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
            hold_an_entry_forever();
            fill_stderr();
            std::process::exit(0);
        }
        "busy-locked" => {
            hold_an_entry_forever();
            hold_the_lock_once_the_hook_waits();
            std::process::exit(0);
        }
        other => panic!("unknown scenario {other}"),
    }
}

/// Finding 1: the exit hook is registered after SQLCipher's exit handler,
/// so it runs while SQLCipher is still initialized. With `init` registering
/// the hook before `sqlite3_initialize`, the child reports
/// `HOOK_AFTER_TEARDOWN`.
#[test]
fn the_exit_hook_runs_before_sqlcipher_teardown() {
    assert_eq!(run_child("order"), Some(HOOK_FIRST));
}

/// Finding 2: once exit has begun, nothing is admitted, and `Cache::open`
/// fails cleanly without touching SQLCipher or the disk.
#[test]
fn admission_is_closed_after_begin_exit() {
    assert_eq!(run_child("closed"), Some(0));
}

/// Finding 3: a cache still open when the exit wait runs out ends the
/// process with `_exit(EXIT_CACHE_BUSY)` instead of returning into
/// SQLCipher's teardown.
#[test]
fn exit_with_a_cache_still_open_skips_sqlcipher_teardown() {
    assert_eq!(run_child("busy"), Some(EXIT_CACHE_BUSY));
}

/// How long a timed-out exit may take: `EXIT_WAIT`, plus the child's own
/// start-up and a margin.
const EXIT_BOUND: Duration = Duration::from_millis(4_500);

/// Round 24, finding 1: the timeout path must not log. With stderr a full
/// pipe nobody reads, a log write before `_exit` blocks forever; the child
/// still ends with `EXIT_CACHE_BUSY` within the bound.
#[test]
fn a_full_stderr_does_not_delay_the_exit_fallback() {
    let (code, took) = run_child_bounded("busy-stderr", EXIT_BOUND + Duration::from_secs(3));
    assert_eq!(code, Some(EXIT_CACHE_BUSY), "after {took:?}");
    assert!(took < EXIT_BOUND, "took {took:?}");
}

/// Round 24, finding 2: the hook never blocks on the census lock. A thread
/// that takes the lock once the hook is waiting and holds it for 6 s does
/// not hold up the exit past `EXIT_WAIT` (a condvar wait would re-acquire
/// the lock only after those 6 s).
#[test]
fn a_held_census_lock_does_not_delay_the_exit_fallback() {
    let (code, took) = run_child_bounded("busy-locked", EXIT_BOUND + Duration::from_secs(4));
    assert_eq!(code, Some(EXIT_CACHE_BUSY), "after {took:?}");
    assert!(took < EXIT_BOUND, "took {took:?}");
}

fn census_empty(dir: &Path) -> bool {
    wait_closed(Some(dir), Duration::ZERO)
}

/// Core shutdown closes admission to its directory: entries already in are
/// kept (and drained), new ones are refused, and the directory reopens once
/// every closer is gone.
#[test]
fn closed_admission_refuses_new_entries_until_reopened() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let held = Open::enter(dir.path()).unwrap();
    let first = close_admission(dir.path());
    let second = close_admission(dir.path());
    assert_eq!(Open::enter(dir.path()).err(), Some(Refused::Closing));
    let key = CacheKey::generate().unwrap();
    assert!(matches!(
        Cache::open(dir.path(), &key),
        Err(CacheError::Refused(Refused::Closing))
    ));
    drop(Open::enter(other.path()).unwrap());
    assert!(!census_empty(dir.path()), "the admitted entry stays");
    drop(held);
    assert!(census_empty(dir.path()));
    drop(first);
    assert!(
        Open::enter(dir.path()).is_err(),
        "still closed by the second"
    );
    drop(second);
    drop(Open::enter(dir.path()).unwrap());
    assert!(census_empty(dir.path()));
}
