//! Device-local secret storage (plan sections 3 and 16).
//!
//! Two items exist: the session blob (home DC, DC auth keys, self user) and the
//! 256-bit cache key. Both live in the macOS Keychain in production
//! ([`KeychainStore`]); tests use [`MemoryStore`] and [`FaultyStore`].
//!
//! Every [`SecretStore`] call is **blocking**. After startup the only caller is
//! the persistence worker thread (`crate::session::PersistHandle`), so no
//! Keychain call ever runs on an async runtime thread.

mod cache_key;
#[cfg(target_os = "macos")]
mod keychain;
#[cfg(any(test, feature = "fake"))]
mod memory;
#[cfg(test)]
mod tests;

use zeroize::Zeroizing;

pub use cache_key::{CacheKeyError, create_cache_key, delete_cache_key, load_or_create_cache_key};
#[cfg(target_os = "macos")]
pub use keychain::{KeychainLocation, KeychainStore};
#[cfg(any(test, feature = "fake"))]
pub use memory::{FaultControl, FaultyStore, MemoryStore, StoreOp};

/// Which secret an operation targets. Each maps to one Keychain item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretItem {
    /// Versioned postcard blob of the critical session state.
    Session,
    /// Raw 32-byte SQLCipher key of the message cache.
    CacheKey,
}

impl SecretItem {
    /// Suffix appended to the store's namespace to form the Keychain service.
    pub(crate) const fn suffix(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::CacheKey => "cachekey",
        }
    }
}

/// Error from a secret store. Never carries secret material; the message is the
/// OS status description at most.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The store refused or failed the operation (Keychain `OSStatus`).
    Os { code: i32, message: String },
    /// The stored value has an unexpected type or shape.
    Malformed,
    /// Injected by `FaultyStore`.
    Injected,
    /// The persistence worker is gone (its thread exited or the runtime is
    /// shutting down).
    Stopped,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Os { code, message } => write!(f, "keychain error {code}: {message}"),
            Self::Malformed => f.write_str("stored secret is malformed"),
            Self::Injected => f.write_str("injected store failure"),
            Self::Stopped => f.write_str("persistence worker stopped"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Blocking key-value store for device-local secrets.
///
/// Contract:
/// - `get` returns `Ok(None)` when the item does not exist.
/// - `set` creates or replaces the item atomically (a failed `set` leaves the
///   previous value, if any, in place).
/// - `delete` succeeds when the item does not exist, so a retried logout is
///   idempotent.
/// - Returned secrets are in zeroizing buffers; implementations must not keep
///   unwiped copies of their own.
pub trait SecretStore: Send + 'static {
    fn get(&self, item: SecretItem) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError>;
    fn set(&self, item: SecretItem, secret: &[u8]) -> Result<(), StoreError>;
    fn delete(&self, item: SecretItem) -> Result<(), StoreError>;
}
