//! API credentials and onboarding transitions (plan sections 17.1-17.2a).
//!
//! Resolution is atomic per source: env pair, else the Keychain item, else
//! an embedded pair, else onboarding. The Keychain item belongs to the
//! persistence worker; core reads, writes and deletes it through one queue of
//! credential operations, run one at a time in FIFO order. A `Load` is
//! tagged with the [`AttemptId`] it serves (a superseded one is ignored); a
//! `Save` with the credential-slot version it saves (a superseded pair is
//! never written).
//!
//! **Proof (round 11, finding 4).** An entered pair is saved only once a
//! code request of the current attempt succeeded on an instance built for
//! that very slot version: `Creds::proven` records the attempt and the
//! version (round 19, finding 5: a proof of an earlier attempt with the
//! same pair does not carry over, and a queued save of an earlier attempt
//! is skipped like one of an earlier version). Nothing is
//! inferred from the screen, so a flood wait on the first code request, or
//! a login confirmed by `fetch_self` after a new pair was entered, proves
//! nothing. Save and delete failures are explicit ([`CredentialItem`]) with
//! Retry, and a stored pair counts as present until its deletion succeeded.
//!
//! Back and Forget are actor transitions: they bump the attempt, supersede
//! the running login call, clear the tokens and, when they change the pair in
//! use, retire the instance built for it (no backoff; the next build waits
//! for a pair). While a sign-in may have gone through (reconciliation), they
//! only record where to go: the pair is dropped and the item deleted once the
//! account is proven signed out (round 11, finding 2).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use super::Actor;
use super::auth::{AttemptId, AuthMsg, Resume};
use super::lifecycle::Phase;
use crate::api::{
    ApiCredentials, ApiHash, ApiId, AuthState, CredentialError, CredentialItem, CredentialSource,
    Event, StorageError, UserError,
};
use crate::backend::Backend;
use crate::credentials::{self, EnvResolution};

/// Queued credential operations; one of each kind at most.
const MAX_CRED_OPS: usize = 3;

/// A Keychain operation on the credential item.
#[derive(Clone, Debug)]
enum CredOp {
    Load,
    /// The pair and the slot version it is.
    Save(Arc<ApiCredentials>, u64),
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
    Saved {
        version: u64,
        res: Result<(), StorageError>,
    },
    Deleted(Result<(), StorageError>),
}

/// Credential bookkeeping owned by the actor.
#[derive(Debug, Default)]
pub(super) struct Creds {
    /// The source of the pair in use (`None`: no pair).
    source: Option<CredentialSource>,
    /// The attempt and slot version a successful code request proved.
    proven: Option<(AttemptId, u64)>,
    /// The slot version saved to the Keychain.
    saved: Option<u64>,
    /// A save of the entered pair is queued or running.
    saving: bool,
    /// The Keychain holds a pair, as far as core knows. Cleared only by a
    /// successful delete.
    stored: bool,
    /// What the UI's Retry controls act on.
    item: CredentialItem,
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
        EnvResolution::Partial { missing } => {
            Some(CredentialError::EnvPartial { missing: *missing })
        }
        EnvResolution::Malformed { var } => Some(CredentialError::EnvMalformed { var: *var }),
        EnvResolution::Absent | EnvResolution::Pair(_) => None,
    }
}

/// Runs one Keychain operation on the persistence worker.
async fn run_op(persist: crate::session::PersistHandle, op: CredOp) -> CredDone {
    let err = |e: &crate::secret::StoreError| StorageError::from(e);
    match op {
        CredOp::Load => CredDone::Loaded(persist.load_api_credentials().await.map_err(|e| err(&e))),
        CredOp::Save(creds, version) => {
            let blob = credentials::encode(&creds);
            let res = persist
                .store_api_credentials(blob)
                .await
                .map_err(|e| err(&e));
            CredDone::Saved { version, res }
        }
        CredOp::Delete => {
            CredDone::Deleted(persist.delete_api_credentials().await.map_err(|e| err(&e)))
        }
    }
}

impl<B: Backend> Actor<B> {
    /// Startup and `ReloadCredentials`: resolve the pair in use.
    pub(super) async fn resolve_credentials(&mut self) {
        let env = self.backend.credentials().env().clone();
        if let Some(error) = env_error(&env) {
            return self
                .need_credentials(Some(error), Some(CredentialSource::Env))
                .await;
        }
        if let EnvResolution::Pair(creds) = env {
            self.use_credentials(creds, CredentialSource::Env).await;
        } else {
            let attempt = self.auth.attempt();
            self.queue_cred(attempt, CredOp::Load).await;
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

    async fn set_item(&mut self, item: CredentialItem) {
        if self.auth.creds.item != item {
            self.auth.creds.item = item;
            self.emit(Event::CredentialItem(item)).await;
        }
    }

    /// The credential screens. During a logout (a resumed one at startup)
    /// the logout screens stay; `finish_logout` resolves again.
    async fn need_credentials(
        &mut self,
        error: Option<CredentialError>,
        source: Option<CredentialSource>,
    ) {
        self.set_source(source.filter(|_| error.is_some())).await;
        if matches!(self.life.phase, Phase::LoggingOut(_)) {
            return;
        }
        self.set_auth(AuthState::NeedCredentials { error, source })
            .await;
    }

    /// Whether the live instance was built for a pair no longer in use.
    pub(super) fn credentials_superseded(&self) -> bool {
        let slot = self.backend.credentials();
        slot.taken() != slot.version()
    }

    /// Drops the pair in use and the instance built for it. Its proof and
    /// save belong to its slot version, so they lapse with it.
    fn drop_credentials(&mut self) {
        self.backend.credentials().set(None);
        self.auth.creds.saving = false;
        self.retire_signed_out();
    }

    async fn queue_cred(&mut self, attempt: AttemptId, op: CredOp) {
        self.auth.creds.push(attempt, op);
        self.next_cred().await;
    }

    /// Starts the next queued operation unless one runs. A save of a pair
    /// no longer in use, or queued by an earlier attempt, is skipped.
    async fn next_cred(&mut self) {
        if self.auth.creds.running.is_some() {
            return;
        }
        let version = self.backend.credentials().version();
        let current = self.auth.attempt();
        while let Some((attempt, op)) = self.auth.creds.ops.pop_front() {
            if matches!(op, CredOp::Save(_, v) if v != version || attempt != current) {
                self.auth.creds.saving = false;
                if self.auth.creds.item == CredentialItem::Saving {
                    self.set_item(CredentialItem::Settled).await;
                }
                continue;
            }
            let persist = self.persist.clone();
            let tx = self.auth.sender();
            self.auth.creds.running = Some(tokio::spawn(async move {
                let done = run_op(persist, op).await;
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
            CredDone::Saved { version, res } => self.on_saved(version, res).await,
            CredDone::Deleted(res) => self.on_deleted(res).await,
        }
        self.next_cred().await;
    }

    async fn on_saved(&mut self, version: u64, res: Result<(), StorageError>) {
        self.auth.creds.saving = false;
        let in_use = version == self.backend.credentials().version();
        match res {
            Ok(()) => {
                self.auth.creds.stored = true;
                self.auth.creds.saved = Some(version);
                self.set_item(CredentialItem::Settled).await;
            }
            Err(e) if in_use => {
                self.set_item(CredentialItem::SaveFailed(e)).await;
                self.error(UserError::CredentialsNotSaved(e)).await;
            }
            Err(_) => self.set_item(CredentialItem::Settled).await,
        }
    }

    /// Only a successful delete makes the pair absent (round 11, finding 6).
    async fn on_deleted(&mut self, res: Result<(), StorageError>) {
        match res {
            Ok(()) => {
                self.auth.creds.stored = false;
                self.set_item(CredentialItem::Settled).await;
                if let AuthState::NeedCredentials {
                    error,
                    source: Some(CredentialSource::Stored),
                } = self.auth.state
                    && error.is_none_or(CredentialError::allows_entry)
                {
                    self.set_auth(AuthState::NeedCredentials {
                        error,
                        source: None,
                    })
                    .await;
                }
            }
            Err(e) => {
                self.set_item(CredentialItem::DeleteFailed(e)).await;
                self.error(UserError::CredentialsNotForgotten(e)).await;
            }
        }
    }

    async fn on_loaded(&mut self, res: Result<Option<Zeroizing<Vec<u8>>>, StorageError>) {
        let stored = Some(CredentialSource::Stored);
        match res {
            Ok(Some(bytes)) => {
                self.auth.creds.stored = true;
                if let Some(creds) = credentials::decode(&bytes) {
                    self.use_credentials(creds, CredentialSource::Stored).await;
                } else {
                    let error = Some(CredentialError::StoredUnreadable);
                    self.need_credentials(error, stored).await;
                }
            }
            Ok(None) => {
                self.auth.creds.stored = false;
                match credentials::embedded() {
                    Some(creds) => {
                        self.use_credentials(creds, CredentialSource::Embedded)
                            .await;
                    }
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

    /// A code request of the current attempt succeeded: it proves the pair
    /// the instance was built with, if that is still the pair in use.
    pub(super) async fn prove_credentials(&mut self) {
        let slot = self.backend.credentials();
        if slot.taken() == slot.version() {
            self.auth.creds.proven = Some((self.auth.attempt(), slot.version()));
        }
        self.persist_proven().await;
    }

    /// Saves the entered pair in use if this attempt proved it and it is
    /// not saved yet.
    pub(super) async fn persist_proven(&mut self) {
        let version = self.backend.credentials().version();
        let attempt = self.auth.attempt();
        let creds = &self.auth.creds;
        if creds.source != Some(CredentialSource::Entered)
            || creds.proven != Some((attempt, version))
            || creds.saved == Some(version)
            || creds.saving
        {
            return;
        }
        let Some(pair) = self.backend.credentials().current() else {
            return;
        };
        self.auth.creds.saving = true;
        self.set_item(CredentialItem::Saving).await;
        self.queue_cred(attempt, CredOp::Save(pair, version)).await;
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
            AuthState::NeedCredentials { error, .. } => {
                error.is_none_or(CredentialError::allows_entry)
            }
            _ => false,
        };
        if !entry_allowed || self.auth.reconcile.is_some() {
            return self.emit_auth().await;
        }
        let Ok(api_id) = ApiId::new(api_id) else {
            let error = Some(CredentialError::InvalidApiId);
            let source = self.auth.creds.stored.then_some(CredentialSource::Stored);
            return self
                .set_auth(AuthState::NeedCredentials { error, source })
                .await;
        };
        self.auth.bump_attempt();
        self.auth.abort_task();
        self.auth.creds.saving = false;
        if matches!(self.auth.creds.item, CredentialItem::SaveFailed(_)) {
            self.set_item(CredentialItem::Settled).await;
        }
        self.backend
            .credentials()
            .set(Some(ApiCredentials { api_id, api_hash }));
        self.set_source(Some(CredentialSource::Entered)).await;
        self.set_auth(AuthState::Connecting).await;
        match self.life.phase {
            Phase::Halted => self.start_build(Duration::ZERO),
            Phase::SignedOut => self.retire_signed_out(),
            _ => {}
        }
    }

    /// Back (plan section 17.3): login screens go to the phone screen; the
    /// phone screen goes to the credential screens. Only before the session
    /// is authorized.
    pub(super) async fn back(&mut self) {
        // "Stay signed in" after a logout that could not start.
        if self.logout_gate.stay_signed_in() {
            return self.emit_auth().await;
        }
        if self.auth.signed_in || matches!(self.life.phase, Phase::LoggingOut(_)) {
            return self.emit_auth().await;
        }
        match self.auth.state {
            AuthState::Code { .. }
            | AuthState::Password { .. }
            | AuthState::PasswordTokenUnavailable { .. }
            | AuthState::SignUpRequired
            | AuthState::OfficialAppRequired { .. }
            | AuthState::FloodWait { .. } => self.back_to_phone().await,
            AuthState::Phone { .. } | AuthState::LoggedOut { .. } => {
                self.back_to_credentials().await;
            }
            _ => self.emit_auth().await,
        }
    }

    /// A sign-in may have gone through: show `Verifying` until it is settled
    /// (the supervised call's own answer, or the probe).
    async fn await_settlement(&mut self) {
        if !matches!(self.auth.state, AuthState::Verifying { .. }) {
            self.set_auth(AuthState::Verifying { error: None }).await;
        }
        self.start_probe().await;
    }

    async fn back_to_phone(&mut self) {
        if self.auth.reconcile.is_some() {
            // An ambiguous sign-in is unresolved: it is answered first.
            self.auth.record_resume(Resume::Phone);
            return self.emit_auth().await;
        }
        self.auth.bump_attempt();
        self.auth.clear_login();
        if self.auth.supersede(Resume::Phone) {
            return self.await_settlement().await;
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
        if self.auth.reconcile.is_some() {
            self.auth.record_resume(Resume::Credentials);
            return self.emit_auth().await;
        }
        self.auth.bump_attempt();
        self.auth.clear_login();
        if self.auth.supersede(Resume::Credentials) {
            return self.await_settlement().await;
        }
        self.leave_to_credentials().await;
    }

    /// Back to the credential screens: the pair in use is dropped (a stored
    /// one stays offered).
    pub(super) async fn leave_to_credentials(&mut self) {
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
    /// While a sign-in may have gone through, nothing is dropped or deleted
    /// until reconciliation proves the account signed out; signed in, the
    /// forget is refused (`SignOutFirst`).
    pub(super) async fn forget_credentials(&mut self) {
        if self.auth.signed_in || matches!(self.life.phase, Phase::LoggingOut(_)) {
            self.error(UserError::SignOutFirst).await;
            return self.emit_auth().await;
        }
        if !matches!(
            self.auth.creds.source,
            Some(CredentialSource::Stored | CredentialSource::Entered)
        ) {
            // Env and embedded pairs are untouched; only the item goes.
            let attempt = self.auth.attempt();
            self.set_item(CredentialItem::Deleting).await;
            return self.queue_cred(attempt, CredOp::Delete).await;
        }
        self.auth.bump_attempt();
        self.auth.clear_login();
        if self.auth.reconcile.is_some() || self.auth.supersede(Resume::Forget) {
            self.auth.record_resume(Resume::Forget);
            return self.await_settlement().await;
        }
        self.forget_now().await;
    }

    /// The forget itself: delete the item, drop the pair, then the
    /// credential screens. The pair is still shown as stored until the
    /// delete succeeded.
    pub(super) async fn forget_now(&mut self) {
        self.forget_item_and_pair().await;
        let source = self.auth.creds.stored.then_some(CredentialSource::Stored);
        self.need_credentials(None, source).await;
    }

    /// Queues the item's deletion and drops the pair in use.
    pub(super) async fn forget_item_and_pair(&mut self) {
        let attempt = self.auth.attempt();
        self.set_item(CredentialItem::Deleting).await;
        self.queue_cred(attempt, CredOp::Delete).await;
        self.drop_credentials();
    }

    pub(super) async fn reload_credentials(&mut self) {
        if !matches!(self.auth.state, AuthState::NeedCredentials { .. }) {
            return self.emit_auth().await;
        }
        self.auth.bump_attempt();
        self.resolve_credentials().await;
    }

    /// Retry of a failed save. Only a pair a code request of this attempt
    /// proved is saved (never one inferred from the screen); refused
    /// otherwise, which re-sends the item state.
    pub(super) async fn retry_credential_save(&mut self) {
        let before = self.auth.creds.item;
        self.persist_proven().await;
        if self.auth.creds.item == before {
            self.emit(Event::CredentialItem(before)).await;
        }
    }
}
