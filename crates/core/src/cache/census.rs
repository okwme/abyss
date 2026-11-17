//! Which cache databases are open, by directory, and SQLCipher's lifetime
//! around process exit (plan section 13, exit SIGSEGV fix and round 23).
//!
//! SQLCipher tears down its global state (crypto provider, private heap) in
//! an exit-time handler (`sqlcipher_atexit`, plus a `__mod_term_func`). A
//! thread still inside SQLCipher at that moment, typically a worker closing
//! its connection (`sqlite3_close` -> `sqlite3FreeCodecArg`), touches freed
//! memory and the process dies with SIGSEGV.
//!
//! The rules:
//! - [`init`] initializes SQLite, which runs `sqlcipher_extra_init` and so
//!   registers SQLCipher's exit handler, and only then registers ours. Exit
//!   handlers run in reverse order of registration, so ours runs first.
//!   `Cache::open` refuses to open anything until `init` has succeeded, and
//!   core calls it at startup before any background work exists.
//! - Every thread that may run SQLCipher code holds an [`Open`] entry. A
//!   `Cache` holds one for as long as its connection lives (the entry is its
//!   last field, so it drops after the connection, on unwinding too), and a
//!   caller takes one before it schedules an open on another thread, so a
//!   queued open is counted before it starts.
//! - Admission is closed atomically with draining: [`close_admission`] for
//!   one directory while core shuts down, [`begin_exit`] for the whole
//!   process. A closed admission refuses new entries, so an empty census
//!   stays empty.
//! - The exit hook closes admission and waits, bounded, for every entry. If
//!   one is still open at the deadline, it ends the process with `_exit`
//!   rather than return into SQLCipher's teardown.

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use rusqlite::ffi;

/// How long process exit waits for open caches to close.
pub const EXIT_WAIT: Duration = Duration::from_secs(3);
/// The process status when exit gives up waiting for a cache to close and
/// skips the rest of exit (`EX_TEMPFAIL`).
pub const EXIT_CACHE_BUSY: i32 = 75;
/// Distinct cache directories the census tracks at once. A process uses
/// one; tests use one per test. Beyond this, opens are refused.
const MAX_DIRS: usize = 256;
/// How often the exit hook retries a contended lock.
const LOCK_RETRY: Duration = Duration::from_millis(1);
/// Upper bound on those retries (the deadline normally ends them first).
const MAX_LOCK_TRIES: u32 = 10_000;

#[derive(Debug)]
struct State {
    /// Open entries per directory, each directory listed at most once.
    open: Vec<(PathBuf, usize)>,
    /// Directories whose admission is closed, with how many closers hold
    /// each (core shutdowns in progress).
    closed: Vec<(PathBuf, usize)>,
    /// Process exit has begun: nothing is admitted any more. Never reset.
    exiting: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    open: Vec::new(),
    closed: Vec::new(),
    exiting: false,
});
static CLOSED: Condvar = Condvar::new();
static INIT: OnceLock<Result<(), InitError>> = OnceLock::new();

fn lock() -> MutexGuard<'static, State> {
    // The state is updated in single steps, so a panic elsewhere never
    // leaves it half-written; a poisoned lock is still consistent.
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why SQLCipher could not be readied. Cache opens are refused after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// `sqlite3_initialize` failed with this code.
    Sqlite(i32),
    /// `atexit` could not register the exit hook.
    Atexit,
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(rc) => write!(f, "sqlite3_initialize failed ({rc})"),
            Self::Atexit => f.write_str("could not register the exit hook"),
        }
    }
}

/// Why a census entry was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Process exit, or a shutdown of this directory, has begun.
    Closing,
    /// [`MAX_DIRS`] directories are open already.
    Full,
}

/// Initializes SQLite and SQLCipher, then registers the exit hook after
/// SQLCipher's own exit handler. Idempotent; concurrent callers wait for
/// the first. Core calls it at startup, on the thread that starts core,
/// before any cache can be opened; `Cache::open` calls it too.
///
/// `sqlite3_initialize` runs `sqlcipher_extra_init` (SQLCipher is built
/// with `SQLITE_EXTRA_INIT=sqlcipher_extra_init`), whose first run calls
/// `atexit(sqlcipher_atexit)` before anything else (SQLCipher 4.20.0,
/// `sqlcipher.c`). If SQLite was already initialized, that registration
/// happened then. Either way it precedes ours.
pub fn init() -> Result<(), InitError> {
    *INIT.get_or_init(init_once)
}

fn init_once() -> Result<(), InitError> {
    // SAFETY: `sqlite3_initialize` takes no arguments, is thread-safe and
    // idempotent, and is called before any SQLite configuration we rely on.
    let rc = unsafe { ffi::sqlite3_initialize() };
    if rc != ffi::SQLITE_OK {
        tracing::error!(rc, "SQLite failed to initialize; running without a cache");
        return Err(InitError::Sqlite(rc));
    }
    // SAFETY: `exit_hook` is an `extern "C" fn()` that never unwinds (its
    // body runs under `catch_unwind`), as `atexit` requires.
    let rc = unsafe { libc::atexit(exit_hook) };
    if rc != 0 {
        tracing::error!(
            rc,
            "could not register the cache exit hook; running without a cache"
        );
        return Err(InitError::Atexit);
    }
    Ok(())
}

/// One thread that may be inside SQLCipher for the cache at `dir`.
/// Dropped once that thread is done with the database.
#[derive(Debug)]
pub(crate) struct Open {
    dir: PathBuf,
}

impl Open {
    /// Admits one more thread to the cache at `dir`, unless admission is
    /// closed. Atomic with [`begin_exit`] and [`close_admission`]: after
    /// either, no entry is ever admitted.
    pub(crate) fn enter(dir: &Path) -> Result<Self, Refused> {
        let mut state = lock();
        let closed = state.closed.iter().any(|(d, _)| d == dir);
        if state.exiting || closed {
            return Err(Refused::Closing);
        }
        let full = state.open.len() >= MAX_DIRS;
        match state.open.iter_mut().find(|(d, _)| d == dir) {
            Some((_, n)) => *n = n.saturating_add(1),
            None if full => return Err(Refused::Full),
            None => state.open.push((dir.to_path_buf(), 1)),
        }
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let mut state = lock();
        if let Some(i) = state.open.iter().position(|(d, _)| *d == self.dir) {
            let left = state.open.get_mut(i).map_or(0, |(_, n)| {
                *n = n.saturating_sub(1);
                *n
            });
            if left == 0 {
                state.open.swap_remove(i);
            }
        }
        drop(state);
        CLOSED.notify_all();
    }
}

/// Keeps `dir` in the census across a close followed by a reopen (logout,
/// re-binding), so a drain cannot finish in the gap between them. `None`
/// when admission is closed, in which case the reopen is refused too.
pub(crate) fn hold(dir: &Path) -> Option<Open> {
    Open::enter(dir).ok()
}

/// Admission to one directory is closed while this lives (core shutdown).
/// Entries already admitted are unaffected; drain them with
/// [`wait_closed`]. Dropping it reopens the directory once no other closer
/// holds it, so a later core can use the directory again.
#[derive(Debug)]
pub(crate) struct AdmissionClosed {
    dir: PathBuf,
}

/// Closes admission to the cache at `dir` (see [`AdmissionClosed`]).
pub(crate) fn close_admission(dir: &Path) -> AdmissionClosed {
    let mut state = lock();
    match state.closed.iter_mut().find(|(d, _)| d == dir) {
        Some((_, n)) => *n = n.saturating_add(1),
        None => state.closed.push((dir.to_path_buf(), 1)),
    }
    AdmissionClosed {
        dir: dir.to_path_buf(),
    }
}

impl Drop for AdmissionClosed {
    fn drop(&mut self) {
        let mut state = lock();
        if let Some(i) = state.closed.iter().position(|(d, _)| *d == self.dir) {
            let left = state.closed.get_mut(i).map_or(0, |(_, n)| {
                *n = n.saturating_sub(1);
                *n
            });
            if left == 0 {
                state.closed.swap_remove(i);
            }
        }
    }
}

/// Closes admission to every cache for the rest of the process: process
/// exit has begun. Open caches keep working until they close. Called by the
/// exit hook, and earlier by the app when it starts to quit.
pub fn begin_exit() {
    lock().exiting = true;
}

fn any_open(state: &State, dir: Option<&Path>) -> bool {
    state
        .open
        .iter()
        .any(|(d, _)| dir.is_none_or(|dir| d == dir))
}

/// Waits on `state` until no cache at `dir` (every cache, for `None`) is
/// open, or `deadline` passes. Whether they all closed.
fn drain(mut state: MutexGuard<'static, State>, dir: Option<&Path>, deadline: Instant) -> bool {
    while any_open(&state, dir) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        state = CLOSED
            .wait_timeout(state, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    true
}

/// Blocks until no cache at `dir` (every cache, for `None`) is open, or
/// `timeout` passes. Whether they all closed. Never call it on an async
/// runtime thread. Pair it with closed admission ([`close_admission`],
/// [`begin_exit`]) when nothing may open after it returns.
pub fn wait_closed(dir: Option<&Path>, timeout: Duration) -> bool {
    drain(lock(), dir, Instant::now() + timeout)
}

/// The census lock, or `None` if it stays contended until `deadline`. The
/// lock is only ever held for a few list operations, never across
/// SQLCipher, so contention is momentary; the bound covers a thread that
/// called `exit` while holding it, which our code never does.
fn lock_until(deadline: Instant) -> Option<MutexGuard<'static, State>> {
    for _ in 0..MAX_LOCK_TRIES {
        match STATE.try_lock() {
            Ok(state) => return Some(state),
            Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_RETRY);
            }
            Err(TryLockError::WouldBlock) => return None,
        }
    }
    None
}

/// Closes admission and waits, bounded by [`EXIT_WAIT`] including the lock,
/// for every open cache to close. Whether they all did.
fn drain_at_exit() -> bool {
    let deadline = Instant::now() + EXIT_WAIT;
    let Some(mut state) = lock_until(deadline) else {
        tracing::error!("cache census locked at exit");
        return false;
    };
    state.exiting = true;
    let drained = drain(state, None, deadline);
    if !drained {
        tracing::error!(
            code = EXIT_CACHE_BUSY,
            "a cache was still open at exit; ending the process without running exit handlers"
        );
    }
    drained
}

/// Runs at process exit, before SQLCipher's own exit handler (see
/// [`init`]).
///
/// SAFETY contract (it is an `atexit` callback):
/// - It must never unwind: a panic out of an `extern "C" fn` aborts. The
///   body runs under `catch_unwind`, and a caught panic counts as "not
///   drained".
/// - The wait is bounded by [`EXIT_WAIT`], lock acquisition included
///   ([`lock_until`]), so exit never hangs.
/// - `exit` is called by the main thread (GPUI's terminate, or `main`
///   returning in tests), which never holds the census lock or an entry:
///   caches are opened and closed on core's runtime, blocking and
///   `abyss-cache` threads, and no cache code calls `exit`. Were a thread
///   holding an entry to call `exit` anyway, the wait would time out on its
///   own entry and end in `_exit` below: a delay, not a crash or a hang.
/// - If anything is still open at the deadline, returning would run
///   SQLCipher's teardown under a live connection. So it ends the process
///   with `_exit(EXIT_CACHE_BUSY)` instead. That skips the remaining
///   `atexit` handlers and C stdio flushing (our logs go unbuffered to
///   stderr). SQLite durability is unaffected: committed transactions are
///   durable, and the WAL is recovered on the next open.
extern "C" fn exit_hook() {
    let drained = std::panic::catch_unwind(drain_at_exit).unwrap_or(false);
    #[cfg(test)]
    tests::probe_at_exit();
    if !drained {
        abandon_exit();
    }
}

fn abandon_exit() {
    // SAFETY: `_exit` takes a plain status and never returns; it skips
    // exit handlers by design (see `exit_hook`).
    unsafe { libc::_exit(EXIT_CACHE_BUSY) }
}

#[cfg(test)]
pub(crate) fn is_open(dir: &Path) -> bool {
    any_open(&lock(), Some(dir))
}

#[cfg(test)]
mod tests;
