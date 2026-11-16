//! Credential provenance and onboarding transitions (plan sections 17.2,
//! 17.2a and 17.5). Tests that wait for a Keychain write run on real time:
//! the persistence worker is an OS thread.

use std::time::Duration;

use super::{Harness, Setup, start};
use crate::api::{
    ApiCredentials, ApiHash, AuthState, CodeDelivery, Command, CredentialError, CredentialSource,
    EnvVar, Event, NewLogin, NextDelivery, UserError,
};
use crate::backend::fake::{
    FAKE_API_ID, FAKE_CODE, FAKE_INVALID_API_ID, FAKE_PUBLISHED_API_ID, FakeBackend, FakeScript,
    Fault, Op, fake_credentials,
};
use crate::credentials::EnvResolution;
use crate::secret::{SecretItem, SecretStore};

const PHONE: &str = "+44 7700 900123";
const HASH: &str = "00112233445566778899AABBCCDDEEFF";
/// Polls of the store (10 ms apart) before a test gives up.
const STORE_POLLS: usize = 500;

fn script(env: EnvResolution) -> FakeScript {
    let mut script = FakeScript::demo(2, 5);
    script.login.password = None;
    script.env = env;
    script
}

fn submit(api_id: i32) -> Command {
    Command::SubmitCredentials {
        api_id,
        api_hash: ApiHash::parse(HASH).unwrap(),
    }
}

fn entered() -> ApiCredentials {
    ApiCredentials {
        api_id: crate::api::ApiId::new(FAKE_API_ID).unwrap(),
        api_hash: ApiHash::parse(HASH).unwrap(),
    }
}

async fn signed_out_with(env: EnvResolution, setup: Setup) -> Harness {
    let (backend, fake) = FakeBackend::new(script(env));
    let setup = Setup {
        signed_out: true,
        ..setup
    };
    start(backend, fake, setup).await
}

/// Onboarding: no env pair, nothing stored.
async fn onboarding() -> Harness {
    let mut h = signed_out_with(EnvResolution::Absent, Setup::default()).await;
    h.auth(|s| {
        *s == AuthState::NeedCredentials {
            error: None,
            source: None,
        }
    })
    .await;
    h
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    Present,
    Absent,
}

fn is_phone(s: &AuthState) -> bool {
    matches!(s, AuthState::Phone { error: None })
}

impl Harness {
    fn stored_creds(&self) -> Option<ApiCredentials> {
        let bytes = self.store.get(SecretItem::ApiCredentials).unwrap()?;
        crate::credentials::decode(&bytes)
    }

    /// Waits (real time) until the credential item is present or absent;
    /// `false` if it never is.
    async fn stored_becomes(&self, want: Item) -> bool {
        for _ in 0..STORE_POLLS {
            if self.store.contains(SecretItem::ApiCredentials) == (want == Item::Present) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn need_credentials(&mut self, want: Option<CredentialError>) -> AuthState {
        self.auth(|s| matches!(s, AuthState::NeedCredentials { error, .. } if *error == want))
            .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entered_pair_is_persisted_only_after_a_code_request() {
    let mut h = onboarding().await;
    assert_eq!(h.fake.builds(), 0, "no client without credentials");
    h.send(submit(FAKE_API_ID)).await;
    h.until(|e| matches!(e, Event::CredentialSource(Some(CredentialSource::Entered))))
        .await;
    h.auth(is_phone).await;
    assert_eq!(h.fake.builds(), 1);
    assert!(!h.store.contains(SecretItem::ApiCredentials));
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.auth(|s| matches!(s, AuthState::Code { .. })).await;
    assert!(h.stored_becomes(Item::Present).await);
    assert_eq!(h.stored_creds(), Some(entered()));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_pair_is_never_persisted() {
    let env = EnvResolution::Pair(fake_credentials());
    let mut h = signed_out_with(env, Setup::default()).await;
    h.until(|e| matches!(e, Event::CredentialSource(Some(CredentialSource::Env))))
        .await;
    h.auth(is_phone).await;
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.auth(|s| matches!(s, AuthState::Code { .. })).await;
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.auth(AuthState::is_signed_in).await;
    h.online().await;
    assert!(!h.store.contains(SecretItem::ApiCredentials));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn stored_pair_is_used_at_startup() {
    let setup = Setup {
        stored_creds: Some(entered()),
        ..Setup::default()
    };
    let mut h = signed_out_with(EnvResolution::Absent, setup).await;
    h.until(|e| matches!(e, Event::CredentialSource(Some(CredentialSource::Stored))))
        .await;
    h.auth(is_phone).await;
    assert_eq!(h.fake.builds(), 1);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn partial_env_pair_is_an_error_and_blocks_entry() {
    let env = EnvResolution::Partial {
        missing: EnvVar::ApiHash,
    };
    let mut h = signed_out_with(env, Setup::default()).await;
    let want = AuthState::NeedCredentials {
        error: Some(CredentialError::EnvPartial {
            missing: EnvVar::ApiHash,
        }),
        source: Some(CredentialSource::Env),
    };
    assert_eq!(h.auth(|_| true).await, AuthState::Connecting);
    assert_eq!(h.auth(|_| true).await, want);
    h.send(submit(FAKE_API_ID)).await;
    assert_eq!(h.auth(|_| true).await, want, "entering a pair is disabled");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(h.fake.builds(), 0);
    h.stop().await;
}

/// A Keychain read failure is not "nothing stored": entry stays blocked
/// until a retry reads the pair.
#[tokio::test(start_paused = true)]
async fn locked_keychain_is_not_absent() {
    let setup = Setup {
        stored_creds: Some(entered()),
        creds_locked: true,
        ..Setup::default()
    };
    let mut h = signed_out_with(EnvResolution::Absent, setup).await;
    let locked = h
        .need_credentials(Some(CredentialError::KeychainUnavailable))
        .await;
    assert_eq!(
        locked,
        AuthState::NeedCredentials {
            error: Some(CredentialError::KeychainUnavailable),
            source: Some(CredentialSource::Stored),
        }
    );
    h.send(submit(FAKE_API_ID)).await;
    assert_eq!(h.auth(|_| true).await, locked, "no re-onboarding");
    h.faults.heal_item(SecretItem::ApiCredentials);
    h.send(Command::ReloadCredentials).await;
    h.auth(is_phone).await;
    assert_eq!(h.fake.builds(), 1);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn invalid_api_id_is_refused_locally() {
    let mut h = onboarding().await;
    h.send(submit(0)).await;
    h.need_credentials(Some(CredentialError::InvalidApiId))
        .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(h.fake.builds(), 0);
    h.stop().await;
}

/// `API_ID_INVALID` goes back to the credential screen; the session and
/// the Keychain are untouched, and the instance is retired.
#[tokio::test(start_paused = true)]
async fn api_id_invalid_returns_to_credentials() {
    let mut h = onboarding().await;
    h.send(submit(FAKE_INVALID_API_ID)).await;
    h.auth(is_phone).await;
    h.send(Command::SubmitPhone(PHONE.into())).await;
    let state = h
        .need_credentials(Some(CredentialError::ApiIdInvalid))
        .await;
    assert!(matches!(
        state,
        AuthState::NeedCredentials {
            source: Some(CredentialSource::Entered),
            ..
        }
    ));
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(h.fake.retired(), 1);
    assert!(!h.store.contains(SecretItem::ApiCredentials));
    // The session is not logged out or deleted.
    assert!(!h.drain().iter().any(|e| matches!(
        e,
        Event::Auth(AuthState::LoggingOut | AuthState::LoggedOut { .. })
    )));
    h.send(submit(FAKE_API_ID)).await;
    h.auth(is_phone).await;
    assert_eq!(h.fake.builds(), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn api_id_published_flood_has_no_countdown() {
    let mut h = onboarding().await;
    h.send(submit(FAKE_PUBLISHED_API_ID)).await;
    h.auth(is_phone).await;
    h.send(Command::SubmitPhone(PHONE.into())).await;
    let (_, skipped) = h
        .until(|e| {
            matches!(
                e,
                Event::Auth(AuthState::NeedCredentials {
                    error: Some(CredentialError::ApiIdPublishedFlood),
                    ..
                })
            )
        })
        .await;
    assert!(
        !skipped
            .iter()
            .any(|e| matches!(e, Event::Auth(AuthState::FloodWait { .. })))
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn forget_is_refused_while_signed_in() {
    let (backend, fake) = FakeBackend::new(script(EnvResolution::Absent));
    let setup = Setup {
        stored_creds: Some(entered()),
        ..Setup::default()
    };
    let mut h = start(backend, fake, setup).await;
    h.online().await;
    h.send(Command::ForgetStoredCredentials).await;
    h.until(|e| matches!(e, Event::Error(UserError::SignOutFirst)))
        .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(h.store.contains(SecretItem::ApiCredentials));
    assert_eq!(h.fake.builds(), 1);
    h.stop().await;
}

/// Forget deletes the stored item only: an env pair stays in use.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_never_affects_an_env_pair() {
    let setup = Setup {
        stored_creds: Some(entered()),
        ..Setup::default()
    };
    let env = EnvResolution::Pair(fake_credentials());
    let mut h = signed_out_with(env, setup).await;
    h.auth(is_phone).await;
    h.send(Command::ForgetStoredCredentials).await;
    assert!(h.stored_becomes(Item::Absent).await);
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.auth(|s| matches!(s, AuthState::Code { .. })).await;
    assert_eq!(h.fake.builds(), 1, "the instance was not retired");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_a_stored_pair_returns_to_onboarding() {
    let setup = Setup {
        stored_creds: Some(entered()),
        ..Setup::default()
    };
    let mut h = signed_out_with(EnvResolution::Absent, setup).await;
    h.auth(is_phone).await;
    h.send(Command::ForgetStoredCredentials).await;
    h.auth(|s| {
        *s == AuthState::NeedCredentials {
            error: None,
            source: None,
        }
    })
    .await;
    assert!(h.stored_becomes(Item::Absent).await);
    h.stop().await;
}

/// Back from the phone screen while a code request runs: the request is
/// superseded with its pair, and nothing is persisted.
#[tokio::test(start_paused = true)]
async fn stale_send_code_after_back_persists_nothing() {
    let mut h = onboarding().await;
    h.send(submit(FAKE_API_ID)).await;
    h.auth(is_phone).await;
    h.fake
        .inject(Op::RequestCode, Fault::Delay(Duration::from_secs(5)));
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.send(Command::Back).await;
    h.need_credentials(None).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(
        !h.drain()
            .iter()
            .any(|e| matches!(e, Event::Auth(AuthState::Code { .. })))
    );
    assert!(!h.store.contains(SecretItem::ApiCredentials));
    assert_eq!(h.fake.retired(), 1);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn back_navigation_follows_the_screens() {
    let mut h = onboarding().await;
    h.send(submit(FAKE_API_ID)).await;
    h.auth(is_phone).await;
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.auth(|s| matches!(s, AuthState::Code { .. })).await;
    h.send(Command::Back).await;
    h.auth(is_phone).await;
    h.send(Command::Back).await;
    h.need_credentials(None).await;
    h.send(submit(FAKE_API_ID)).await;
    h.auth(is_phone).await;
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.auth(|s| matches!(s, AuthState::Code { .. })).await;
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    h.auth(AuthState::is_signed_in).await;
    // Signed in: Back changes nothing.
    h.send(Command::Back).await;
    assert!(h.auth(|_| true).await.is_signed_in());
    assert_eq!(h.fake.builds(), 2);
    h.stop().await;
}

/// An env pair cannot be changed from the phone screen.
#[tokio::test(start_paused = true)]
async fn back_from_phone_keeps_an_env_pair() {
    let env = EnvResolution::Pair(fake_credentials());
    let mut h = signed_out_with(env, Setup::default()).await;
    h.auth(is_phone).await;
    h.send(Command::Back).await;
    assert!(is_phone(&h.auth(|_| true).await));
    h.stop().await;
}

/// Back while a sign-in that went through has no answer yet: the probe
/// finds the session authorized and the login completes.
#[tokio::test(start_paused = true)]
async fn superseded_sign_in_is_detected_by_the_probe() {
    let env = EnvResolution::Pair(fake_credentials());
    let mut h = signed_out_with(env, Setup::default()).await;
    h.auth(is_phone).await;
    h.send(Command::SubmitPhone(PHONE.into())).await;
    h.auth(|s| matches!(s, AuthState::Code { .. })).await;
    h.fake.inject(Op::SignIn, Fault::CompleteThenHang);
    h.send(Command::SubmitCode(FAKE_CODE.into())).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.send(Command::Back).await;
    h.auth(|s| matches!(s, AuthState::Verifying { .. })).await;
    // Paused time may outrun the login barrier: saved or not, it is a login
    // whose 2FA status is unknown.
    let (AuthState::SignedIn { new_login, .. } | AuthState::SignedInUnsaved { new_login, .. }) =
        h.auth(AuthState::is_signed_in).await
    else {
        unreachable!()
    };
    assert_eq!(new_login, Some(NewLogin { has_password: None }));
    assert_eq!(h.fake.calls(Op::IsAuthorized), 1);
    assert_eq!(h.fake.calls(Op::SignIn), 1);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn sent_code_types_and_unsupported_flows() {
    let env = EnvResolution::Pair(fake_credentials());
    let mut h = signed_out_with(env, Setup::default()).await;
    h.auth(is_phone).await;
    h.send(Command::SubmitPhone("+998 1234567".into())).await;
    let AuthState::Code {
        sent: Some(sent), ..
    } = h.auth(|s| matches!(s, AuthState::Code { .. })).await
    else {
        panic!("no sent-code info");
    };
    assert_eq!(sent.delivery, CodeDelivery::Sms);
    assert_eq!(sent.next, Some(NextDelivery::Call));
    assert_eq!(sent.length, Some(5));
    // The email-setup flow: finish in an official app.
    h.send(Command::Back).await;
    h.auth(is_phone).await;
    h.send(Command::SubmitPhone("+999 1234567".into())).await;
    h.auth(|s| *s == AuthState::OfficialAppRequired).await;
    h.send(Command::Back).await;
    h.auth(is_phone).await;
    h.stop().await;
}
