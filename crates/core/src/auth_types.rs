//! Login, logout and credential types shared by the UI and core (plan
//! sections 4, 5 and 17.2). Re-exported from [`crate::api`].

use std::time::SystemTime;

use zeroize::Zeroize;

use crate::api::UserError;
use crate::cache::EraseFailure;
use crate::secret::{CacheKeyError, StoreError};
use crate::session::PersistError;

/// Capacity of [`PasswordBuf`] in bytes. Matches the app's password field.
pub const PASSWORD_CAPACITY: usize = 256;

/// A 2FA password in a fixed-capacity buffer that never reallocates and is
/// wiped (all [`PASSWORD_CAPACITY`] bytes) on drop (plan section 10).
pub struct PasswordBuf {
    bytes: Box<[u8; PASSWORD_CAPACITY]>,
    len: usize,
}

/// The password does not fit [`PASSWORD_CAPACITY`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasswordTooLong;

impl PasswordBuf {
    /// An empty buffer. The one allocation happens here.
    pub fn new() -> Self {
        Self {
            bytes: Box::new([0; PASSWORD_CAPACITY]),
            len: 0,
        }
    }

    /// Copies `secret` in. The caller wipes its own copy.
    pub fn from_slice(secret: &[u8]) -> Result<Self, PasswordTooLong> {
        let mut buf = Self::new();
        let dst = buf.bytes.get_mut(..secret.len()).ok_or(PasswordTooLong)?;
        dst.copy_from_slice(secret);
        buf.len = secret.len();
        Ok(buf)
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for PasswordBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PasswordBuf {
    fn drop(&mut self) {
        self.bytes.zeroize();
        self.len = 0;
    }
}

impl std::fmt::Debug for PasswordBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordBuf(<redacted>)")
    }
}

/// A validated `api_hash`: exactly 32 hex digits, lowercase. Never logged;
/// `Debug` redacts it (plan section 17.2).
#[derive(Clone, PartialEq, Eq)]
pub struct ApiHash(String);

/// Why a string is not an `api_hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiHashError {
    /// Not exactly 32 characters after trimming.
    Length,
    /// A character other than `0-9a-fA-F`.
    NotHex,
}

impl ApiHash {
    /// Length of an `api_hash` in hex digits.
    pub const LEN: usize = 32;

    /// Trims surrounding whitespace (a paste), checks 32 hex digits, and
    /// normalizes to lowercase.
    pub fn parse(input: &str) -> Result<Self, ApiHashError> {
        let trimmed = input.trim();
        if trimmed.len() != Self::LEN {
            return Err(ApiHashError::Length);
        }
        if !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ApiHashError::NotHex);
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// The hash, for the backend's `request_login_code`. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiHash(<redacted>)")
    }
}

impl Drop for ApiHash {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Telegram rejected the API credentials (plan section 17.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialError {
    /// `API_ID_INVALID`: the id/hash pair is unknown.
    ApiIdInvalid,
    /// `API_ID_PUBLISHED_FLOOD`: the pair was published and is blocked.
    ApiIdPublishedFlood,
}

/// A device-local storage failure, without OS detail (that may be logged
/// where it occurs; it never carries secrets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    /// The Keychain refused or failed the operation.
    Keychain,
    /// A stored item was unreadable.
    Corrupt,
    /// Too many writes queued; this one never reached the Keychain.
    Busy,
    /// The persistence or cache worker stopped.
    Stopped,
    /// The system random number generator failed.
    Random,
    /// The message cache could not be closed; nothing was deleted.
    Cache,
    /// The cache key is gone (the cache is unreadable), but its files remain.
    CacheFiles,
    /// A bug: a poisoned lock or a call from the wrong thread.
    Internal,
}

impl From<&StoreError> for StorageError {
    fn from(e: &StoreError) -> Self {
        match e {
            StoreError::Os { .. } | StoreError::Injected => Self::Keychain,
            StoreError::Malformed => Self::Corrupt,
            StoreError::Stopped => Self::Stopped,
        }
    }
}

impl From<&PersistError> for StorageError {
    fn from(e: &PersistError) -> Self {
        match e {
            PersistError::Store(e) => e.into(),
            PersistError::NotQueued => Self::Busy,
            PersistError::Terminated | PersistError::Stopped => Self::Stopped,
            PersistError::InAsyncContext | PersistError::Poisoned => Self::Internal,
        }
    }
}

impl From<&CacheKeyError> for StorageError {
    fn from(e: &CacheKeyError) -> Self {
        match e {
            CacheKeyError::Store(e) => e.into(),
            CacheKeyError::Malformed => Self::Corrupt,
            CacheKeyError::Random => Self::Random,
        }
    }
}

impl From<&EraseFailure> for StorageError {
    fn from(e: &EraseFailure) -> Self {
        match e {
            EraseFailure::Stopped => Self::Stopped,
            EraseFailure::Close(_) => Self::Cache,
            EraseFailure::DeleteKey(_) => Self::Keychain,
            EraseFailure::Files(_) => Self::CacheFiles,
        }
    }
}

/// `UserError::Storage` for each storage error type.
macro_rules! storage_user_error {
    ($($ty:ty),*) => {$(
        impl From<&$ty> for UserError {
            fn from(e: &$ty) -> Self {
                UserError::Storage(e.into())
            }
        }
    )*};
}

storage_user_error!(StoreError, PersistError, CacheKeyError, EraseFailure);

/// Outcome of the remote half of a logout (`auth.logOut`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteLogout {
    /// The server no longer accepts the session: `auth.logOut` succeeded, or
    /// the server had already revoked it (`AUTH_KEY_UNREGISTERED`,
    /// `SESSION_REVOKED`).
    Revoked,
    /// `auth.logOut` failed; end the session from Active Sessions on another
    /// device.
    Failed(UserError),
    /// `auth.logOut` did not answer within its deadline.
    TimedOut,
    /// There was no remote session to end (a retry of the local deletion
    /// while signed out).
    NotAttempted,
}

/// Outcome of the local half of a logout: the session item, then the cache
/// (crypto-erase).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalDeletion {
    Deleted,
    /// Something may remain on this device. The UI shows "Logout incomplete"
    /// with a retry, never "logged out".
    Failed(StorageError),
}

/// Whether shutdown observed the session saved (plan section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveStatus {
    Confirmed,
    Unconfirmed,
}

/// A logout running or finished when shutdown began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogoutStatus {
    Done {
        remote: RemoteLogout,
        local: LocalDeletion,
    },
    /// Interrupted: the session key may remain in the Keychain.
    Incomplete,
}

/// Login state (plan sections 4 and 17.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthState {
    /// No usable API credentials. `error` says why the last pair failed.
    NeedCredentials {
        error: Option<CredentialError>,
    },
    Connecting,
    Phone {
        error: Option<UserError>,
    },
    Code {
        /// Masked, e.g. `+44 •••• ••12`; see [`mask_phone`].
        phone_hint: String,
        error: Option<UserError>,
    },
    Password {
        hint: Option<String>,
        error: Option<UserError>,
    },
    /// The password token was consumed by a failed attempt. The user must
    /// `RetryPasswordParams` before submitting again.
    PasswordTokenUnavailable {
        error: UserError,
    },
    /// Telegram asked to wait. Core returns to `resume` at `until`.
    FloodWait {
        until: SystemTime,
        resume: Box<AuthState>,
    },
    /// The number has no account; create it in an official app.
    SignUpRequired,
    /// `display_name` is empty until the server has answered once.
    SignedIn {
        display_name: String,
    },
    /// Signed in, but the session could not be saved: the user signs in again
    /// next launch.
    SignedInUnsaved {
        display_name: String,
    },
    LoggingOut,
    LoggedOut {
        remote: RemoteLogout,
        local: LocalDeletion,
    },
}

impl AuthState {
    /// Whether the account is signed in (possibly unsaved).
    pub fn is_signed_in(&self) -> bool {
        matches!(self, Self::SignedIn { .. } | Self::SignedInUnsaved { .. })
    }
}

/// Masks a phone number for display: the first three characters and the last
/// two digits, e.g. `+44 •••• ••12`. Input of five characters or fewer is
/// masked entirely.
pub fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    let n = chars.len();
    if n <= 5 {
        return "•".repeat(n);
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars.iter().skip(n - 2).collect();
    let hidden = n - 5;
    let first = hidden.min(4);
    let rest = hidden - first;
    format!("{head} {} {}{tail}", "•".repeat(first), "•".repeat(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_buf_is_fixed_and_bounded() {
        let pw = PasswordBuf::from_slice(b"hunter2").unwrap();
        assert_eq!(pw.as_bytes(), b"hunter2");
        assert_eq!(pw.len(), 7);
        assert_eq!(format!("{pw:?}"), "PasswordBuf(<redacted>)");
        let full = [b'x'; PASSWORD_CAPACITY];
        assert_eq!(PasswordBuf::from_slice(&full).unwrap().len(), 256);
        let over = [b'x'; PASSWORD_CAPACITY + 1];
        assert_eq!(PasswordBuf::from_slice(&over).unwrap_err(), PasswordTooLong);
        assert!(PasswordBuf::new().is_empty());
    }

    #[test]
    fn api_hash_validation() {
        let ok = format!("  {}\n", "AbCdEf0123456789".repeat(2));
        let hash = ApiHash::parse(&ok).unwrap();
        assert_eq!(hash.expose(), "abcdef0123456789".repeat(2));
        assert_eq!(format!("{hash:?}"), "ApiHash(<redacted>)");
        assert_eq!(ApiHash::parse(&"a".repeat(31)), Err(ApiHashError::Length));
        assert_eq!(ApiHash::parse(&"a".repeat(33)), Err(ApiHashError::Length));
        assert_eq!(ApiHash::parse(""), Err(ApiHashError::Length));
        let bad = format!("{}g", "a".repeat(31));
        assert_eq!(ApiHash::parse(&bad), Err(ApiHashError::NotHex));
        let inner = format!("{} {}", "a".repeat(15), "a".repeat(16));
        assert_eq!(ApiHash::parse(&inner), Err(ApiHashError::NotHex));
    }

    #[test]
    fn masks_phone_numbers() {
        assert_eq!(mask_phone("+442079460012"), "+44 •••• ••••12");
        assert_eq!(mask_phone("+15550100"), "+15 •••• 00");
        assert_eq!(mask_phone("+155501"), "+15 •• 01");
        assert_eq!(mask_phone("12345"), "•••••");
        assert_eq!(mask_phone(""), "");
    }

    #[test]
    fn storage_errors_map_without_detail() {
        let os = StoreError::Os {
            code: -25300,
            message: "x".into(),
        };
        assert_eq!(UserError::from(&os), UserError::Storage(StorageError::Keychain));
        assert_eq!(
            UserError::from(&PersistError::NotQueued),
            UserError::Storage(StorageError::Busy)
        );
        assert_eq!(
            UserError::from(&CacheKeyError::Malformed),
            UserError::Storage(StorageError::Corrupt)
        );
        assert_eq!(
            UserError::from(&EraseFailure::Stopped),
            UserError::Storage(StorageError::Stopped)
        );
    }
}
