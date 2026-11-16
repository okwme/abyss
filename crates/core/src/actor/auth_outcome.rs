//! Applying login task outcomes (plan section 4, "Flow" and "Error mapping";
//! section 17.2a).

use super::Actor;
use super::auth::{AuthFail, AuthKind, AuthMsg, AuthStep, Probed, Resume, SignInNext, sent_info};
use crate::api::{AuthState, UserError};
use crate::backend::{Account, AuthError, Backend, Client, CodeRequest, PasswordChallenge};

type Token<B> = <<B as Backend>::Client as Client>::LoginToken;
type Challenge<B> = PasswordChallenge<<<B as Backend>::Client as Client>::PasswordToken>;
type Next<B> = SignInNext<<<B as Backend>::Client as Client>::PasswordToken>;

impl<B: Backend> Actor<B> {
    pub(super) async fn on_auth(&mut self, msg: AuthMsg<B::Client>) {
        match msg {
            AuthMsg::Step { tag, step } => self.on_auth_step(tag, step).await,
            AuthMsg::Cred { attempt, done } => self.on_cred(attempt, done).await,
        }
    }

    /// A current task's result.
    pub(super) async fn apply_auth(&mut self, kind: AuthKind, step: AuthStep<B::Client>) {
        match step {
            AuthStep::Startup(res) => self.on_startup(res).await,
            AuthStep::CodeSent(res) => self.on_code_sent(kind, res).await,
            AuthStep::SignIn(res) => self.on_sign_in(res).await,
            AuthStep::Password(res) => self.on_password(res).await,
            AuthStep::Challenge(res) => self.on_challenge(res).await,
            AuthStep::Confirmed {
                account,
                durable,
                new_login,
            } => self.finish_login(account, durable, new_login).await,
            AuthStep::Probe(Probed::NotAuthorized) => self.reconcile_not_authorized().await,
            AuthStep::Probe(Probed::Unknown(fail)) => self.reconcile_failed(fail).await,
        }
    }

    async fn on_startup(&mut self, res: Result<Account, AuthFail>) {
        match res {
            Err(fail) if fail.is_revoked() => return self.session_revoked().await,
            Ok(_) => self.credentials_proven(),
            Err(_) => {}
        }
        if self.auth.state != AuthState::Connecting {
            return;
        }
        // Offline: still signed in; the name follows on a later start.
        let display_name = res.map(|a| a.display_name).unwrap_or_default();
        self.set_auth(AuthState::SignedIn {
            display_name,
            new_login: None,
        })
        .await;
    }

    async fn on_code_sent(&mut self, kind: AuthKind, res: Result<CodeRequest<Token<B>>, AuthFail>) {
        let fail = match res {
            Ok(CodeRequest { token, sent }) => {
                self.auth.login_token = Some(std::sync::Arc::new(token));
                self.auth.sent = Some(sent_info(sent));
                // The first successful code request proves the pair.
                self.credentials_proven();
                return self.code_state(None).await;
            }
            Err(fail) => fail,
        };
        if fail.is_revoked() {
            return self.login_revoked().await;
        }
        let resend = kind == AuthKind::ResendCode;
        if let Some(seconds) = fail.flood_seconds() {
            let resume = if resend {
                self.code_resume()
            } else {
                AuthState::Phone { error: None }
            };
            return self.flood_wait(seconds, resume).await;
        }
        match fail {
            // Plan section 17.2: back to credentials; the session is untouched.
            AuthFail::Auth(AuthError::Credentials(e)) => self.credentials_rejected(e).await,
            AuthFail::Auth(AuthError::OfficialAppRequired) => {
                self.auth.clear_login();
                self.set_auth(AuthState::OfficialAppRequired).await;
            }
            AuthFail::Auth(
                AuthError::PhoneInvalid | AuthError::PhoneBanned | AuthError::PhoneFlood,
            ) => {
                let error = Some(fail.user_error());
                self.set_auth(AuthState::Phone { error }).await;
            }
            _ if resend => self.code_state(Some(fail.user_error())).await,
            _ => {
                let error = Some(fail.user_error());
                self.set_auth(AuthState::Phone { error }).await;
            }
        }
    }

    fn code_resume(&self) -> AuthState {
        AuthState::Code {
            phone_hint: self.auth.phone_hint.clone(),
            sent: self.auth.sent,
            error: None,
        }
    }

    async fn on_sign_in(&mut self, res: Result<Next<B>, AuthFail>) {
        let fail = match res {
            Ok(SignInNext::Password(challenge)) => {
                self.auth.login_token = None;
                return self.password_state(challenge, None).await;
            }
            Ok(SignInNext::InvalidCode) => {
                return self.code_state(Some(UserError::InvalidCode)).await;
            }
            Ok(SignInNext::SignUpRequired) => {
                self.auth.clear_login();
                return self.set_auth(AuthState::SignUpRequired).await;
            }
            Err(fail) => fail,
        };
        if fail.is_revoked() {
            return self.login_revoked().await;
        }
        if fail.is_ambiguous() {
            return self.begin_reconcile(Resume::Code).await;
        }
        if let Some(seconds) = fail.flood_seconds() {
            let resume = self.code_resume();
            return self.flood_wait(seconds, resume).await;
        }
        self.code_state(Some(fail.user_error())).await;
    }

    /// `check_password` answered. Anything but a wrong password consumed the
    /// token.
    async fn on_password(&mut self, res: Result<Challenge<B>, AuthFail>) {
        let fail = match res {
            Ok(challenge) => {
                return self
                    .password_state(challenge, Some(UserError::InvalidPassword))
                    .await;
            }
            Err(fail) => fail,
        };
        if fail.is_revoked() {
            return self.login_revoked().await;
        }
        if fail.is_ambiguous() {
            return self.begin_reconcile(Resume::Password).await;
        }
        self.token_consumed(fail).await;
    }

    async fn on_challenge(&mut self, res: Result<Challenge<B>, AuthFail>) {
        match res {
            Ok(challenge) => self.password_state(challenge, None).await,
            Err(fail) if fail.is_revoked() => self.login_revoked().await,
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

    /// A login task panicked. Its permit was released while unwinding, so
    /// the user can retry.
    pub(super) async fn auth_panicked(&mut self, kind: AuthKind) {
        let error = UserError::Internal;
        match kind {
            AuthKind::Startup => {
                self.error(error).await;
                self.on_startup(Err(AuthFail::Dropped)).await;
            }
            AuthKind::RequestCode => {
                self.set_auth(AuthState::Phone { error: Some(error) }).await;
            }
            AuthKind::ResendCode | AuthKind::SignIn => self.code_state(Some(error)).await,
            AuthKind::Password | AuthKind::Challenge => self.token_unavailable(error).await,
            // Still unresolved: retry the probe with backoff.
            AuthKind::Probe => self.reconcile_failed(AuthFail::Dropped).await,
        }
    }
}
