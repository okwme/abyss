//! Login state machine (plan section 4).
//!
//! Login calls run one at a time on the signed-out instance, each in its own
//! task under the call deadline and the instance's cancellation. An
//! [`AuthInFlight`] guard moves into that task; its `Drop` clears the pending
//! flag on every exit path (success, error, timeout, panic, cancellation), so
//! a stuck "submitting…" is impossible. After a timeout or transport error in
//! `sign_in`/`check_password` the outcome is ambiguous: the guard moves on to
//! a probe (`is_authorized`, then the self user) before a retry is allowed.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::Actor;
use super::apply::user_error;
use super::jobs::CallError;
use super::lifecycle::Phase;
use crate::api::{AuthState, Event, PasswordBuf, SaveStatus, UserError, mask_phone};
use crate::backend::{
    Account, AuthError, Backend, BackendError, CheckPassword, Client, PasswordChallenge, SignIn,
};
use crate::session::SessionSnapshot;

/// How long a login waits for the session to reach the Keychain before
/// reporting `SignedInUnsaved`.
pub(super) const LOGIN_BARRIER_WAIT: Duration = Duration::from_secs(10);
/// Phone numbers: E.164 allows at most 15 digits.
const PHONE_DIGITS: std::ops::RangeInclusive<usize> = 5..=15;
const MAX_CODE_LEN: usize = 16;

/// Holds the pending flag; clears it when dropped.
#[derive(Debug)]
pub(super) struct AuthInFlight(Arc<AtomicBool>);

impl AuthInFlight {
    fn acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(Arc::clone(flag)))
    }
}

impl Drop for AuthInFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Which login call a task runs, for panic handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AuthKind {
    /// Startup check of a stored session (the self user).
    Startup,
    RequestCode,
    ResendCode,
    SignIn,
    Password,
    Challenge,
    /// Resolving an ambiguous `sign_in` / `check_password`.
    Probe(Ambiguous),
}

/// Which step an ambiguous completion returns to if not authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ambiguous {
    Code,
    Password,
}

/// Why a login call has no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AuthFail {
    Auth(AuthError),
    Timeout,
    /// The instance was retired during the call.
    Dropped,
}

impl AuthFail {
    pub(super) fn user_error(self) -> UserError {
        match self {
            Self::Auth(AuthError::Backend(e)) => user_error(CallError::Backend(e)),
            Self::Auth(AuthError::PhoneInvalid) => UserError::PhoneInvalid,
            Self::Auth(AuthError::PhoneBanned) => UserError::PhoneBanned,
            Self::Auth(AuthError::Credentials(e)) => UserError::Credentials(e),
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

/// What a login task reports.
pub(super) enum AuthStep<C: Client> {
    Startup(AuthResult<Account>),
    CodeSent(AuthResult<C::LoginToken>),
    SignIn {
        token: C::LoginToken,
        res: AuthResult<SignInNext<C::PasswordToken>>,
    },
    /// A wrong password (with a fresh token), or a failure.
    Password(AuthResult<PasswordChallenge<C::PasswordToken>>),
    Challenge(AuthResult<PasswordChallenge<C::PasswordToken>>),
    /// Signed in; `durable` is the login barrier's result.
    Confirmed {
        account: Account,
        durable: SaveStatus,
    },
    /// An ambiguous completion that did not sign in.
    Unresolved(Ambiguous),
}

pub(super) struct AuthDone<C: Client> {
    pub(super) guard: AuthInFlight,
    pub(super) step: AuthStep<C>,
}

type AuthTask<C> = (AuthKind, JoinHandle<AuthDone<C>>);

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
    flag: Arc<AtomicBool>,
    task: Option<AuthTask<C>>,
    phone: Option<Zeroizing<String>>,
    pub(super) phone_hint: String,
    pub(super) login_token: Option<C::LoginToken>,
    pub(super) password_token: Option<C::PasswordToken>,
    pub(super) hint: Option<String>,
    flood: Option<(Instant, AuthState)>,
}

impl<C: Client> AuthMachine<C> {
    pub(super) fn new(account: &SessionSnapshot) -> Self {
        Self {
            state: AuthState::Connecting,
            signed_in: account.is_signed_in().unwrap_or(false),
            phone_error: None,
            code_after_build: false,
            startup_checked: false,
            flag: Arc::new(AtomicBool::new(false)),
            task: None,
            phone: None,
            phone_hint: String::new(),
            login_token: None,
            password_token: None,
            hint: None,
            flood: None,
        }
    }

    /// Whether a login call is pending (the guard is held).
    #[cfg(test)]
    pub(super) fn pending(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    pub(super) fn flood_deadline(&self) -> Option<Instant> {
        self.flood.as_ref().map(|(at, _)| *at)
    }

    pub(super) fn task_mut(&mut self) -> &mut Option<AuthTask<C>> {
        &mut self.task
    }

    /// Forgets the phone number and tokens of a finished or abandoned login.
    pub(super) fn clear_login(&mut self) {
        self.phone = None;
        self.code_after_build = false;
        self.login_token = None;
        self.password_token = None;
        self.hint = None;
        self.flood = None;
    }

    /// Stops a running login call; its guard drops with the task.
    pub(super) fn abort_task(&mut self) {
        if let Some((_, handle)) = self.task.take() {
            handle.abort();
        }
    }

    pub(super) fn reset_signed_out(&mut self) {
        self.abort_task();
        self.clear_login();
        self.signed_in = false;
    }
}

/// Awaits the running login task, if any.
pub(super) async fn next_auth<C: Client>(
    slot: &mut Option<AuthTask<C>>,
) -> (AuthKind, Result<AuthDone<C>, JoinError>) {
    let Some((kind, handle)) = slot.as_mut() else {
        return std::future::pending().await;
    };
    let kind = *kind;
    let res = handle.await;
    *slot = None;
    (kind, res)
}

/// Runs a login call under the deadline and the instance's cancellation.
async fn call<T, E: Into<AuthError>>(
    cancel: &CancellationToken,
    deadline: Duration,
    fut: impl Future<Output = Result<T, E>>,
) -> AuthResult<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(AuthFail::Dropped),
        res = tokio::time::timeout(deadline, fut) => match res {
            Ok(res) => res.map_err(|e| AuthFail::Auth(e.into())),
            Err(_) => Err(AuthFail::Timeout),
        },
    }
}

/// Login durability (plan section 3): wait for the self-user write.
async fn confirm<C: Client>(account: SessionSnapshot, signed_in: Account) -> AuthStep<C> {
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
    }
}

/// Everything a login task needs from the instance.
struct Ctx<C> {
    client: Arc<C>,
    cancel: CancellationToken,
    deadline: Duration,
    account: SessionSnapshot,
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

impl<B: Backend> Actor<B> {
    pub(super) async fn set_auth(&mut self, state: AuthState) {
        self.auth.state = state.clone();
        self.emit(Event::Auth(state)).await;
    }

    /// Repeats the current state (a command that does not apply to it).
    pub(super) async fn emit_auth(&self) {
        self.emit(Event::Auth(self.auth.state.clone())).await;
    }

    /// The signed-out instance's login context, if one is up.
    fn login_ctx(&self) -> Option<Ctx<B::Client>> {
        let live = self
            .live
            .as_ref()
            .filter(|_| self.life.phase == Phase::SignedOut)?;
        Some(Ctx {
            client: Arc::clone(&live.client),
            cancel: live.cancel.clone(),
            deadline: self.deadline,
            account: self.account.clone(),
        })
    }

    /// Takes the pending flag, unless a call runs or a flood wait holds.
    async fn begin_auth(&mut self) -> Option<AuthInFlight> {
        if self.auth.flood.is_some() {
            self.emit_auth().await;
            return None;
        }
        let guard = AuthInFlight::acquire(&self.auth.flag);
        if guard.is_none() {
            self.error(UserError::Busy).await;
        }
        guard
    }

    fn spawn_auth<F>(&mut self, kind: AuthKind, guard: AuthInFlight, fut: F)
    where
        F: Future<Output = AuthStep<B::Client>> + Send + 'static,
    {
        let handle = tokio::spawn(async move {
            let step = fut.await;
            AuthDone { guard, step }
        });
        self.auth.task = Some((kind, handle));
    }

    /// Called after every successful build.
    pub(super) async fn after_live(&mut self) {
        match self.life.phase {
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
            Phase::Booting if !self.auth.startup_checked => self.check_startup(),
            _ => {}
        }
    }

    /// A stored session: fetch the self user, which proves the session is
    /// still authorized and gives the display name. The bootstrap runs
    /// meanwhile.
    fn check_startup(&mut self) {
        self.auth.startup_checked = true;
        let Some(live) = self.live.as_ref() else {
            return;
        };
        let Some(guard) = AuthInFlight::acquire(&self.auth.flag) else {
            return;
        };
        let client = Arc::clone(&live.client);
        let cancel = live.cancel.clone();
        let deadline = self.deadline;
        self.spawn_auth(AuthKind::Startup, guard, async move {
            AuthStep::Startup(call(&cancel, deadline, client.fetch_self()).await)
        });
    }

    /// After a repeated migration: the user signs in again (plan section 2).
    pub(super) async fn require_relogin(&mut self) {
        self.auth.reset_signed_out();
        self.set_auth(AuthState::Phone {
            error: Some(UserError::AccountMigrated),
        })
        .await;
    }

    pub(super) async fn submit_phone(&mut self, input: &str) {
        if !matches!(
            self.auth.state,
            AuthState::Phone { .. }
                | AuthState::SignUpRequired
                | AuthState::NeedCredentials { .. }
                | AuthState::LoggedOut { .. }
        ) || self.auth.signed_in
        {
            return self.emit_auth().await;
        }
        let Some(phone) = normalize_phone(input) else {
            let error = Some(UserError::PhoneInvalid);
            return self.set_auth(AuthState::Phone { error }).await;
        };
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
        self.request_code(AuthKind::ResendCode).await;
    }

    async fn request_code(&mut self, kind: AuthKind) {
        let (Some(ctx), Some(phone)) = (self.login_ctx(), self.auth.phone.clone()) else {
            return self.login_unavailable().await;
        };
        let Some(guard) = self.begin_auth().await else {
            return;
        };
        self.spawn_auth(kind, guard, async move {
            let res = call(
                &ctx.cancel,
                ctx.deadline,
                ctx.client.request_login_code(&phone),
            )
            .await;
            AuthStep::CodeSent(res)
        });
    }

    pub(super) async fn submit_code(&mut self, code: Zeroizing<String>) {
        if !matches!(self.auth.state, AuthState::Code { .. }) {
            return self.emit_auth().await;
        }
        let code = Zeroizing::new(code.trim().to_owned());
        if !valid_code(&code) {
            return self.code_state(Some(UserError::InvalidCode)).await;
        }
        let Some(ctx) = self.login_ctx() else {
            return self.login_unavailable().await;
        };
        let Some(token) = self.auth.login_token.take() else {
            return self.code_state(Some(UserError::InvalidCode)).await;
        };
        let Some(guard) = self.begin_auth().await else {
            self.auth.login_token = Some(token);
            return;
        };
        self.spawn_auth(AuthKind::SignIn, guard, async move {
            let res = call(&ctx.cancel, ctx.deadline, ctx.client.sign_in(&token, &code)).await;
            let res = match res {
                Ok(SignIn::Done(account)) => return confirm(ctx.account, account).await,
                Ok(SignIn::PasswordRequired(ch)) => Ok(SignInNext::Password(ch)),
                Ok(SignIn::InvalidCode) => Ok(SignInNext::InvalidCode),
                Ok(SignIn::SignUpRequired) => Ok(SignInNext::SignUpRequired),
                Err(e) => Err(e),
            };
            AuthStep::SignIn { token, res }
        });
    }

    pub(super) async fn submit_password(&mut self, password: PasswordBuf) {
        if !matches!(self.auth.state, AuthState::Password { .. }) {
            // Includes `PasswordTokenUnavailable`: retry the params first.
            return self.emit_auth().await;
        }
        let Some(ctx) = self.login_ctx() else {
            return self.login_unavailable().await;
        };
        let Some(token) = self.auth.password_token.take() else {
            return self.token_unavailable(UserError::Network).await;
        };
        let Some(guard) = self.begin_auth().await else {
            self.auth.password_token = Some(token);
            return;
        };
        self.spawn_auth(AuthKind::Password, guard, async move {
            let res = call(
                &ctx.cancel,
                ctx.deadline,
                ctx.client.check_password(token, password),
            )
            .await;
            match res {
                Ok(CheckPassword::Done(account)) => confirm(ctx.account, account).await,
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
        let Some(ctx) = self.login_ctx() else {
            return self.login_unavailable().await;
        };
        let Some(guard) = self.begin_auth().await else {
            return;
        };
        self.spawn_auth(AuthKind::Challenge, guard, async move {
            let res = call(&ctx.cancel, ctx.deadline, ctx.client.password_challenge()).await;
            AuthStep::Challenge(res)
        });
    }

    /// Resolves an ambiguous completion (plan section 4): authorized means
    /// the call went through, so fetch and cache the self user and confirm.
    pub(super) fn probe(&mut self, after: Ambiguous, guard: AuthInFlight) {
        let ctx = self.login_ctx();
        self.spawn_auth(AuthKind::Probe(after), guard, async move {
            // No instance to ask: treat as not signed in; the user retries.
            let Some(ctx) = ctx else {
                return AuthStep::Unresolved(after);
            };
            if call(&ctx.cancel, ctx.deadline, ctx.client.is_authorized()).await != Ok(true) {
                return AuthStep::Unresolved(after);
            }
            match call(&ctx.cancel, ctx.deadline, ctx.client.fetch_self()).await {
                Ok(account) => confirm(ctx.account, account).await,
                Err(_) => AuthStep::Unresolved(after),
            }
        });
    }

    pub(super) async fn code_state(&mut self, error: Option<UserError>) {
        let phone_hint = self.auth.phone_hint.clone();
        self.set_auth(AuthState::Code { phone_hint, error }).await;
    }

    pub(super) async fn token_unavailable(&mut self, error: UserError) {
        self.set_auth(AuthState::PasswordTokenUnavailable { error })
            .await;
    }

    /// `FLOOD_WAIT`: count down, then return to `resume`.
    pub(super) async fn flood_wait(&mut self, seconds: u32, resume: AuthState) {
        let wait = Duration::from_secs(u64::from(seconds));
        self.auth.flood = Some((Instant::now() + wait, resume.clone()));
        let until = SystemTime::now() + wait;
        let resume = Box::new(resume);
        self.set_auth(AuthState::FloodWait { until, resume }).await;
    }

    pub(super) async fn flood_over(&mut self) {
        if let Some((_, resume)) = self.auth.flood.take() {
            self.set_auth(resume).await;
        }
    }

    /// A login completed: persist-confirmed or not, the account is signed in.
    pub(super) async fn finish_login(&mut self, account: Account, durable: SaveStatus) {
        self.auth.clear_login();
        self.auth.signed_in = true;
        self.auth.startup_checked = true;
        self.clear_halt();
        let display_name = account.display_name;
        let state = if durable == SaveStatus::Confirmed {
            AuthState::SignedIn { display_name }
        } else {
            AuthState::SignedInUnsaved { display_name }
        };
        self.set_auth(state).await;
        if self.life.phase == Phase::SignedOut {
            self.start_bootstrap();
        }
    }

    /// A login command arrived while no signed-out instance is up.
    async fn login_unavailable(&mut self) {
        let error = Some(UserError::Network);
        let state = match &self.auth.state {
            AuthState::Code { phone_hint, .. } => AuthState::Code {
                phone_hint: phone_hint.clone(),
                error,
            },
            AuthState::Password { hint, .. } => AuthState::Password {
                hint: hint.clone(),
                error,
            },
            _ => AuthState::Phone { error },
        };
        self.set_auth(state).await;
    }
}
