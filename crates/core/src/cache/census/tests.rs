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
