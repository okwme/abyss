//! Provisioning of the message-cache key (plan section 16).
//!
//! These functions block. After startup they run only on the persistence
//! worker thread (`PersistHandle::load_or_create_cache_key`,
//! `PersistHandle::cache_key_deleter`).

use zeroize::Zeroizing;

use super::{SecretItem, SecretStore, StoreError};
use crate::cache::CacheKey;

const KEY_LEN: usize = 32;

/// Why no usable cache key could be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheKeyError {
    /// The store failed. The cache must not be opened this run (the key may
    /// still exist), but it must not be destroyed either.
    Store(StoreError),
    /// A stored key exists but is not 32 bytes. Its cache is undecryptable:
    /// destroy the cache files, then call [`create_cache_key`].
    Malformed,
    /// The system random number generator failed.
    Random,
}

impl std::fmt::Display for CacheKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "cache key store: {e}"),
            Self::Malformed => f.write_str("stored cache key is malformed"),
            Self::Random => f.write_str("system random number generator failed"),
        }
    }
}

impl std::error::Error for CacheKeyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            Self::Malformed | Self::Random => None,
        }
    }
}

/// Loads the cache key, generating and storing a fresh one on first run.
///
/// A newly generated key is returned only after the store accepted it, so the
/// cache is never encrypted under a key that would be lost at exit.
pub fn load_or_create_cache_key(store: &dyn SecretStore) -> Result<CacheKey, CacheKeyError> {
    match store
        .get(SecretItem::CacheKey)
        .map_err(CacheKeyError::Store)?
    {
        Some(stored) => key_from_stored(&stored),
        None => create_cache_key(store),
    }
}

/// Generates a fresh key and stores it, replacing any existing one. Use after a
/// [`CacheKeyError::Malformed`] once the old cache files are destroyed.
pub fn create_cache_key(store: &dyn SecretStore) -> Result<CacheKey, CacheKeyError> {
    // `generate` fails only when the system RNG does.
    let key = CacheKey::generate().map_err(|_| CacheKeyError::Random)?;
    store
        .set(SecretItem::CacheKey, key.as_bytes())
        .map_err(CacheKeyError::Store)?;
    Ok(key)
}

/// Deletes the stored cache key. Succeeds when no key exists.
pub fn delete_cache_key(store: &dyn SecretStore) -> Result<(), StoreError> {
    store.delete(SecretItem::CacheKey)
}

fn key_from_stored(stored: &[u8]) -> Result<CacheKey, CacheKeyError> {
    if stored.len() != KEY_LEN {
        return Err(CacheKeyError::Malformed);
    }
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    key.copy_from_slice(stored);
    Ok(CacheKey::from_bytes(key))
}
