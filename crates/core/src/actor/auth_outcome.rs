//! Applying login task outcomes (plan section 4, "Flow" and "Error mapping").

use tokio::task::JoinError;

use super::Actor;
use super::auth::{Ambiguous, AuthDone, AuthFail, AuthInFlight, AuthKind, AuthStep, SignInNext};
use super::lifecycle::Phase;
use crate::api::{AuthState, UserError};
use crate::backend::{Account, AuthError, Backend, BackendError, Client, PasswordChallenge};

type Token<B> = <<B as Backend>::Client as Client>::LoginToken;
type Challenge<B> = PasswordChallenge<<<B as Backend>::Client as Client>::PasswordToken>;

impl<B: Backend> Actor<B> {
    pub(super) async fn on_auth(
        &mut self,
        kind: AuthKind,
        joined: Result<AuthDone<B::Client>, JoinError>,
    ) {
        // A logout aborted the login; nothing it found applies any more.
        if matches!(self.life.phase, Phase::LoggingOut(_)) {
            return;
        }
        let AuthDone { guard, step } = match joined {
            Ok(done) => done,
            Err(e) => {
                if e.is_panic() {
                    self.auth_panicked(kind).await;
                }
                return;
            }
        };
        match step {
            AuthStep::SignIn { token, res } => self.on_sign_in(guard, token, res).await,
            AuthStep::Password(res) => self.on_password(guard, res).await,
            other => {
                drop(guard);
                self.on_settled(kind, other).await;
            }
        }
    }

    /// Outcomes that release the pending flag.
    async fn on_settled(&mut self, kind: AuthKind, step: AuthStep<B::Client>) {
        match step {
            AuthStep::Startup(res) => self.on_startup(res).await,
            AuthStep::CodeSent(res) => self.on_code_sent(kind, res).await,
            AuthStep::Challenge(res) => self.on_challenge(res).await,
            AuthStep::Confirmed { account, durable } => self.finish_login(account, durable).await,
            AuthStep::Unresolved(Ambiguous::Code) => {
                self.code_state(Some(UserError::Network)).await;
            }
            AuthStep::Unresolved(Ambiguous::Password) => {
                self.token_unavailable(UserError::Network).await;
            }
            AuthStep::SignIn { .. } | AuthStep::Password(_) => {}
        }
    }

    async fn on_startup(&mut self, res: Result<Account, AuthFail>) {
        if res == Err(AuthFail::Auth(AuthError::Backend(BackendError::Unauthorized))) {
            return self.session_revoked().await;
        }
        if self.auth.state != AuthState::Connecting {
            return;
        }
        // Offline: still signed in; the name follows on a later start.
        let display_name = res.map(|a| a.display_name).unwrap_or_default();
        self.set_auth(AuthState::SignedIn { display_name }).await;
    }

    async fn on_code_sent(&mut self, kind: AuthKind, res: Result<Token<B>, AuthFail>) {
        let fail = match res {
            Ok(token) => {
                self.auth.login_token = Some(token);
                return self.code_state(None).await;
            }
            Err(fail) => fail,
        };
        let resume = if kind == AuthKind::ResendCode {
            AuthState::Code {
                phone_hint: self.auth.phone_hint.clone(),
                error: None,
            }
        } else {
            AuthState::Phone { error: None }
        };
        if let Some(seconds) = fail.flood_seconds() {
            return self.flood_wait(seconds, resume).await;
        }
        let error = fail.user_error();
        let next = match (fail, resume) {
            // Plan section 17.2: back to credentials; the session is untouched.
            (AuthFail::Auth(AuthError::Credentials(e)), _) => {
                self.auth.clear_login();
                AuthState::NeedCredentials { error: Some(e) }
            }
            (AuthFail::Auth(AuthError::PhoneInvalid | AuthError::PhoneBanned), _)
            | (_, AuthState::Phone { .. }) => AuthState::Phone { error: Some(error) },
            (_, resume) => with_error(resume, error),
        };
        self.set_auth(next).await;
    }

    async fn on_sign_in(
        &mut self,
        guard: AuthInFlight,
        token: Token<B>,
        res: Result<SignInNext<<B::Client as Client>::PasswordToken>, AuthFail>,
    ) {
        match res {
            Ok(SignInNext::Password(challenge)) => {
                drop(guard);
                self.password_state(challenge, None).await;
            }
            Ok(SignInNext::InvalidCode) => {
                drop(guard);
                self.auth.login_token = Some(token);
                self.code_state(Some(UserError::InvalidCode)).await;
            }
            Ok(SignInNext::SignUpRequired) => {
                drop(guard);
                self.auth.clear_login();
                self.set_auth(AuthState::SignUpRequired).await;
            }
            Err(fail) => {
                self.auth.login_token = Some(token);
                if fail.is_ambiguous() {
                    // Keep the pending flag until the probe has answered.
                    return self.probe(Ambiguous::Code, guard);
                }
                drop(guard);
                if let Some(seconds) = fail.flood_seconds() {
                    let resume = AuthState::Code {
                        phone_hint: self.auth.phone_hint.clone(),
                        error: None,
                    };
                    return self.flood_wait(seconds, resume).await;
                }
                self.code_state(Some(fail.user_error())).await;
            }
        }
    }

    /// `check_password` answered. Anything but a wrong password consumed the
    /// token.
    async fn on_password(&mut self, guard: AuthInFlight, res: Result<Challenge<B>, AuthFail>) {
        let fail = match res {
            Ok(challenge) => {
                drop(guard);
                return self
                    .password_state(challenge, Some(UserError::InvalidPassword))
                    .await;
            }
            Err(fail) => fail,
        };
        if fail.is_ambiguous() {
            return self.probe(Ambiguous::Password, guard);
        }
        drop(guard);
        self.token_consumed(fail).await;
    }

    async fn on_challenge(&mut self, res: Result<Challenge<B>, AuthFail>) {
        match res {
            Ok(challenge) => self.password_state(challenge, None).await,
            Err(fail) => self.token_consumed(fail).await,
        }
    }

    /// No usable password token: the user must retry the params first.
    async fn token_consumed(&mut self, fail: AuthFail) {
        let error = fail.user_error();
        match fail.flood_seconds() {
            Some(seconds) => {
                let resume = AuthState::PasswordTokenUnavailable { error };
                self.flood_wait(seconds, resume).await;
            }
            None => self.token_unavailable(error).await,
        }
    }

    async fn password_state(&mut self, challenge: Challenge<B>, error: Option<UserError>) {
        let PasswordChallenge { token, hint } = challenge;
        let hint = hint.map(|h| crate::sanitize::display_name(&h));
        self.auth.password_token = Some(token);
        self.auth.hint.clone_from(&hint);
        self.set_auth(AuthState::Password { hint, error }).await;
    }

    /// A login task panicked (a bug in the Telegram library). Its guard was
    /// dropped while unwinding, so the user can retry.
    async fn auth_panicked(&mut self, kind: AuthKind) {
        let error = UserError::Internal;
        match kind {
            AuthKind::Startup => {
                self.error(error).await;
                self.on_startup(Err(AuthFail::Dropped)).await;
            }
            AuthKind::RequestCode => {
                self.set_auth(AuthState::Phone { error: Some(error) })
                    .await;
            }
            AuthKind::ResendCode | AuthKind::SignIn | AuthKind::Probe(Ambiguous::Code) => {
                self.code_state(Some(error)).await;
            }
            AuthKind::Password | AuthKind::Challenge | AuthKind::Probe(Ambiguous::Password) => {
                self.token_unavailable(error).await;
            }
        }
    }
}

fn with_error(state: AuthState, error: UserError) -> AuthState {
    match state {
        AuthState::Code { phone_hint, .. } => AuthState::Code {
            phone_hint,
            error: Some(error),
        },
        other => other,
    }
}
