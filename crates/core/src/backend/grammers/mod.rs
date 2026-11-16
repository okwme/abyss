//! The grammers (`MTProto`) backend (plan sections 2–9, task 11).
//!
//! One build is one client instance: a fresh [`AbyssSession`] over the
//! account's critical cell, a `SenderPool` with `NoRetries` and a 1,024-update
//! channel, a `Client`, and an update stream with `catch_up: false`. Nothing is
//! spawned here: core spawns and supervises the runner and the update consumer,
//! so a panic in grammers (known sites: plan section 1) is contained by core's
//! `JoinHandle`s and becomes a rebuild or `Error(Internal)`. The mappers in
//! [`map`] are panic-free by construction (the panicking lints are denied).
//!
//! Logging: grammers' own events are dropped by the app's subscriber
//! (`grammers*` filtered out, plan section 11). This module logs only RPC
//! error names and codes, counts and ids, never a TL value: no `Debug` of a
//! request, response, peer or update, which could carry text or a phone.
//!
//! # TODO(task 6b): auth
//!
//! Task 6 added the auth methods to [`Client`] and the state machine in core.
//! Here they are stubs: every one answers `Rejected` (so a stored session
//! still starts as signed in, and a login attempt fails visibly). Over a built
//! instance they map onto grammers as follows (plan section 4):
//! - `is_authorized` → `Client::is_authorized` (`auth.rs:105-111`).
//! - `request_login_code(phone)` → `Client::request_login_code(phone, api_hash)`
//!   (handles `303` itself); needs the `api_hash`, which this backend does not
//!   hold yet (only `api_id`).
//! - `sign_in(token, code)` → `Client::sign_in`; `PasswordRequired`,
//!   `InvalidCode` (includes `PHONE_CODE_EXPIRED`), `SignUpRequired`.
//! - `check_password(token, pw)` → `Client::check_password` with the password
//!   bytes as a slice; contains the `current_algo.clone().unwrap()` panic site.
//! - `refetch_password_token` → raw `account.getPassword` +
//!   `PasswordToken::new`.
//! - `fetch_self` (ambiguous completion) → raw `users.getUsers([inputUserSelf])`,
//!   then `session.cache_peer(is_self: Some(true))` and `session.barrier()`.
//! - `sign_out` → `Client::sign_out` with the runner alive.
//! - Auth calls use [`invoke::RetryClass::Never`]; their RPC errors need typed
//!   mapping (`PHONE_NUMBER_INVALID`, `PHONE_NUMBER_BANNED`, `API_ID_INVALID`,
//!   `API_ID_PUBLISHED_FLOOD`) beyond [`BackendError`].
//! - `BackendError::Unauthorized` (`AUTH_KEY_UNREGISTERED`, `SESSION_REVOKED`,
//!   any `401`) must run the local part of logout in core; today core maps it
//!   to `UserError::Rejected`.
//! - Until then, an instance built from a logged-out session never reports a
//!   baseline (grammers skips `getState` without a self user), so core would
//!   rebuild it every 10 s: core must not bootstrap while logged out.

mod calls;
mod errors;
mod invoke;
pub(crate) mod map;
mod pins;
mod stream;
#[cfg(test)]
mod tests;

use std::future::{Future, ready};
use std::num::NonZeroUsize;
use std::sync::Arc;

use grammers_client::client::ClientConfiguration;
use grammers_client::sender::{
    ConnectionParams, NoRetries, SenderPool, SenderPoolHandle, UpdatesConfiguration,
};

pub use stream::GrammersUpdates;

use super::{
    Account, AuthError, Backend, BackendError, CheckPassword, Client, DialogsCursor, DialogsPage,
    HistoryPage, HistoryQuery, Instance, InstanceControl, PasswordChallenge, Seed, SessionSeed,
    SessionSnapshot, SignIn,
};
use crate::api::PasswordBuf;
use crate::model::{ChatId, MsgId};
use crate::session::{AbyssSession, PersistHandle};
use calls::Calls;
use pins::Pins;

/// Update channel between the runner and the stream (plan section 2; grammers'
/// default is 100).
pub const UPDATES_CHANNEL_CAPACITY: NonZeroUsize = match NonZeroUsize::new(1024) {
    Some(n) => n,
    None => NonZeroUsize::MIN,
};

fn connection_params() -> ConnectionParams {
    ConnectionParams {
        // Every retry is decided by the adapter's classes (`invoke`).
        retry_policy: Box::new(NoRetries),
        updates_channel_capacity: UPDATES_CHANNEL_CAPACITY,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        ..ConnectionParams::default()
    }
}

/// Builds grammers client instances for one Telegram application id.
pub struct GrammersBackend {
    api_id: i32,
    /// Loads the persisted session for `SessionSeed::Stored` when core has
    /// not provided the account (`use_account`).
    persist: PersistHandle,
    /// The account core loaded at startup or created after a logout.
    account: std::sync::Mutex<Option<SessionSnapshot>>,
}

impl std::fmt::Debug for GrammersBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrammersBackend").finish_non_exhaustive()
    }
}

impl GrammersBackend {
    pub fn new(api_id: i32, persist: PersistHandle) -> Self {
        Self {
            api_id,
            persist,
            account: std::sync::Mutex::new(None),
        }
    }

    fn account(&self) -> Option<SessionSnapshot> {
        self.account
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn snapshot(&self, seed: SessionSeed) -> SessionSnapshot {
        match seed {
            SessionSeed::Live(snapshot) => snapshot,
            SessionSeed::Stored => {
                if let Some(account) = self.account() {
                    return account;
                }
                if let Ok(loaded) = self.persist.load_session().await {
                    loaded.snapshot
                } else {
                    // Unreadable store: start logged out (task 5 wiring, step 1).
                    tracing::warn!("session store unreadable; starting signed out");
                    self.persist.fresh_session()
                }
            }
        }
    }
}

impl Backend for GrammersBackend {
    type Client = GrammersClient;
    type Updates = GrammersUpdates;
    type Control = GrammersControl;

    fn use_account(&self, account: SessionSnapshot) {
        *self
            .account
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(account);
    }

    async fn build(&self, seed: Seed) -> Result<Instance<Self>, BackendError> {
        let snapshot = self.snapshot(seed.session).await;
        // The retired instance's pins (its dialogs, open chats and sends)
        // stay pinned here until newer dialogs push them out.
        let pins = Pins::new(
            seed.peers.as_slice(),
            snapshot.carried.iter().filter_map(map::info_chat),
        );
        let session =
            Arc::new(AbyssSession::from_snapshot(snapshot).map_err(|_| BackendError::Transient)?);
        let SenderPool {
            runner,
            handle,
            updates,
        } = SenderPool::with_configuration(Arc::clone(&session), self.api_id, connection_params());
        let client = grammers_client::Client::with_configuration(
            handle.clone(),
            ClientConfiguration {
                auto_cache_peers: true,
            },
        );
        let stream = client
            .stream_updates(updates, UpdatesConfiguration { catch_up: false })
            .await
            .map_err(|_| BackendError::Transient)?;
        Ok(Instance {
            client: GrammersClient(Calls::new(client, Arc::clone(&session), pins)),
            updates: GrammersUpdates::new(stream, session.baseline_established()),
            runner: Box::pin(runner.run()),
            control: GrammersControl {
                handle: handle.thin,
                session,
            },
        })
    }
}

/// Calls of one grammers instance.
#[derive(Debug)]
pub struct GrammersClient(Calls<grammers_client::Client>);

/// Stub login token (task 6b: grammers `LoginToken` plus the full `SentCode`).
#[derive(Debug)]
pub struct GrammersLoginToken(());

/// Stub password token (task 6b: grammers `PasswordToken`).
#[derive(Debug)]
pub struct GrammersPasswordToken(());

/// Every auth call until task 6b.
const AUTH_STUB: AuthError = AuthError::Backend(BackendError::Rejected);

impl Client for GrammersClient {
    type LoginToken = GrammersLoginToken;
    type PasswordToken = GrammersPasswordToken;

    fn is_authorized(&self) -> impl Future<Output = Result<bool, BackendError>> + Send {
        ready(Err(BackendError::Rejected))
    }

    fn request_login_code(
        &self,
        _phone: &str,
    ) -> impl Future<Output = Result<GrammersLoginToken, AuthError>> + Send {
        ready(Err(AUTH_STUB))
    }

    fn sign_in(
        &self,
        _token: &GrammersLoginToken,
        _code: &str,
    ) -> impl Future<Output = Result<SignIn<GrammersPasswordToken>, AuthError>> + Send {
        ready(Err(AUTH_STUB))
    }

    fn check_password(
        &self,
        _token: GrammersPasswordToken,
        password: PasswordBuf,
    ) -> impl Future<Output = Result<CheckPassword<GrammersPasswordToken>, AuthError>> + Send {
        drop(password);
        ready(Err(AUTH_STUB))
    }

    fn password_challenge(
        &self,
    ) -> impl Future<Output = Result<PasswordChallenge<GrammersPasswordToken>, AuthError>> + Send
    {
        ready(Err(AUTH_STUB))
    }

    fn fetch_self(&self) -> impl Future<Output = Result<Account, BackendError>> + Send {
        ready(Err(BackendError::Rejected))
    }

    fn sign_out(&self) -> impl Future<Output = Result<(), BackendError>> + Send {
        ready(Err(BackendError::Rejected))
    }

    async fn dialogs(
        &self,
        cursor: Option<DialogsCursor>,
        limit: u32,
    ) -> Result<DialogsPage, BackendError> {
        self.0.dialogs(cursor, limit).await
    }

    async fn history(
        &self,
        chat: ChatId,
        query: HistoryQuery,
        limit: u32,
    ) -> Result<HistoryPage, BackendError> {
        self.0.history(chat, query, limit).await
    }

    async fn send_text(
        &self,
        chat: ChatId,
        text: String,
        random_id: i64,
    ) -> Result<MsgId, BackendError> {
        self.0.send_text(chat, text, random_id).await
    }

    async fn read_history(&self, chat: ChatId, max_id: MsgId) -> Result<(), BackendError> {
        self.0.read_history(chat, max_id).await
    }
}

/// Retirement hooks of one grammers instance.
pub struct GrammersControl {
    handle: SenderPoolHandle,
    session: Arc<AbyssSession>,
}

impl std::fmt::Debug for GrammersControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrammersControl").finish_non_exhaustive()
    }
}

impl InstanceControl for GrammersControl {
    fn quit(&self) {
        // `false` means the runner is already gone, which is what we want.
        let _ = self.handle.quit();
    }

    fn retire(self) -> SessionSnapshot {
        // Revoke before snapshotting (plan section 2, rebuild steps 4–5): a
        // late write through this instance must not reach the next one.
        if self.session.revoke().is_err() {
            tracing::warn!("retired session was poisoned");
        }
        self.session.snapshot()
    }
}
