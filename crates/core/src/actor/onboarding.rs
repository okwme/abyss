//! API credentials and onboarding transitions (plan sections 17.1-17.2a).
//!
//! Resolution is atomic per source: env pair, else the Keychain item, else
//! an embedded pair, else onboarding. The Keychain item belongs to the
//! persistence worker; core reads, writes and deletes it through one queue of
//! credential operations, run one at a time in FIFO order. Each operation is
//! tagged with the [`AttemptId`] it serves: a `Save` of a superseded attempt
//! never runs, and a `Load` of one is ignored.
//!
//! Back and Forget are actor transitions: they bump the attempt, supersede
//! the running login call, clear the tokens and, when they change the pair in
//! use, retire the instance built for it (no backoff; the next build waits
//! for a pair).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use super::Actor;
use super::auth::{AttemptId, AuthMsg, Resume};
use super::lifecycle::Phase;
use crate::api::{
    ApiCredentials, ApiHash, ApiId, AuthState, CredentialError, CredentialSource, Event,
    StorageError, UserError,
};
use crate::backend::Backend;
use crate::credentials::{self, EnvResolution};

/// Queued credential operations; one of each kind at most.
const MAX_CRED_OPS: usize = 3;

/// A Keychain operation on the credential item.
#[derive(Clone, Debug)]
enum CredOp {
    Load,
    Save(Arc<ApiCredentials>),
    Delete,
}

impl CredOp {
    fn same_kind(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// A finished credential operation.
pub(super) enum CredDone {
    Loaded(Result<Option<Zeroizing<Vec<u8>>>, StorageError>),
    Saved(Result<(), StorageError>),
    Deleted(Result<(), StorageError>),
}

/// Credential bookkeeping owned by the actor.
#[derive(Debug, Default)]
pub(super) struct Creds {
    /// The source of the pair in use (`None`: no pair).
    source: Option<CredentialSource>,
    /// The entered pair in use has reached the Keychain.
    saved: bool,
    /// A save of the entered pair is queued or running.
    saving: bool,
    /// The Keychain holds a pair, as far as core knows.
    stored: bool,
    ops: VecDeque<(AttemptId, CredOp)>,
    running: Option<JoinHandle<()>>,
}

impl Creds {
    fn push(&mut self, attempt: AttemptId, op: CredOp) {
        self.ops.retain(|(_, queued)| !queued.same_kind(&op));
        if self.ops.len() < MAX_CRED_OPS {
            self.ops.push_back((attempt, op));
        }
    }

    /// Shutdown: a running Keychain call is abandoned (the worker finishes
    /// it; nothing waits for the answer).
    pub(super) fn abort(&mut self) {
        self.ops.clear();
        if let Some(handle) = self.running.take() {
            handle.abort();
        }
    }
}

/// `NeedCredentials` for an env resolution that is not a usable pair.
fn env_error(env: &EnvResolution) -> Option<CredentialError> {
    match env {
        EnvResolution::Partial { missing } => Some(CredentialError::EnvPartial { missing: *missing }),
        EnvResolution::Malformed { var } => Some(CredentialError::EnvMalformed { var: *var }),
        EnvResolution::Absent | EnvResolution::Pair(_) => None,
    }
}

impl<B: Backend> Actor<B> {
    /// Startup and `ReloadCredentials`: resolve the pair in use.
    pub(super) async fn resolve_credentials(&mut self) {
        let env = self.backend.credentials().env().clone();
        if let Some(error) = env_error(&env) {
            return self.need_credentials(Some(error), Some(CredentialSource::Env)).await;
        }
        if let EnvResolution::Pair(creds) = env {
            self.use_credentials(creds, CredentialSource::Env).await;
        } else {
            let attempt = self.auth.attempt();
            self.queue_cred(attempt, CredOp::Load);
        }
    }

    /// Puts `creds` in the slot (unless it holds them already, which would
    /// make the instance look superseded) and reports the source.
    async fn use_credentials(&mut self, creds: ApiCredentials, source: CredentialSource) {
        let slot = self.backend.credentials();
        if slot.current().as_deref() != Some(&creds) {
            slot.set(Some(creds));
        }
        self.set_source(Some(source)).await;
        if matches!(self.auth.state, AuthState::NeedCredentials { .. }) {
            self.set_auth(AuthState::Connecting).await;
        }
        if self.life.phase == Phase::Halted {
            self.start_build(Duration::ZERO);
        }
    }

    async fn set_source(&mut self, source: Option<CredentialSource>) {
        self.auth.creds.source = source;
        self.emit(Event::CredentialSource(source)).await;
    }

    async fn need_credentials(
        &mut self,
        error: Option<CredentialError>,
        source: Option<CredentialSource>,
    ) {
        self.set_source(source.filter(|_| error.is_some())).await;
        self.set_auth(AuthState::NeedCredentials { error, source })
            .await;
    }

    /// Whether the live instance was built for a pair no longer in use.
    pub(super) fn credentials_superseded(&self) -> bool {
        let slot = self.backend.credentials();
        slot.taken() != slot.version()
    }

    /// Drops the pair in use and the instance built for it.
    fn drop_credentials(&mut self) {
        self.backend.credentials().set(None);
        self.auth.creds.saved = false;
        self.auth.creds.saving = false;
        self.retire_for_credentials();
    }

    fn queue_cred(&mut self, attempt: AttemptId, op: CredOp) {
        self.auth.creds.push(attempt, op);
        self.next_cred();
    }

    /// Starts the next queued operation unless one runs. A save of a
    /// superseded attempt is skipped.
    fn next_cred(&mut self) {
        if self.auth.creds.running.is_some() {
            return;
        }
        let current = self.auth.attempt();
        while let Some((attempt, op)) = self.auth.creds.ops.pop_front() {
            if matches!(op, CredOp::Save(_)) && attempt != current {
                self.auth.creds.saving = false;
                continue;
            }
            let persist = self.persist.clone();
            let tx = self.auth.sender();
            self.auth.creds.running = Some(tokio::spawn(async move {
                let err = |e: &crate::secret::StoreError| StorageError::from(e);
                let done = match op {
                    CredOp::Load => CredDone::Loaded(persist.load_api_credentials().await.map_err(|e| err(&e))),
                    CredOp::Save(creds) => {
                        let blob = credentials::encode(&creds);
                        CredDone::Saved(persist.store_api_credentials(blob).await.map_err(|e| err(&e)))
                    }
                    CredOp::Delete => CredDone::Deleted(persist.delete_api_credentials().await.map_err(|e| err(&e))),
                };
                let _ = tx.send(AuthMsg::Cred { attempt, done }).await;
            }));
            return;
        }
    }

    pub(super) async fn on_cred(&mut self, attempt: AttemptId, done: CredDone) {
        self.auth.creds.running = None;
        let current = attempt == self.auth.attempt();
        match done {
            CredDone::Loaded(res) if current => self.on_loaded(res).await,
            // A superseded load: the user moved on meanwhile.
            CredDone::Loaded(_) => {}
            CredDone::Saved(res) => {
                self.auth.creds.saving = false;
                match res {
                    Ok(()) => {
                        self.auth.creds.stored = true;
                        self.auth.creds.saved |= current;
                    }
                    Err(e) if current => self.error(UserError::CredentialsNotSaved(e)).await,
                    Err(_) => {}
                }
            }
            CredDone::Deleted(Ok(())) => self.auth.creds.stored = false,
            CredDone::Deleted(Err(e)) => self.error(UserError::CredentialsNotForgotten(e)).await,
        }
        self.next_cred();
    }

    async fn on_loaded(&mut self, res: Result<Option<Zeroizing<Vec<u8>>>, StorageError>) {
        let stored = Some(CredentialSource::Stored);
        match res {
            Ok(Some(bytes)) => {
                self.auth.creds.stored = true;
                if let Some(creds) = credentials::decode(&bytes) {
                    self.auth.creds.saved = true;
                    self.use_credentials(creds, CredentialSource::Stored).await;
                } else {
                    let error = Some(CredentialError::StoredUnreadable);
                    self.need_credentials(error, stored).await;
                }
            }
            Ok(None) => {
                self.auth.creds.stored = false;
                match credentials::embedded() {
                    Some(creds) => self.use_credentials(creds, CredentialSource::Embedded).await,
                    None => self.need_credentials(None, None).await,
                }
            }
            // Locked or denied: not "absent". Retry, never re-onboard.
            Err(_) => {
                let error = Some(CredentialError::KeychainUnavailable);
                self.need_credentials(error, stored).await;
            }
        }
    }

    /// A successful login step proved the pair: persist an entered one.
    pub(super) fn credentials_proven(&mut self) {
        let creds = &self.auth.creds;
        if creds.source != Some(CredentialSource::Entered) || creds.saved || creds.saving {
            return;
        }
        let Some(pair) = self.backend.credentials().current() else {
            return;
        };
        self.auth.creds.saving = true;
        let attempt = self.auth.attempt();
        self.queue_cred(attempt, CredOp::Save(pair));
    }

    /// `API_ID_INVALID` / `API_ID_PUBLISHED_FLOOD`: back to the credential
    /// screen; the session is untouched, the instance is retired.
    pub(super) async fn credentials_rejected(&mut self, error: CredentialError) {
        let source = self.auth.creds.source;
        self.auth.clear_login();
        self.auth.bump_attempt();
        self.drop_credentials();
        self.need_credentials(Some(error), source).await;
    }

    pub(super) async fn submit_credentials(&mut self, api_id: i32, api_hash: ApiHash) {
        let env = self.backend.credentials().env().clone();
        if env != EnvResolution::Absent {
            // The env override is in control: entering a pair is disabled.
            return self.emit_auth().await;
        }
        let entry_allowed = match &self.auth.state {
            AuthState::NeedCredentials { error, .. } => error.is_none_or(CredentialError::allows_entry),
            _ => false,
        };
        if !entry_allowed || self.auth.reconcile.is_some() {
            return self.emit_auth().await;
        }
        let Ok(api_id) = ApiId::new(api_id) else {
            let error = Some(CredentialError::InvalidApiId);
            let source = self.auth.creds.stored.then_some(CredentialSource::Stored);
            return self.set_auth(AuthState::NeedCredentials { error, source }).await;
        };
        self.auth.bump_attempt();
        self.auth.abort_task();
        self.auth.creds.saved = false;
        self.auth.creds.saving = false;
        self.backend
            .credentials()
            .set(Some(ApiCredentials { api_id, api_hash }));
        self.set_source(Some(CredentialSource::Entered)).await;
        self.set_auth(AuthState::Connecting).await;
        match self.life.phase {
            Phase::Halted => self.start_build(Duration::ZERO),
            Phase::SignedOut => self.retire_for_credentials(),
            _ => {}
        }
    }

    /// Back (plan section 17.3): login screens go to the phone screen; the
    /// phone screen goes to the credential screens. Only before the session
    /// is authorized.
    pub(super) async fn back(&mut self) {
        if self.auth.signed_in || matches!(self.life.phase, Phase::LoggingOut(_)) {
            return self.emit_auth().await;
        }
        match self.auth.state {
            AuthState::Code { .. }
            | AuthState::Password { .. }
            | AuthState::PasswordTokenUnavailable { .. }
            | AuthState::SignUpRequired
            | AuthState::OfficialAppRequired
            | AuthState::FloodWait { .. } => self.back_to_phone().await,
            AuthState::Phone { .. } | AuthState::LoggedOut { .. } => {
                self.back_to_credentials().await;
            }
            _ => self.emit_auth().await,
        }
    }

    async fn back_to_phone(&mut self) {
        if self.auth.reconcile.is_some() {
            // An ambiguous sign-in is unresolved: it is answered first.
            self.auth.reconcile = Some(Resume::Phone);
            return self.emit_auth().await;
        }
        self.auth.bump_attempt();
        self.auth.supersede(Resume::Phone);
        self.auth.clear_login();
        if self.auth.reconcile.is_some() {
            return self.start_probe().await;
        }
        self.set_auth(AuthState::Phone { error: None }).await;
    }

    async fn back_to_credentials(&mut self) {
        if matches!(
            self.auth.creds.source,
            Some(CredentialSource::Env | CredentialSource::Embedded)
        ) {
            // Not changeable here (17.2a): the screen says so.
            return self.emit_auth().await;
        }
        self.auth.bump_attempt();
        self.auth.supersede(Resume::Phone);
        self.auth.clear_login();
        if self.auth.reconcile.is_some() {
            return self.start_probe().await;
        }
        self.drop_credentials();
        let source = self.auth.creds.stored.then_some(CredentialSource::Stored);
        self.set_source(None).await;
        self.set_auth(AuthState::NeedCredentials {
            error: None,
            source,
        })
        .await;
    }

    /// Deletes the stored pair; only while logged out, never an env pair.
    pub(super) async fn forget_credentials(&mut self) {
        if self.auth.signed_in || matches!(self.life.phase, Phase::LoggingOut(_)) {
            self.error(UserError::SignOutFirst).await;
            return self.emit_auth().await;
        }
        if self.auth.reconcile.is_some() {
            return self.emit_auth().await;
        }
        let attempt = self.auth.bump_attempt();
        self.queue_cred(attempt, CredOp::Delete);
        self.auth.creds.stored = false;
        if !matches!(
            self.auth.creds.source,
            Some(CredentialSource::Stored | CredentialSource::Entered)
        ) {
            // Env and embedded pairs are untouched; only the item goes.
            if let AuthState::NeedCredentials { error, .. } = self.auth.state {
                let source = self.auth.creds.source.filter(|_| error.is_some());
                self.set_auth(AuthState::NeedCredentials { error, source }).await;
            }
            return;
        }
        self.auth.supersede(Resume::Phone);
        self.auth.clear_login();
        self.drop_credentials();
        self.need_credentials(None, None).await;
    }

    pub(super) async fn reload_credentials(&mut self) {
        if !matches!(self.auth.state, AuthState::NeedCredentials { .. }) {
            return self.emit_auth().await;
        }
        self.auth.bump_attempt();
        self.resolve_credentials().await;
    }

    pub(super) fn retry_credential_save(&mut self) {
        let creds = &self.auth.creds;
        if creds.source != Some(CredentialSource::Entered) || creds.saved || creds.saving {
            return;
        }
        // Only a pair a login step has proven is saved.
        if matches!(
            self.auth.state,
            AuthState::NeedCredentials { .. } | AuthState::Connecting | AuthState::Phone { .. }
        ) {
            return;
        }
        self.credentials_proven();
    }
}
