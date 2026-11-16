//! Login state machine (plan sections 4 and 17.2a).
//!
//! Login calls run one at a time on the signed-out instance. Each one is an
//! admitted task (an [`Admission`](super::admission::Admission) permit, the
//! call deadline) spawned into the instance's own task set, so retirement
//! cancels and joins it with the instance's other calls, and a timed-out call
//! parks its permit like any other (stall detection included). A task is
//! tagged with an [`AuthTag`]: the [`AttemptId`] it serves and the
//! [`ClientGen`] it runs on. Its result is applied only while that tag is the
//! running one; anything else is stale and dropped (a dropped password token
//! is consumed with it).
//!
//! **Ambiguous completions.** A `sign_in`/`check_password` that timed out,
//! failed in transport, was superseded (Back) or lost with its instance may
//! have signed in. Core then *reconciles*: [`AuthState::Verifying`] until the
//! `is_authorized` probe gives a verified answer. "Not authorized" returns to
//! the step; "authorized" completes the login; an inconclusive probe (a
//! network failure, a flood wait) is retried with backoff and never read as
//! signed out. No other login call starts while reconciliation is pending.
//!
//! **Revocation.** `AUTH_KEY_UNREGISTERED`/`SESSION_REVOKED` from a login call
//! tears the local session down (signed in or not, plan section 5). That is
//! distinct from the probe's `is_authorized() == false`, which only means the
//! login did not go through.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::Actor;
use super::apply::user_error;
use super::jobs::{CallError, Done, Outcome, TaskKind};
use super::lifecycle::{Phase, Reason};
use super::onboarding::{CredDone, Creds};
use crate::api::{
    AuthState, ClientGen, Event, NewLogin, PasswordBuf, SaveStatus, SentCodeInfo, UserError,
    mask_phone,
};
use crate::backend::{
    Account, AuthError, Backend, BackendError, CheckPassword, Client, CodeRequest,
    PasswordChallenge, SentCode, SignIn,
};
use crate::session::SessionSnapshot;

/// How long a login waits for the session to reach the Keychain before
/// reporting `SignedInUnsaved`.
pub(super) const LOGIN_BARRIER_WAIT: Duration = Duration::from_secs(10);
/// Phone numbers: E.164 allows at most 15 digits.
const PHONE_DIGITS: std::ops::RangeInclusive<usize> = 5..=15;
const MAX_CODE_LEN: usize = 16;
/// Results in flight between login tasks and the actor. Every task holds an
/// admission permit until it has sent, so this never fills.
const AUTH_QUEUE: usize = 2 * super::MAX_CONCURRENT;
/// Retry delays of an inconclusive probe.
const PROBE_RETRY_MIN: Duration = Duration::from_secs(2);
const PROBE_RETRY_MAX: Duration = Duration::from_secs(60);
/// A resend timeout longer than this is treated as this (bounded countdowns).
const MAX_RESEND_WAIT: Duration = Duration::from_hours(24);

/// One login attempt: bumped by a new phone number, new credentials, Back
/// and Forget (plan section 17.2a).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AttemptId(u64);

/// What a login task belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AuthTag {
    pub(super) attempt: AttemptId,
    pub(super) cgen: ClientGen,
}

/// Which login call a task runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AuthKind {
    /// Startup check of a stored session (the self user).
    Startup,
    RequestCode,
    ResendCode,
    SignIn,
    Password,
    Challenge,
    /// Reconciling an ambiguous `sign_in` / `check_password`.
    Probe,
}

impl AuthKind {
    /// A superseded or lost call of this kind may have signed in.
    fn may_sign_in(self) -> bool {
        matches!(self, Self::SignIn | Self::Password | Self::Probe)
    }
}

/// Where a reconciliation returns when the probe says "not signed in".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resume {
    Phone,
    Code,
    Password,
}

/// Why a login call has no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AuthFail {
    Auth(AuthError),
    Timeout,
    /// The instance was retired, or the attempt superseded, during the call.
    Dropped,
}

impl AuthFail {
    pub(super) fn user_error(self) -> UserError {
        match self {
            Self::Auth(AuthError::Backend(e)) => user_error(CallError::Backend(e)),
            Self::Auth(AuthError::PhoneInvalid) => UserError::PhoneInvalid,
            Self::Auth(AuthError::PhoneBanned) => UserError::PhoneBanned,
            Self::Auth(AuthError::PhoneFlood) => UserError::PhoneFlood,
            Self::Auth(AuthError::Credentials(e)) => UserError::Credentials(e),
            Self::Auth(AuthError::OfficialAppRequired) => UserError::Unsupported,
            Self::Timeout | Self::Dropped => UserError::Network,
        }
    }

    /// The server may have applied the call.
    pub(super) fn is_ambiguous(self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Dropped | Self::Auth(AuthError::Backend(BackendError::Transient))
        )
    }

    /// `AUTH_KEY_UNREGISTERED` / `SESSION_REVOKED`: the key is dead.
    pub(super) fn is_revoked(self) -> bool {
        self == Self::Auth(AuthError::Backend(BackendError::Unauthorized))
    }

    pub(super) fn flood_seconds(self) -> Option<u32> {
        match self {
            Self::Auth(AuthError::Backend(BackendError::FloodWait { seconds })) => Some(seconds),
            _ => None,
        }
    }
}

type AuthResult<T> = Result<T, AuthFail>;

/// `sign_in` outcomes other than success (which the task confirms itself).
pub(super) enum SignInNext<P> {
    Password(PasswordChallenge<P>),
    InvalidCode,
    SignUpRequired,
}

/// What the probe found, short of a confirmed login.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Probed {
    /// The server answered: the session is not authorized.
    NotAuthorized,
    /// No verified answer.
    Unknown(AuthFail),
}

/// What a login task reports.
pub(super) enum AuthStep<C: Client> {
    Startup(AuthResult<Account>),
    CodeSent(AuthResult<CodeRequest<C::LoginToken>>),
    SignIn(AuthResult<SignInNext<C::PasswordToken>>),
    /// A wrong password (with a fresh token), or a failure.
    Password(AuthResult<PasswordChallenge<C::PasswordToken>>),
    Challenge(AuthResult<PasswordChallenge<C::PasswordToken>>),
    /// Signed in; `durable` is the login barrier's result.
    Confirmed {
        account: Account,
        durable: SaveStatus,
        new_login: Option<NewLogin>,
    },
    Probe(Probed),
}

/// Messages from login and credential tasks to the actor.
pub(super) enum AuthMsg<C: Client> {
    Step {
        tag: AuthTag,
        step: AuthStep<C>,
    },
    Cred {
        attempt: AttemptId,
        done: CredDone,
    },
    /// A background cache re-binding finished (see `binding`).
    CacheSettled,
}

/// The running login task.
#[derive(Debug)]
struct Running {
    tag: AuthTag,
    kind: AuthKind,
    /// Cancels this task only (a child of the instance's token).
    cancel: CancellationToken,
}

/// What the timer does when it fires.
#[derive(Debug)]
enum Wake {
    /// A flood wait ended: show this state again.
    Resume(AuthState),
    /// Probe again (after a flood wait or an inconclusive answer).
    Probe,
}

/// Login state owned by the actor.
pub(super) struct AuthMachine<C: Client> {
    /// The last state sent to the UI.
    pub(super) state: AuthState,
    /// The account is signed in: builds bootstrap instead of serving login.
    pub(super) signed_in: bool,
    /// Error to show on `Phone` once the next signed-out instance is up.
    pub(super) phone_error: Option<UserError>,
    /// Request a code as soon as a signed-out instance is up (a phone number
    /// submitted while none was).
    code_after_build: bool,
    startup_checked: bool,
    attempt: AttemptId,
    current: Option<Running>,
    tx: mpsc::Sender<AuthMsg<C>>,
    pub(super) rx: mpsc::Receiver<AuthMsg<C>>,
    phone: Option<Zeroizing<String>>,
    pub(super) phone_hint: String,
    pub(super) sent: Option<SentCodeInfo>,
    pub(super) login_token: Option<Arc<C::LoginToken>>,
    pub(super) password_token: Option<C::PasswordToken>,
    pub(super) hint: Option<String>,
    timer: Option<(Instant, Wake)>,
    /// An ambiguous sign-in awaits the probe.
    pub(super) reconcile: Option<Resume>,
    probe_failures: u32,
    pub(super) creds: Creds,
}

impl<C: Client> AuthMachine<C> {
    pub(super) fn new(account: &SessionSnapshot) -> Self {
        let (tx, rx) = mpsc::channel(AUTH_QUEUE);
        Self {
            state: AuthState::Connecting,
            signed_in: account.is_signed_in().unwrap_or(false),
            phone_error: None,
            code_after_build: false,
            startup_checked: false,
            attempt: AttemptId(0),
            current: None,
            tx,
            rx,
            phone: None,
            phone_hint: String::new(),
            sent: None,
            login_token: None,
            password_token: None,
            hint: None,
            timer: None,
            reconcile: None,
            probe_failures: 0,
            creds: Creds::default(),
        }
    }

    pub(super) fn flood_deadline(&self) -> Option<Instant> {
        self.timer.as_ref().map(|(at, _)| *at)
    }

    pub(super) fn attempt(&self) -> AttemptId {
        self.attempt
    }

    /// Starts a new attempt: results of the old one are stale from now on.
    pub(super) fn bump_attempt(&mut self) -> AttemptId {
        self.attempt = AttemptId(self.attempt.0.wrapping_add(1));
        self.attempt
    }

    pub(super) fn sender(&self) -> mpsc::Sender<AuthMsg<C>> {
        self.tx.clone()
    }

    /// Forgets the phone number and tokens of a finished or abandoned login.
    pub(super) fn clear_login(&mut self) {
        self.phone = None;
        self.code_after_build = false;
        self.sent = None;
        self.login_token = None;
        self.password_token = None;
        self.hint = None;
        self.timer = None;
    }

    /// Cancels the running login task; its result will be stale. A call
    /// that may have signed in leaves a reconciliation behind.
    pub(super) fn supersede(&mut self, resume: Resume) {
        if let Some(run) = self.current.take() {
            run.cancel.cancel();
            if run.kind.may_sign_in() {
                self.reconcile = Some(resume);
            }
        }
    }

    /// Forgets the running task (its instance is being retired with it).
    pub(super) fn abort_task(&mut self) {
        if let Some(run) = self.current.take() {
            run.cancel.cancel();
        }
    }

    pub(super) fn reset_signed_out(&mut self) {
        self.abort_task();
        self.clear_login();
        self.reconcile = None;
        self.probe_failures = 0;
        self.signed_in = false;
    }

    /// Takes the running task if `tag` is it and still current.
    fn finish(&mut self, tag: AuthTag, cgen: ClientGen) -> Option<AuthKind> {
        let run = self.current.as_ref()?;
        if run.tag != tag || tag.cgen != cgen {
            return None;
        }
        self.current.take().map(|r| r.kind)
    }

    fn probe_delay(&mut self) -> Duration {
        self.probe_failures = self.probe_failures.saturating_add(1);
        let factor = 2u32
            .checked_pow(self.probe_failures.saturating_sub(1))
            .unwrap_or(u32::MAX);
        PROBE_RETRY_MIN.saturating_mul(factor).min(PROBE_RETRY_MAX)
    }
}

/// Everything a login task needs from the instance, and what its calls saw.
pub(super) struct Ctx<C> {
    pub(super) client: Arc<C>,
    cancel: CancellationToken,
    deadline: Duration,
    pub(super) account: SessionSnapshot,
    timed_out: AtomicBool,
    dropped: AtomicBool,
}

impl<C> Ctx<C> {
    /// Runs a login call under the deadline and the task's cancellation.
    pub(super) async fn call<T, E: Into<AuthError>>(
        &self,
        fut: impl Future<Output = Result<T, E>>,
    ) -> AuthResult<T> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => {
                self.dropped.store(true, Ordering::Release);
                Err(AuthFail::Dropped)
            }
            res = tokio::time::timeout(self.deadline, fut) => {
                if let Ok(res) = res {
                    res.map_err(|e| AuthFail::Auth(e.into()))
                } else {
                    self.timed_out.store(true, Ordering::Release);
                    Err(AuthFail::Timeout)
                }
            }
        }
    }

    /// The admission report: a call that may still sit in the library's
    /// queues (timed out or cancelled) parks the permit.
    fn done(&self, cgen: ClientGen, permit: OwnedSemaphorePermit) -> Done {
        let err = if self.timed_out.load(Ordering::Acquire) {
            Some(CallError::Timeout)
        } else if self.dropped.load(Ordering::Acquire) {
            Some(CallError::Dropped)
        } else {
            None
        };
        Done {
            cgen,
            parked: err.map(|_| permit),
            // A login call reports like a read mark: no store change, only
            // the error for stall detection.
            outcome: Outcome::Read(err.map_or(Ok(()), Err)),
        }
    }
}

/// An admitted, not yet spawned login task.
pub(super) struct Prepared<C> {
    ctx: Arc<Ctx<C>>,
    permit: OwnedSemaphorePermit,
    tag: AuthTag,
}

/// Login durability (plan section 3): wait for the self-user write.
pub(super) async fn confirm<C: Client>(
    account: SessionSnapshot,
    signed_in: Account,
    has_password: Option<bool>,
) -> AuthStep<C> {
    let durable = if matches!(
        tokio::time::timeout(LOGIN_BARRIER_WAIT, account.barrier()).await,
        Ok(Ok(()))
    ) {
        SaveStatus::Confirmed
    } else {
        SaveStatus::Unconfirmed
    };
    AuthStep::Confirmed {
        account: signed_in,
        durable,
        new_login: Some(NewLogin { has_password }),
    }
}

/// Strips separators; `None` unless `+` and 5 to 15 digits remain.
fn normalize_phone(input: &str) -> Option<Zeroizing<String>> {
    let mut out = Zeroizing::new(String::with_capacity(16));
    for (i, ch) in input.trim().chars().enumerate() {
        match ch {
            '+' if i == 0 => out.push('+'),
            '0'..='9' => out.push(ch),
            ' ' | '-' | '(' | ')' | '.' => {}
            _ => return None,
        }
    }
    let digits = out.chars().filter(char::is_ascii_digit).count();
    PHONE_DIGITS.contains(&digits).then_some(out)
}

fn valid_code(code: &str) -> bool {
    !code.is_empty() && code.len() <= MAX_CODE_LEN && code.bytes().all(|b| b.is_ascii_digit())
}

/// The code screen's view of a sent code.
pub(super) fn sent_info(sent: SentCode) -> SentCodeInfo {
    SentCodeInfo {
        delivery: sent.delivery,
        length: sent.length,
        next: sent.next,
        resend_at: sent
            .timeout
            .map(|t| SystemTime::now() + t.min(MAX_RESEND_WAIT)),
    }
}

impl<B: Backend> Actor<B> {
    pub(super) async fn set_auth(&mut self, state: AuthState) {
        self.auth.state = state.clone();
        self.emit(Event::Auth(state)).await;
    }

    /// Repeats the current state (a command that does not apply to it).
    pub(super) async fn emit_auth(&self) {
        self.emit(Event::Auth(self.auth.state.clone())).await;
    }

    /// Admits a login call: one at a time, on an instance in the right
    /// phase, under a permit. Reports why not otherwise.
    async fn prepare(&mut self, kind: AuthKind) -> Option<Prepared<B::Client>> {
        if self.auth.current.is_some() {
            self.error(UserError::Busy).await;
            return None;
        }
        let phase_ok = match kind {
            AuthKind::Startup => matches!(self.life.phase, Phase::Booting | Phase::Live),
            _ => self.life.phase == Phase::SignedOut,
        };
        let Some(live) = self.live.as_ref().filter(|_| phase_ok) else {
            self.login_unavailable(kind).await;
            return None;
        };
        let ctx = Ctx {
            client: Arc::clone(&live.client),
            cancel: live.cancel.child_token(),
            deadline: self.deadline,
            account: self.account.clone(),
            timed_out: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
        };
        let Some(permit) = self.admission.try_acquire() else {
            // Only login calls run on a signed-out instance: no permit means
            // the budget is parked in calls the library may still hold.
            if self.admission.parked() > 0 {
                self.rebuild(Reason::Stall).await;
            }
            self.error(UserError::Busy).await;
            return None;
        };
        let tag = AuthTag {
            attempt: self.auth.attempt,
            cgen: self.cgen,
        };
        Some(Prepared {
            ctx: Arc::new(ctx),
            permit,
            tag,
        })
    }

    /// Spawns an admitted login task into the instance's task set.
    fn launch<F, Fut>(&mut self, prep: Prepared<B::Client>, kind: AuthKind, body: F)
    where
        F: FnOnce(Arc<Ctx<B::Client>>) -> Fut,
        Fut: Future<Output = AuthStep<B::Client>> + Send + 'static,
    {
        let Prepared { ctx, permit, tag } = prep;
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let tx = self.auth.sender();
        let cancel = ctx.cancel.clone();
        let fut = body(Arc::clone(&ctx));
        let handle = live.tasks.spawn(async move {
            let step = fut.await;
            // The actor outlives its instances' tasks; a closed channel
            // means it is shutting down.
            let _ = tx.send(AuthMsg::Step { tag, step }).await;
            ctx.done(tag.cgen, permit)
        });
        live.kinds.insert(handle.id(), TaskKind::Auth(tag));
        self.auth.current = Some(Running { tag, kind, cancel });
    }

    /// Called after every successful build.
    pub(super) async fn after_live(&mut self) {
        self.reap_retired().await;
        if self.life.phase == Phase::SignedOut && self.credentials_superseded() {
            return self.retire_for_credentials();
        }
        match self.life.phase {
            Phase::SignedOut if self.auth.reconcile.is_some() => self.start_probe().await,
            Phase::SignedOut if self.auth.code_after_build => {
                self.auth.code_after_build = false;
                self.request_code(AuthKind::RequestCode).await;
            }
            Phase::SignedOut => {
                if matches!(
                    self.auth.state,
                    AuthState::Connecting | AuthState::LoggedOut { .. }
                ) {
                    let error = self.auth.phone_error.take();
                    self.set_auth(AuthState::Phone { error }).await;
                }
            }
            Phase::Booting if !self.auth.startup_checked => self.check_startup().await,
            _ => {}
        }
    }

    /// The running task's instance was retired: its answer is lost (or
    /// stale when it comes). Recover the step it served.
    pub(super) async fn reap_retired(&mut self) {
        let Some(run) = self.auth.current.as_ref() else {
            return;
        };
        if run.tag.cgen == self.cgen {
            return;
        }
        let kind = run.kind;
        self.auth.abort_task();
        self.on_lost(kind).await;
    }

    /// A login call ended without a usable answer (lost with its instance).
    async fn on_lost(&mut self, kind: AuthKind) {
        match kind {
            AuthKind::Startup => self.auth.startup_checked = false,
            AuthKind::SignIn => self.begin_reconcile(Resume::Code).await,
            AuthKind::Password => self.begin_reconcile(Resume::Password).await,
            AuthKind::Probe => self.reconcile_failed(AuthFail::Dropped).await,
            AuthKind::RequestCode => {
                let error = Some(UserError::Network);
                self.set_auth(AuthState::Phone { error }).await;
            }
            AuthKind::ResendCode => self.code_state(Some(UserError::Network)).await,
            AuthKind::Challenge => self.token_unavailable(UserError::Network).await,
        }
    }

    /// A stored session: fetch the self user, which proves the session is
    /// still authorized and gives the display name. The bootstrap runs
    /// meanwhile.
    async fn check_startup(&mut self) {
        self.auth.startup_checked = true;
        let Some(prep) = self.prepare(AuthKind::Startup).await else {
            self.auth.startup_checked = false;
            return;
        };
        self.launch(prep, AuthKind::Startup, |ctx| async move {
            AuthStep::Startup(ctx.call(ctx.client.fetch_self()).await)
        });
    }

    /// A login command while a reconciliation is pending: the probe answers
    /// first (started now if it is waiting for a retry).
    async fn await_reconcile(&mut self) {
        if self.auth.current.is_none() && self.life.phase == Phase::SignedOut {
            self.auth.timer = None;
            return self.start_probe().await;
        }
        self.emit_auth().await;
    }

    /// Commands that start a login call: refused while one runs, a flood
    /// wait holds or a reconciliation is pending.
    async fn login_blocked(&mut self) -> bool {
        if self.auth.reconcile.is_some() {
            self.await_reconcile().await;
            return true;
        }
        if self.auth.timer.is_some() {
            self.emit_auth().await;
            return true;
        }
        false
    }

    pub(super) async fn submit_phone(&mut self, input: &str) {
        if !matches!(
            self.auth.state,
            AuthState::Phone { .. }
                | AuthState::SignUpRequired
                | AuthState::OfficialAppRequired
                | AuthState::LoggedOut { .. }
        ) || self.auth.signed_in
        {
            return self.emit_auth().await;
        }
        if self.login_blocked().await {
            return;
        }
        let Some(phone) = normalize_phone(input) else {
            let error = Some(UserError::PhoneInvalid);
            return self.set_auth(AuthState::Phone { error }).await;
        };
        if self.auth.current.is_some() {
            return self.error(UserError::Busy).await;
        }
        self.auth.bump_attempt();
        self.auth.phone_hint = mask_phone(&phone);
        self.auth.phone = Some(phone);
        match self.life.phase {
            Phase::SignedOut => self.request_code(AuthKind::RequestCode).await,
            // No instance yet: request the code once one is up.
            Phase::Halted => {
                self.auth.code_after_build = true;
                self.start_build(Duration::ZERO);
            }
            Phase::Building | Phase::Retiring(_) => self.auth.code_after_build = true,
            Phase::Booting | Phase::Live | Phase::LoggingOut(_) => self.emit_auth().await,
        }
    }

    pub(super) async fn resend_code(&mut self) {
        if !matches!(self.auth.state, AuthState::Code { .. }) {
            return self.emit_auth().await;
        }
        if self.login_blocked().await {
            return;
        }
        self.request_code(AuthKind::ResendCode).await;
    }

    /// `auth.sendCode`, or `auth.resendCode` with the current token. A
    /// resend the server refuses (expired, no next type) sends a new code.
    async fn request_code(&mut self, kind: AuthKind) {
        let Some(phone) = self.auth.phone.clone() else {
            return self.login_unavailable(kind).await;
        };
        let token = self
            .auth
            .login_token
            .clone()
            .filter(|_| kind == AuthKind::ResendCode);
        let Some(prep) = self.prepare(kind).await else {
            return;
        };
        self.launch(prep, kind, |ctx| async move {
            let resent = match &token {
                Some(token) => ctx.call(ctx.client.resend_login_code(token)).await,
                None => Err(AuthFail::Auth(AuthError::Backend(BackendError::Rejected))),
            };
            let res = match resent {
                Err(AuthFail::Auth(AuthError::Backend(BackendError::Rejected))) => {
                    ctx.call(ctx.client.request_login_code(&phone)).await
                }
                other => other,
            };
            AuthStep::CodeSent(res)
        });
    }

    pub(super) async fn submit_code(&mut self, code: Zeroizing<String>) {
        if !matches!(self.auth.state, AuthState::Code { .. }) {
            return self.emit_auth().await;
        }
        if self.login_blocked().await {
            return;
        }
        let code = Zeroizing::new(code.trim().to_owned());
        if !valid_code(&code) {
            return self.code_state(Some(UserError::InvalidCode)).await;
        }
        // A pending call answers `Busy` before the token is looked at.
        let Some(prep) = self.prepare(AuthKind::SignIn).await else {
            return;
        };
        // Shared, not taken: the token stays valid on the server until a
        // sign-in succeeds, so a lost answer or a panic does not lose it.
        let Some(token) = self.auth.login_token.clone() else {
            drop(prep);
            return self.code_state(Some(UserError::InvalidCode)).await;
        };
        self.launch(prep, AuthKind::SignIn, |ctx| async move {
            let res = match ctx.call(ctx.client.sign_in(&token, &code)).await {
                Ok(SignIn::Done(account)) => {
                    return confirm(ctx.account.clone(), account, Some(false)).await;
                }
                Ok(SignIn::PasswordRequired(ch)) => Ok(SignInNext::Password(ch)),
                Ok(SignIn::InvalidCode) => Ok(SignInNext::InvalidCode),
                Ok(SignIn::SignUpRequired) => Ok(SignInNext::SignUpRequired),
                Err(e) => Err(e),
            };
            AuthStep::SignIn(res)
        });
    }

    pub(super) async fn submit_password(&mut self, password: PasswordBuf) {
        if !matches!(self.auth.state, AuthState::Password { .. }) {
            // Includes `PasswordTokenUnavailable`: retry the params first.
            return self.emit_auth().await;
        }
        if self.login_blocked().await {
            return;
        }
        let Some(prep) = self.prepare(AuthKind::Password).await else {
            return;
        };
        // Taken: a password token is single-use (`check_password` consumes it).
        let Some(token) = self.auth.password_token.take() else {
            drop(prep);
            return self.token_unavailable(UserError::Network).await;
        };
        self.launch(prep, AuthKind::Password, |ctx| async move {
            match ctx.call(ctx.client.check_password(token, password)).await {
                Ok(CheckPassword::Done(account)) => {
                    confirm(ctx.account.clone(), account, Some(true)).await
                }
                Ok(CheckPassword::InvalidPassword(ch)) => AuthStep::Password(Ok(ch)),
                Err(e) => AuthStep::Password(Err(e)),
            }
        });
    }

    pub(super) async fn retry_password_params(&mut self) {
        if !matches!(
            self.auth.state,
            AuthState::PasswordTokenUnavailable { .. } | AuthState::Password { .. }
        ) {
            return self.emit_auth().await;
        }
        if self.login_blocked().await {
            return;
        }
        let Some(prep) = self.prepare(AuthKind::Challenge).await else {
            return;
        };
        self.launch(prep, AuthKind::Challenge, |ctx| async move {
            AuthStep::Challenge(ctx.call(ctx.client.password_challenge()).await)
        });
    }

    /// An ambiguous completion: reconcile before anything else.
    pub(super) async fn begin_reconcile(&mut self, resume: Resume) {
        self.auth.reconcile = Some(resume);
        self.auth.probe_failures = 0;
        self.start_probe().await;
    }

    /// Asks the server whether the session is signed in (plan section 4):
    /// authorized means a call went through, so fetch and cache the self
    /// user and confirm.
    pub(super) async fn start_probe(&mut self) {
        if self.auth.reconcile.is_none() || self.auth.current.is_some() {
            return;
        }
        if !matches!(self.auth.state, AuthState::Verifying { .. }) {
            self.set_auth(AuthState::Verifying { error: None }).await;
        }
        let Some(prep) = self.prepare(AuthKind::Probe).await else {
            return;
        };
        self.launch(prep, AuthKind::Probe, |ctx| async move {
            match ctx.call(ctx.client.is_authorized()).await {
                Ok(false) => return AuthStep::Probe(Probed::NotAuthorized),
                Ok(true) => {}
                Err(fail) => return AuthStep::Probe(Probed::Unknown(fail)),
            }
            match ctx.call(ctx.client.fetch_self()).await {
                Ok(account) => confirm(ctx.account.clone(), account, None).await,
                Err(fail) => AuthStep::Probe(Probed::Unknown(fail)),
            }
        });
    }

    /// The probe had no verified answer: keep reconciling. A flood wait is
    /// honored; a revoked key tears down; anything else retries with backoff.
    pub(super) async fn reconcile_failed(&mut self, fail: AuthFail) {
        if self.auth.reconcile.is_none() {
            return;
        }
        if fail.is_revoked() {
            return self.login_revoked().await;
        }
        if let Some(seconds) = fail.flood_seconds() {
            let wait = Duration::from_secs(u64::from(seconds));
            self.auth.timer = Some((Instant::now() + wait, Wake::Probe));
            let until = SystemTime::now() + wait;
            let resume = Box::new(AuthState::Verifying { error: None });
            return self.set_auth(AuthState::FloodWait { until, resume }).await;
        }
        let delay = self.auth.probe_delay();
        self.auth.timer = Some((Instant::now() + delay, Wake::Probe));
        let error = Some(fail.user_error());
        self.set_auth(AuthState::Verifying { error }).await;
    }

    /// The probe verified that the session is not signed in.
    pub(super) async fn reconcile_not_authorized(&mut self) {
        self.auth.probe_failures = 0;
        match self.auth.reconcile.take() {
            Some(Resume::Code) => self.code_state(Some(UserError::Network)).await,
            Some(Resume::Password) => self.token_unavailable(UserError::Network).await,
            Some(Resume::Phone) | None => {
                self.set_auth(AuthState::Phone { error: None }).await;
            }
        }
    }

    /// `AUTH_KEY_UNREGISTERED`/`SESSION_REVOKED` during login: the local
    /// session is torn down even though no account is signed in.
    pub(super) async fn login_revoked(&mut self) {
        self.auth.reconcile = None;
        self.auth.clear_login();
        self.session_revoked().await;
    }

    pub(super) async fn code_state(&mut self, error: Option<UserError>) {
        let phone_hint = self.auth.phone_hint.clone();
        let sent = self.auth.sent;
        self.set_auth(AuthState::Code {
            phone_hint,
            sent,
            error,
        })
        .await;
    }

    pub(super) async fn token_unavailable(&mut self, error: UserError) {
        self.set_auth(AuthState::PasswordTokenUnavailable { error })
            .await;
    }

    /// `FLOOD_WAIT`: count down, then return to `resume`.
    pub(super) async fn flood_wait(&mut self, seconds: u32, resume: AuthState) {
        let wait = Duration::from_secs(u64::from(seconds));
        self.auth.timer = Some((Instant::now() + wait, Wake::Resume(resume.clone())));
        let until = SystemTime::now() + wait;
        let resume = Box::new(resume);
        self.set_auth(AuthState::FloodWait { until, resume }).await;
    }

    /// The login timer fired: a flood wait ended or a probe is due.
    pub(super) async fn flood_over(&mut self) {
        match self.auth.timer.take() {
            Some((_, Wake::Resume(resume))) => self.set_auth(resume).await,
            Some((_, Wake::Probe)) => self.start_probe().await,
            None => {}
        }
    }

    /// A login completed: persist-confirmed or not, the account is signed in.
    pub(super) async fn finish_login(
        &mut self,
        account: Account,
        durable: SaveStatus,
        new_login: Option<NewLogin>,
    ) {
        self.auth.clear_login();
        self.auth.reconcile = None;
        self.auth.probe_failures = 0;
        self.auth.signed_in = true;
        self.auth.startup_checked = true;
        self.clear_halt();
        self.credentials_proven();
        let display_name = account.display_name;
        let state = if durable == SaveStatus::Confirmed {
            AuthState::SignedIn {
                display_name,
                new_login,
            }
        } else {
            AuthState::SignedInUnsaved {
                display_name,
                new_login,
            }
        };
        self.set_auth(state).await;
        // Before the bootstrap writes anything: rows are served only from a
        // cache bound to this account.
        self.bind_cache().await;
        if self.life.phase == Phase::SignedOut {
            self.start_bootstrap();
        }
    }

    /// A login call could not start: no signed-out instance is up.
    async fn login_unavailable(&mut self, kind: AuthKind) {
        let error = Some(UserError::Network);
        match kind {
            // Retried on the next instance.
            AuthKind::Startup => {}
            AuthKind::Probe => self.set_auth(AuthState::Verifying { error }).await,
            AuthKind::ResendCode | AuthKind::SignIn => self.code_state(error).await,
            AuthKind::Password | AuthKind::Challenge => {
                let state = match &self.auth.state {
                    AuthState::Password { hint, .. } => AuthState::Password {
                        hint: hint.clone(),
                        error,
                    },
                    _ => AuthState::PasswordTokenUnavailable {
                        error: UserError::Network,
                    },
                };
                self.set_auth(state).await;
            }
            AuthKind::RequestCode => self.set_auth(AuthState::Phone { error }).await,
        }
    }

    /// Applies a login task's result if it is the running one.
    pub(super) async fn on_auth_step(&mut self, tag: AuthTag, step: AuthStep<B::Client>) {
        // A logout aborted the login; nothing it found applies any more.
        if matches!(self.life.phase, Phase::LoggingOut(_)) {
            return;
        }
        let Some(kind) = self.auth.finish(tag, self.cgen) else {
            // Stale: another attempt or instance. Dropping it consumes any
            // password token it carries.
            drop(step);
            return self.reap_retired().await;
        };
        self.apply_auth(kind, step).await;
    }

    /// A login task panicked (a bug in the Telegram library). Its permit was
    /// released while unwinding, so the user can retry.
    pub(super) async fn on_auth_panic(&mut self, tag: AuthTag) {
        if let Some(kind) = self.auth.finish(tag, self.cgen) {
            self.auth_panicked(kind).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_normalization() {
        assert_eq!(
            normalize_phone(" +44 (7700) 900-123 ")
                .as_deref()
                .map(String::as_str),
            Some("+447700900123")
        );
        assert!(normalize_phone("+1234").is_none(), "four digits");
        assert!(
            normalize_phone("+1234567890123456").is_none(),
            "sixteen digits"
        );
        assert!(normalize_phone("44+7700900123").is_none(), "plus not first");
        assert!(normalize_phone("+44 7700 abc").is_none());
    }

    #[test]
    fn code_validation() {
        assert!(valid_code("12345"));
        assert!(!valid_code(""));
        assert!(!valid_code("12a45"));
        assert!(!valid_code(&"1".repeat(MAX_CODE_LEN + 1)));
    }

    #[test]
    fn resend_countdown_is_bounded() {
        let sent = SentCode {
            delivery: crate::api::CodeDelivery::Sms,
            length: Some(5),
            next: None,
            timeout: Some(Duration::from_secs(u64::MAX / 4)),
        };
        let at = sent_info(sent).resend_at.unwrap();
        assert!(at <= SystemTime::now() + MAX_RESEND_WAIT);
    }
}
