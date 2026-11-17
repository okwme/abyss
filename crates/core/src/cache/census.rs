//! Which cache databases are open, by directory.
//!
//! SQLCipher tears down its global state (crypto provider, private heap) in
//! an exit-time handler (`sqlcipher_atexit`, plus a `__mod_term_func`). A
//! thread still inside SQLCipher at that moment, typically a worker closing
//! its connection (`sqlite3_close` -> `sqlite3FreeCodecArg`), touches freed
//! memory and the process dies with SIGSEGV after `main` has returned.
//!
//! So every thread that may run SQLCipher code holds an [`Open`] entry, taken
//! before `Cache::open` and released only after the connection is closed.
//! Shutdown waits for its cache directory to drain ([`wait_closed`]) before
//! acknowledging, and [`guard_exit`] makes process exit wait, bounded, for
//! every entry, which covers exits core does not control (an OS-initiated
//! quit, the UI's own timeout).

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, Once, PoisonError};
use std::time::{Duration, Instant};

/// How long process exit waits for open caches to close.
pub const EXIT_WAIT: Duration = Duration::from_secs(3);

/// Open caches per directory. A directory is listed at most once, so the
/// list is bounded by the number of distinct cache directories in use.
static OPEN: Mutex<Vec<(PathBuf, usize)>> = Mutex::new(Vec::new());
static CLOSED: Condvar = Condvar::new();
static EXIT_GUARD: Once = Once::new();

fn lock() -> MutexGuard<'static, Vec<(PathBuf, usize)>> {
    // The list is updated in single steps, so a panic elsewhere never leaves
    // it half-written; a poisoned lock is still consistent.
    OPEN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One thread that may be inside SQLCipher for the cache at `dir`.
/// Dropped once that thread is done with the database.
#[derive(Debug)]
pub(crate) struct Open {
    dir: PathBuf,
}

impl Open {
    pub(crate) fn enter(dir: &Path) -> Self {
        let mut open = lock();
        match open.iter_mut().find(|(d, _)| d == dir) {
            Some((_, n)) => *n = n.saturating_add(1),
            None => open.push((dir.to_path_buf(), 1)),
        }
        Self {
            dir: dir.to_path_buf(),
        }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let mut open = lock();
        if let Some(i) = open.iter().position(|(d, _)| *d == self.dir) {
            let left = open.get_mut(i).map_or(0, |(_, n)| {
                *n = n.saturating_sub(1);
                *n
            });
            if left == 0 {
                open.swap_remove(i);
            }
        }
        drop(open);
        CLOSED.notify_all();
    }
}

fn any_open(open: &[(PathBuf, usize)], dir: Option<&Path>) -> bool {
    open.iter().any(|(d, _)| dir.is_none_or(|dir| d == dir))
}

/// Blocks until no cache at `dir` (every cache, for `None`) is open, or
/// `timeout` passes. Whether they all closed. Never call it on an async
/// runtime thread.
pub fn wait_closed(dir: Option<&Path>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut open = lock();
    while any_open(&open, dir) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        open = CLOSED
            .wait_timeout(open, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    true
}

extern "C" fn wait_at_exit() {
    // Nothing to report to: exit continues either way.
    let _ = wait_closed(None, EXIT_WAIT);
}

/// Makes process exit wait (up to [`EXIT_WAIT`]) for every open cache to
/// close before SQLCipher's own exit-time teardown runs. Call it after the
/// first database is open: exit handlers run in reverse order of
/// registration, and SQLCipher registers its handler when it initializes.
pub(crate) fn guard_exit() {
    EXIT_GUARD.call_once(|| {
        // SAFETY: `wait_at_exit` is a plain `extern "C" fn()` that never
        // unwinds (it only locks, waits on a condvar and returns), as
        // `atexit` requires. A failed registration only loses the guard.
        let _ = unsafe { libc::atexit(wait_at_exit) };
    });
}
