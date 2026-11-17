//! Where the encrypted message cache lives (plan section 16, task 9b).
//!
//! - **Real backend:** `$HOME/Library/Application Support/dev.abyss/cache`.
//!   `dev.abyss` is the app id, the same namespace as the Keychain items
//!   (`dev.abyss.session`, `dev.abyss.cachekey`). The home directory comes
//!   from `HOME`, without a new dependency (no `dirs`, no `getpwuid_r`): it
//!   must be set and absolute, and `Library/Application Support` under it
//!   must already exist as a directory. A missing or unusable `HOME` means no
//!   cache this run, never a guess.
//! - **`--fake`:** no cache, unless `ABYSS_FAKE_CACHE=1`. Then the cache is
//!   `$TMPDIR/dev.abyss.fake/cache`. Its last two components differ from the
//!   real ones, so a fake run can never open, re-key or destroy the real
//!   cache, whatever `TMPDIR` says. The fake keeps its key in memory, so each
//!   run finds the previous run's database under a key it does not have and
//!   destroys and recreates it: what this exercises is open, bind, writes,
//!   the recreate path and the exit drain (`cache::census`) under the real
//!   GPUI quit, not persistence across runs (that is tested in core,
//!   `cached_history_survives_a_core_restart`).
//!
//! [`prepare`] creates the components below the root one at a time with mode
//! 0700, refuses a component that is a symlink, is not a directory or has
//! another owner than the root, and narrows a wider mode to 0700. Core's
//! `Cache::open` creates the leaf again after a logout's erase, with the same
//! mode and symlink refusal. A failure is logged and the run continues
//! without a cache; it never blocks login. Paths are never logged (they hold
//! the user name, plan section 11).

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use abyss_core::CacheConfig;

/// The app id: the directory in Application Support.
const APP_ID: &str = "dev.abyss";
/// The fake's directory under `TMPDIR`; never [`APP_ID`].
#[cfg(feature = "fake")]
const FAKE_APP_ID: &str = "dev.abyss.fake";
/// The cache directory inside the app's.
const CACHE_DIR: &str = "cache";
/// `HOME`, the user's home directory.
pub(crate) const HOME_VAR: &str = "HOME";
/// `TMPDIR`, the per-user temporary directory macOS sets.
#[cfg(feature = "fake")]
pub(crate) const TMPDIR_VAR: &str = "TMPDIR";
/// Opt-in for a cache in `--fake` runs; only the value `1` enables it.
#[cfg(feature = "fake")]
pub(crate) const FAKE_CACHE_VAR: &str = "ABYSS_FAKE_CACHE";
/// The private mode of every directory we create.
const PRIVATE: u32 = 0o700;

/// Where a cache goes: an existing root, and the directories created below
/// it (app id, then cache).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CacheLocation {
    root: PathBuf,
    app: &'static str,
}

impl CacheLocation {
    /// The real backend's cache, under `home`.
    pub(crate) fn real(home: Option<OsString>) -> Result<Self, StorageError> {
        let home = absolute(home).ok_or(StorageError::NoHome)?;
        Ok(Self {
            root: home.join("Library").join("Application Support"),
            app: APP_ID,
        })
    }

    /// The fake's cache: `None` unless `enabled` is exactly `1`, or without
    /// an absolute `tmpdir`.
    #[cfg(feature = "fake")]
    pub(crate) fn fake(
        enabled: Option<&std::ffi::OsStr>,
        tmpdir: Option<OsString>,
    ) -> Option<Self> {
        if enabled != Some(std::ffi::OsStr::new("1")) {
            return None;
        }
        let Some(root) = absolute(tmpdir) else {
            tracing::warn!("{FAKE_CACHE_VAR} is set but TMPDIR is not absolute; no cache");
            return None;
        };
        Some(Self {
            root,
            app: FAKE_APP_ID,
        })
    }

    /// The cache directory this location names.
    pub(crate) fn dir(&self) -> PathBuf {
        self.root.join(self.app).join(CACHE_DIR)
    }
}

/// An absolute path from an environment value, or `None`.
fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    path.is_absolute().then_some(path)
}

/// Why there is no cache this run. Names a component, never a path.
#[derive(Debug)]
pub(crate) enum StorageError {
    /// `HOME` is unset or not absolute.
    NoHome,
    /// The root is missing or not a directory.
    Root(io::Error),
    /// A component is a symlink.
    Symlink(&'static str),
    /// A component exists but is not a directory.
    NotADirectory(&'static str),
    /// A component is owned by someone other than the root's owner.
    Owner(&'static str),
    /// Creating, inspecting or narrowing a component failed.
    Io(&'static str, io::Error),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => f.write_str("HOME is unset or not absolute"),
            Self::Root(e) => write!(f, "the cache's parent directory is unusable: {e}"),
            Self::Symlink(c) => write!(f, "{c:?} is a symlink"),
            Self::NotADirectory(c) => write!(f, "{c:?} is not a directory"),
            Self::Owner(c) => write!(f, "{c:?} has another owner"),
            Self::Io(c, e) => write!(f, "{c:?}: {e}"),
        }
    }
}

/// Creates the location's directories (0700, no symlinks) and returns the
/// cache directory.
pub(crate) fn prepare(location: &CacheLocation) -> Result<PathBuf, StorageError> {
    let root = std::fs::metadata(&location.root).map_err(StorageError::Root)?;
    if !root.is_dir() {
        return Err(StorageError::Root(io::Error::from(
            io::ErrorKind::NotADirectory,
        )));
    }
    let app = location.root.join(location.app);
    private_dir(&app, location.app, root.uid())?;
    let cache = location.dir();
    private_dir(&cache, CACHE_DIR, root.uid())?;
    Ok(cache)
}

/// Creates `path` with mode 0700 if missing, then checks it is a directory
/// (not a symlink to one) owned by `owner`, and narrows its mode to 0700.
fn private_dir(path: &Path, name: &'static str, owner: u32) -> Result<(), StorageError> {
    match std::fs::DirBuilder::new().mode(PRIVATE).create(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
            return Err(StorageError::Io(name, e));
        }
        _ => {}
    }
    // `symlink_metadata` does not follow a symlink in the last component.
    let meta = std::fs::symlink_metadata(path).map_err(|e| StorageError::Io(name, e))?;
    if meta.file_type().is_symlink() {
        return Err(StorageError::Symlink(name));
    }
    if !meta.is_dir() {
        return Err(StorageError::NotADirectory(name));
    }
    if meta.uid() != owner {
        return Err(StorageError::Owner(name));
    }
    if meta.permissions().mode() & 0o777 != PRIVATE {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE))
            .map_err(|e| StorageError::Io(name, e))?;
    }
    Ok(())
}

/// The core cache config for a location, or `None` (logged): the run then
/// goes on without a disk cache.
pub(crate) fn config(location: Result<CacheLocation, StorageError>) -> Option<CacheConfig> {
    match location.and_then(|l| prepare(&l)) {
        Ok(dir) => {
            tracing::info!("disk cache enabled");
            Some(CacheConfig { dir })
        }
        Err(error) => {
            tracing::warn!(%error, "no disk cache this run");
            None
        }
    }
}

#[cfg(test)]
mod tests;
