//! Census and exit-hook tests (rounds 23, 24, 27). Process exit and
//! `begin_exit` are one-way, so those run in a child process: this test
//! binary, re-run on one test (`child`) with `CHILD` naming the scenario.
//!
//! The child reports progress as `CENSUS:<marker>` lines written straight to
//! fd 1 (past libtest's capture). The parent starts timing at `ready`, so
//! the child's start-up and the parent's scheduling are not measured. A
//! regression shows as a child that never exits (a persistently blocked
//! condition), caught by a generous watchdog, not as a few seconds of
//! difference.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{
    EXIT_CACHE_BUSY, EXIT_WAIT, Open, Refused, begin_exit, close_admission, init, wait_closed,
};
use crate::cache::{Cache, CacheError, CacheKey};

const CHILD: &str = "ABYSS_CENSUS_CHILD";
/// Child status: the exit hook ran while SQLCipher was still initialized.
const HOOK_FIRST: i32 = 42;
/// Child status: the exit hook ran after SQLCipher's teardown.
const HOOK_AFTER_TEARDOWN: i32 = 43;
/// Child status: something allocated inside the exit hook.
const ALLOCATED_IN_HOOK: i32 = 44;
/// How long a child may take to report `ready`.
const READY_WATCHDOG: Duration = Duration::from_secs(60);
/// How long a child may take to exit after `ready`. A healthy one needs
/// `EXIT_WAIT` at most; a regression never exits.
const EXIT_WATCHDOG: Duration = Duration::from_secs(30);

static PROBE: AtomicBool = AtomicBool::new(false);
/// The scenario checks the hook for allocations.
static GUARD_ARMED: AtomicBool = AtomicBool::new(false);
/// The hook is running with the guard armed: any allocation ends the child.
static FORBID: AtomicBool = AtomicBool::new(false);
/// The hook's drain panics with a payload whose destructor panics.
static PANIC_IN_DRAIN: AtomicBool = AtomicBool::new(false);
static DRAIN_STARTED: AtomicBool = AtomicBool::new(false);

/// The test binary's allocator: the system's, except that it ends the
/// process with `ALLOCATED_IN_HOOK` when asked to allocate while
/// `FORBID` is set.
struct GuardedAlloc;

#[global_allocator]
static ALLOC: GuardedAlloc = GuardedAlloc;

fn check_allowed() {
    if FORBID.load(Ordering::SeqCst) {
        // SAFETY: ends the child at once; nothing here allocates.
        unsafe { libc::_exit(ALLOCATED_IN_HOOK) }
    }
}

// SAFETY: every method forwards to `System` with the caller's arguments,
// after a check that only reads an atomic (and may end the process).
unsafe impl GlobalAlloc for GuardedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        check_allowed();
        // SAFETY: forwarded as is; the caller upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        check_allowed();
        // SAFETY: forwarded as is.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        check_allowed();
        // SAFETY: forwarded as is.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded as is.
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Called first thing by the exit hook.
pub(super) fn forbid_allocation() {
    if GUARD_ARMED.load(Ordering::SeqCst) {
        FORBID.store(true, Ordering::SeqCst);
    }
}

/// Called by the exit hook before it returns into the rest of exit.
pub(super) fn allow_allocation() {
    FORBID.store(false, Ordering::SeqCst);
}

/// A panic payload whose destructor panics again.
struct PanicsOnDrop;

impl Drop for PanicsOnDrop {
    fn drop(&mut self) {
        panic!("a panic payload was dropped");
    }
}

/// Called when the hook's drain starts.
pub(super) fn drain_started() {
    DRAIN_STARTED.store(true, Ordering::SeqCst);
    if PANIC_IN_DRAIN.load(Ordering::SeqCst) {
        std::panic::resume_unwind(Box::new(PanicsOnDrop));
    }
}

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

/// Child side: reports `marker` to the parent on fd 1, unbuffered.
fn signal(marker: &str) {
    let line = format!("CENSUS:{marker}\n");
    // SAFETY: `line` is a live buffer of `line.len()` bytes.
    let n = unsafe { libc::write(1, line.as_ptr().cast(), line.len()) };
    assert_eq!(usize::try_from(n).ok(), Some(line.len()));
}

/// What a child run showed.
#[derive(Debug)]
struct Run {
    /// Exit status; `None` if it was killed by a watchdog or a signal.
    code: Option<i32>,
    /// Markers in the order reported.
    markers: Vec<String>,
    /// From `ready` to exit.
    after_ready: Duration,
}

/// Runs `child` for `scenario`. Its stderr is a pipe nobody reads.
fn run(scenario: &str) -> Run {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cache::census::tests::child", "--test-threads=1"])
        .env(CHILD, scenario)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some((_, marker)) = line.split_once("CENSUS:") {
                let _ = tx.send((marker.to_owned(), Instant::now()));
            }
        }
    });
    let mut markers = Vec::new();
    let mut ready = None;
    while ready.is_none() {
        match rx.recv_timeout(READY_WATCHDOG) {
            Ok((marker, at)) => {
                if marker == "ready" {
                    ready = Some(at);
                }
                markers.push(marker);
            }
            Err(_) => break,
        }
    }
    let start = ready.unwrap_or_else(Instant::now);
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if ready.is_none() || start.elapsed() > EXIT_WATCHDOG {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let after_ready = start.elapsed();
    reader.join().unwrap();
    markers.extend(rx.try_iter().map(|(marker, _)| marker));
    Run {
        code,
        markers,
        after_ready,
    }
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

/// Takes the census lock on another thread (once `after` returns), reports
/// `held`, and keeps it for the rest of the process.
fn hold_the_lock_forever(after: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        after();
        let _state = super::lock();
        signal("held");
        loop {
            std::thread::park();
        }
    });
}

/// Returns once another thread holds the census lock.
fn wait_until_the_lock_is_held() {
    for _ in 0..100_000 {
        if matches!(
            super::STATE.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("the lock was never taken");
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

/// The child side. Does nothing unless run by `run`.
#[test]
fn child() {
    let Some(scenario) = std::env::var_os(CHILD) else {
        return;
    };
    init().unwrap();
    let scenario = scenario.to_str().unwrap().to_owned();
    if scenario == "closed" {
        return closed_child();
    }
    match scenario.as_str() {
        "order" => PROBE.store(true, Ordering::SeqCst),
        "idle" => GUARD_ARMED.store(true, Ordering::SeqCst),
        "busy" => {
            GUARD_ARMED.store(true, Ordering::SeqCst);
            hold_an_entry_forever();
        }
        "busy-stderr" => {
            // Logs go to stderr, as in the app.
            let subscriber = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
            hold_an_entry_forever();
            fill_stderr();
        }
        "busy-locked" => {
            hold_an_entry_forever();
            hold_the_lock_forever(|| {
                while !DRAIN_STARTED.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
        }
        "begin-exit-locked" => {
            let (tx, rx) = std::sync::mpsc::channel();
            hold_the_lock_forever(move || tx.send(()).unwrap());
            rx.recv().unwrap();
            wait_until_the_lock_is_held();
            signal("ready");
            begin_exit();
            signal("begun");
            // SAFETY: ends the child at once; `begin_exit` returned.
            unsafe { libc::_exit(0) }
        }
        "panic-payload" => PANIC_IN_DRAIN.store(true, Ordering::SeqCst),
        other => panic!("unknown scenario {other}"),
    }
    signal("ready");
    std::process::exit(0);
}

fn closed_child() {
    signal("ready");
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

/// Round 23, finding 1: the exit hook is registered after SQLCipher's exit
/// handler, so it runs while SQLCipher is still initialized. With `init`
/// registering the hook before `sqlite3_initialize`, the child reports
/// `HOOK_AFTER_TEARDOWN`.
#[test]
fn the_exit_hook_runs_before_sqlcipher_teardown() {
    assert_eq!(run("order").code, Some(HOOK_FIRST));
}

/// Round 23, finding 2: once exit has begun, nothing is admitted, and
/// `Cache::open` fails cleanly without touching SQLCipher or the disk.
#[test]
fn admission_is_closed_after_begin_exit() {
    assert_eq!(run("closed").code, Some(0));
}

/// Round 23, finding 3: a cache still open when the exit wait runs out ends
/// the process with `_exit(EXIT_CACHE_BUSY)` instead of returning into
/// SQLCipher's teardown, after the full wait. Round 27: and the hook
/// allocates nothing on the way (the child would end with
/// `ALLOCATED_IN_HOOK`).
#[test]
fn exit_with_a_cache_still_open_skips_sqlcipher_teardown() {
    let run = run("busy");
    assert_eq!(run.code, Some(EXIT_CACHE_BUSY), "{run:?}");
    assert!(run.after_ready >= EXIT_WAIT, "{run:?}");
}

/// Round 27, finding 1: exit right after `init`, with nothing in the census
/// ever, allocates nothing in the hook (the child would end with
/// `ALLOCATED_IN_HOOK`) and then exits normally.
#[test]
fn the_exit_hook_allocates_nothing_after_a_bare_init() {
    assert_eq!(run("idle").code, Some(0));
}

/// Round 24, finding 1: the timeout path must not log. With stderr a full
/// pipe nobody reads, a log write blocks forever; the child must still end
/// with `EXIT_CACHE_BUSY`, not be killed by the watchdog.
#[test]
fn a_full_stderr_does_not_delay_the_exit_fallback() {
    let run = run("busy-stderr");
    assert_eq!(run.code, Some(EXIT_CACHE_BUSY), "{run:?}");
}

/// Round 24, finding 2 (round 27: proven held): a thread takes the census
/// lock after the hook's drain has started and keeps it until the process
/// ends. The hook must not care: `EXIT_CACHE_BUSY`, not the watchdog.
#[test]
fn a_held_census_lock_does_not_delay_the_exit_fallback() {
    let run = run("busy-locked");
    assert_eq!(run.code, Some(EXIT_CACHE_BUSY), "{run:?}");
    assert!(run.markers.iter().any(|m| m == "held"), "{run:?}");
}

/// Round 27, finding 2: `begin_exit` (called from the app's `on_app_quit`)
/// returns while another thread holds the census lock forever.
#[test]
fn begin_exit_does_not_wait_for_the_census_lock() {
    let run = run("begin-exit-locked");
    assert_eq!(run.code, Some(0), "{run:?}");
    for marker in ["held", "ready", "begun"] {
        assert!(run.markers.iter().any(|m| m == marker), "{run:?}");
    }
}

/// Round 27: a panic inside the hook ends the process with
/// `EXIT_CACHE_BUSY`, and its payload is never dropped: this payload's
/// destructor panics, which outside the catch would abort at the C
/// boundary (a signal, no status).
#[test]
fn a_panic_in_the_exit_hook_ends_with_exit_cache_busy() {
    let run = run("panic-payload");
    assert_eq!(run.code, Some(EXIT_CACHE_BUSY), "{run:?}");
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
