//! The raw login adapter (task 6b): the sent-code mapper for every
//! `auth.SentCodeType`, the auth error table, and `auth.sendCode` over a
//! scripted invoker (request shape, `api_hash`, a login-time `303`).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grammers_client::InvocationError;
use grammers_client::sender::{ReadError, RpcError};
use grammers_client::session::Session as _;
use grammers_client::tl::{self, Deserializable as _, Serializable as _};

use crate::api::{ApiCredentials, CodeDelivery, CredentialError, NextDelivery};
use crate::backend::grammers::auth::{
    GrammersLoginToken, MappedCode, SignInFailure, auth_error, is_authorized, login_state,
    map_sent_code, send_code, sign_in, sign_in_failure,
};
use crate::backend::grammers::invoke::Invoke;
use crate::backend::{AuthError, BackendError, SentCode, SessionSnapshot};
use crate::session::AbyssSession;

use tl::enums::auth::SentCodeType as T;
use tl::types::auth as ty;

type Reply = Result<Vec<u8>, InvocationError>;

#[derive(Default)]
struct Mock {
    script: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Vec<u8>>>,
}

impl Invoke for Arc<Mock> {
    fn invoke<R>(
        &self,
        request: &R,
    ) -> impl Future<Output = Result<R::Return, InvocationError>> + Send
    where
        R: tl::RemoteCall + Send + Sync,
        R::Return: Send,
    {
        self.requests.lock().unwrap().push(request.to_bytes());
        let reply = self.script.lock().unwrap().pop_front();
        async move {
            match reply {
                Some(Ok(bytes)) => R::Return::from_bytes(&bytes)
                    .map_err(|e| InvocationError::from(ReadError::from(e))),
                Some(Err(e)) => Err(e),
                None => Err(InvocationError::Dropped("script exhausted")),
            }
        }
    }
}

fn mock(replies: Vec<Reply>) -> Arc<Mock> {
    Arc::new(Mock {
        script: Mutex::new(replies.into()),
        requests: Mutex::default(),
    })
}

fn rpc(code: i32, name: &str, value: Option<u32>) -> InvocationError {
    InvocationError::Rpc(RpcError {
        code,
        name: name.to_owned(),
        value,
        caused_by: None,
    })
}

fn code(
    r#type: T,
    next: Option<tl::enums::auth::CodeType>,
    timeout: Option<i32>,
) -> tl::enums::auth::SentCode {
    tl::enums::auth::SentCode::Code(ty::SentCode {
        r#type,
        phone_code_hash: "h4sh".into(),
        next_type: next,
        timeout,
    })
}

fn mapped(delivery: CodeDelivery, length: Option<u8>) -> MappedCode {
    MappedCode {
        hash: "h4sh".into(),
        sent: SentCode {
            delivery,
            length,
            next: None,
            timeout: None,
        },
    }
}

#[test]
fn every_sent_code_type_is_mapped() {
    let table: Vec<(T, Result<MappedCode, AuthError>)> = vec![
        (
            T::App(ty::SentCodeTypeApp { length: 5 }),
            Ok(mapped(CodeDelivery::App, Some(5))),
        ),
        (
            T::Sms(ty::SentCodeTypeSms { length: 6 }),
            Ok(mapped(CodeDelivery::Sms, Some(6))),
        ),
        (
            T::Call(ty::SentCodeTypeCall { length: 5 }),
            Ok(mapped(CodeDelivery::Call, Some(5))),
        ),
        (
            T::FlashCall(ty::SentCodeTypeFlashCall {
                pattern: "+44 7700 9*****".into(),
            }),
            Ok(mapped(
                CodeDelivery::FlashCall {
                    pattern: "+44 7700 9*****".into(),
                },
                None,
            )),
        ),
        (
            T::MissedCall(ty::SentCodeTypeMissedCall {
                prefix: "+44 7700".into(),
                length: 4,
            }),
            Ok(mapped(
                CodeDelivery::MissedCall {
                    prefix: "+44 7700".into(),
                },
                Some(4),
            )),
        ),
        (
            T::FragmentSms(ty::SentCodeTypeFragmentSms {
                url: "https://fragment.com/number/888".into(),
                length: 5,
            }),
            Ok(mapped(
                CodeDelivery::Fragment {
                    url: Some("https://fragment.com/number/888".into()),
                },
                Some(5),
            )),
        ),
    ];
    for (t, want) in table {
        assert_eq!(map_sent_code(code(t, None, None)), want);
    }
}

/// Round 11, finding 5: per-type info is kept and sanitized. A flash-call
/// pattern keeps its full length (the code is the whole number); a
/// Fragment URL that is not plain `https://` is dropped, never passed on.
#[test]
fn delivery_text_is_kept_and_sanitized() {
    let flash = T::FlashCall(ty::SentCodeTypeFlashCall {
        pattern: "+1 (555) 01*-****\u{202e}x".into(),
    });
    let sent = map_sent_code(code(flash, None, None)).unwrap().sent;
    let CodeDelivery::FlashCall { pattern } = &sent.delivery else {
        panic!("{sent:?}");
    };
    assert_eq!(pattern, "+1 555 01*-****");
    let info = crate::api::SentCodeInfo {
        delivery: sent.delivery.clone(),
        length: sent.length,
        next: None,
        resend_at: None,
    };
    assert_eq!(info.max_code_digits(), 11, "not truncated to 8");
    for bad in ["http://fragment.com", "javascript:alert(1)", "https://a b"] {
        let fragment = T::FragmentSms(ty::SentCodeTypeFragmentSms {
            url: bad.into(),
            length: 5,
        });
        let sent = map_sent_code(code(fragment, None, None)).unwrap().sent;
        assert_eq!(sent.delivery, CodeDelivery::Fragment { url: None }, "{bad}");
    }
}

/// Round 19, finding 6: only an unambiguous Fragment address is kept;
/// misleading hosts are dropped (the screen then shows no address).
#[test]
fn fragment_urls_are_parsed_strictly() {
    use crate::backend::grammers::auth::safe_url;
    for good in [
        "https://fragment.com/number/888",
        "https://fragment.com",
        "https://fragment.com:443/number/888?x=1#y",
        "https://www.fragment.com/number/888",
    ] {
        assert_eq!(safe_url(good).as_deref(), Some(good), "{good}");
    }
    let long_path = format!("https://fragment.com/{}", "a".repeat(235));
    assert_eq!(long_path.len(), 256);
    assert!(safe_url(&long_path).is_some(), "256 bytes fit");
    let too_long = format!("{long_path}a");
    let bad = [
        // Userinfo and suffix confusion.
        "https://fragment.com@evil.example/",
        "https://user:pw@fragment.com/",
        "https://fragment.com.evil.example/",
        "https://evilfragment.com/",
        "https://fragment.com\\@evil.example/",
        // Ports, schemes.
        "https://fragment.com:8443/",
        "https://fragment.com:/",
        "http://fragment.com/",
        "javascript:alert(1)",
        // IDN, punycode, case tricks.
        "https://fragмent.com/",
        "https://xn--frgment-9ya.com/",
        "https://xn--80ak6aa92e.fragment.com/",
        "https://FRAGMENT.COM/",
        // Controls, bidi marks, spaces.
        "https://fragment.com/\u{202e}moc.live",
        "https://fragment.com/\u{0}",
        "https://fragment.com/a b",
        "https://fragment.com\t/",
        too_long.as_str(),
    ];
    for url in bad {
        assert_eq!(safe_url(url), None, "{url:?}");
    }
}

/// Round 19, finding 7: `phone_code_hash` never reaches a log.
#[test]
fn mapped_code_debug_redacts_the_hash() {
    let sentinel = "SENTINEL-CODE-HASH-7f3a";
    let mapped = MappedCode {
        hash: sentinel.into(),
        sent: mapped(CodeDelivery::App, Some(5)).sent,
    };
    let shown = format!("{mapped:?} {mapped:#?}");
    assert!(!shown.contains(sentinel), "{shown}");
    assert!(shown.contains("<redacted>"), "{shown}");
}

/// Round 11, finding 3: `updates.getState` after an authorization. A
/// revocation in between is propagated (not swallowed as "no state"); only
/// transport and flood failures are best-effort; anything else is
/// ambiguous, so core reconciles.
#[test]
fn login_state_propagates_revocation() {
    assert_eq!(
        login_state(Err(rpc(401, "SESSION_REVOKED", None))).unwrap_err(),
        AuthError::Backend(BackendError::Unauthorized)
    );
    assert_eq!(
        login_state(Err(rpc(401, "AUTH_KEY_UNREGISTERED", None))).unwrap_err(),
        AuthError::Backend(BackendError::Unauthorized)
    );
    assert_eq!(login_state(Err(rpc(420, "FLOOD_WAIT", Some(5)))), Ok(None));
    assert_eq!(
        login_state(Err(InvocationError::Dropped("gone"))),
        Ok(None),
        "transport"
    );
    assert_eq!(
        login_state(Err(rpc(400, "SOMETHING_ELSE", None))).unwrap_err(),
        AuthError::Backend(BackendError::Transient)
    );
}

/// Email, Firebase and word/phrase codes need an official app.
#[test]
fn unsupported_sent_code_types_need_an_official_app() {
    let official = Err(AuthError::OfficialAppRequired);
    let table: Vec<(T, Result<MappedCode, AuthError>)> = vec![
        (
            T::EmailCode(ty::SentCodeTypeEmailCode {
                apple_signin_allowed: false,
                google_signin_allowed: false,
                email_pattern: "a***@b".into(),
                length: 6,
                reset_available_period: None,
                reset_pending_date: None,
            }),
            official.clone(),
        ),
        (
            T::SetUpEmailRequired(ty::SentCodeTypeSetUpEmailRequired {
                apple_signin_allowed: false,
                google_signin_allowed: false,
            }),
            official.clone(),
        ),
        (
            T::FirebaseSms(ty::SentCodeTypeFirebaseSms {
                nonce: None,
                play_integrity_project_id: None,
                play_integrity_nonce: None,
                receipt: None,
                push_timeout: None,
                length: 5,
            }),
            official.clone(),
        ),
        (
            T::SmsWord(ty::SentCodeTypeSmsWord { beginning: None }),
            official.clone(),
        ),
        (
            T::SmsPhrase(ty::SentCodeTypeSmsPhrase { beginning: None }),
            official,
        ),
    ];
    for (t, want) in table {
        assert_eq!(map_sent_code(code(t, None, None)), want);
    }
}

#[test]
fn next_type_and_timeout_are_kept_and_bounded() {
    use tl::enums::auth::CodeType as C;
    let app = || T::App(ty::SentCodeTypeApp { length: 5 });
    for (next, want) in [
        (C::Sms, NextDelivery::Sms),
        (C::Call, NextDelivery::Call),
        (C::FlashCall, NextDelivery::FlashCall),
        (C::MissedCall, NextDelivery::MissedCall),
        (C::FragmentSms, NextDelivery::FragmentSms),
    ] {
        let got = map_sent_code(code(app(), Some(next), Some(60))).unwrap();
        assert_eq!(got.sent.next, Some(want));
        assert_eq!(got.sent.timeout, Some(Duration::from_secs(60)));
    }
    let long = map_sent_code(code(app(), None, Some(i32::MAX))).unwrap();
    assert_eq!(long.sent.timeout, Some(Duration::from_hours(24)));
    let negative = map_sent_code(code(app(), None, Some(-5))).unwrap();
    assert_eq!(negative.sent.timeout, None);
    let zero_length = map_sent_code(code(T::Sms(ty::SentCodeTypeSms { length: 0 }), None, None));
    assert_eq!(zero_length.unwrap().sent.length, None);
}

#[test]
fn sent_code_success_and_payment_need_an_official_app() {
    let success = tl::enums::auth::SentCode::Success(ty::SentCodeSuccess {
        authorization: tl::enums::auth::Authorization::SignUpRequired(
            ty::AuthorizationSignUpRequired {
                terms_of_service: None,
            },
        ),
    });
    assert_eq!(map_sent_code(success), Err(AuthError::OfficialAppRequired));
    let paid = tl::enums::auth::SentCode::PaymentRequired(ty::SentCodePaymentRequired {
        store_product: String::new(),
        phone_code_hash: "h".into(),
        support_email_address: String::new(),
        support_email_subject: String::new(),
        premium_days: 0,
        currency: "EUR".into(),
        amount: 0,
    });
    assert_eq!(map_sent_code(paid), Err(AuthError::OfficialAppRequired));
}

#[test]
fn auth_error_table() {
    let table: &[(i32, &str, Option<u32>, AuthError)] = &[
        (
            400,
            "API_ID_INVALID",
            None,
            AuthError::Credentials(CredentialError::ApiIdInvalid),
        ),
        (
            400,
            "API_ID_PUBLISHED_FLOOD",
            None,
            AuthError::Credentials(CredentialError::ApiIdPublishedFlood),
        ),
        (400, "PHONE_NUMBER_INVALID", None, AuthError::PhoneInvalid),
        (400, "PHONE_NUMBER_BANNED", None, AuthError::PhoneBanned),
        (400, "PHONE_NUMBER_FLOOD", None, AuthError::PhoneFlood),
        (400, "PHONE_PASSWORD_FLOOD", None, AuthError::PhoneFlood),
        (
            420,
            "FLOOD_WAIT",
            Some(31),
            AuthError::Backend(BackendError::FloodWait { seconds: 31 }),
        ),
        (
            401,
            "AUTH_KEY_UNREGISTERED",
            None,
            AuthError::Backend(BackendError::Unauthorized),
        ),
        (
            401,
            "SESSION_REVOKED",
            None,
            AuthError::Backend(BackendError::Unauthorized),
        ),
        (
            400,
            "PASSWORD_HASH_INVALID",
            None,
            AuthError::Backend(BackendError::Rejected),
        ),
        (
            500,
            "INTERNAL",
            None,
            AuthError::Backend(BackendError::Transient),
        ),
    ];
    for (code, name, value, want) in table {
        assert_eq!(auth_error(&rpc(*code, name, *value)), *want, "{name}");
    }
    let io = InvocationError::Io(std::io::Error::other("reset"));
    assert_eq!(auth_error(&io), AuthError::Backend(BackendError::Transient));
}

#[test]
fn sign_in_failures() {
    let table: &[(i32, &str, SignInFailure)] = &[
        (
            401,
            "SESSION_PASSWORD_NEEDED",
            SignInFailure::PasswordNeeded,
        ),
        (400, "PHONE_CODE_INVALID", SignInFailure::InvalidCode),
        (400, "PHONE_CODE_EXPIRED", SignInFailure::InvalidCode),
        (400, "PHONE_CODE_EMPTY", SignInFailure::InvalidCode),
        (
            400,
            "PHONE_NUMBER_UNOCCUPIED",
            SignInFailure::SignUpRequired,
        ),
        (
            420,
            "FLOOD_WAIT",
            SignInFailure::Other(AuthError::Backend(BackendError::FloodWait { seconds: 1 })),
        ),
        (
            401,
            "AUTH_KEY_UNREGISTERED",
            SignInFailure::Other(AuthError::Backend(BackendError::Unauthorized)),
        ),
    ];
    for (code, name, want) in table {
        assert_eq!(sign_in_failure(&rpc(*code, name, Some(1))), *want, "{name}");
    }
}

#[tokio::test]
async fn is_authorized_reads_401_as_not_signed_in() {
    let state = tl::enums::updates::State::State(tl::types::updates::State {
        pts: 1,
        qts: 0,
        date: 0,
        seq: 0,
        unread_count: 0,
    });
    let inv = mock(vec![
        Ok(state.to_bytes()),
        Err(rpc(401, "AUTH_KEY_UNREGISTERED", None)),
        Err(InvocationError::Io(std::io::Error::other("reset"))),
    ]);
    assert_eq!(is_authorized(&inv).await, Ok(true));
    assert_eq!(is_authorized(&inv).await, Ok(false));
    // A failed call is not an answer.
    assert_eq!(is_authorized(&inv).await, Err(BackendError::Transient));
}

fn creds() -> ApiCredentials {
    ApiCredentials {
        api_id: crate::api::ApiId::new(4_242_424).unwrap(),
        api_hash: crate::api::ApiHash::parse("feedfacefeedfacefeedfacefeedface").unwrap(),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `auth.sendCode` carries the instance's pair; a `303` moves the home DC
/// and asks once more on it.
#[tokio::test]
async fn send_code_uses_the_pair_and_follows_one_migration() {
    let sent = code(T::Sms(ty::SentCodeTypeSms { length: 5 }), None, Some(30));
    let inv = mock(vec![
        Err(rpc(303, "PHONE_MIGRATE", Some(4))),
        Ok(sent.to_bytes()),
    ]);
    let session = AbyssSession::from_snapshot(SessionSnapshot::detached()).unwrap();
    let old = session.home_dc_id().unwrap();
    let culled = Mutex::new(Vec::new());
    let disconnect = |dc| culled.lock().unwrap().push(dc);
    let got = send_code(&inv, &session, &creds(), "+447700900123", disconnect)
        .await
        .unwrap();
    assert_eq!(got.sent.delivery, CodeDelivery::Sms);
    assert_eq!(got.sent.timeout, Some(Duration::from_secs(30)));
    assert_eq!(session.home_dc_id().unwrap(), 4);
    assert_eq!(*culled.lock().unwrap(), vec![old]);
    let requests = inv.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for req in &requests {
        assert!(contains(req, b"feedfacefeedfacefeedfacefeedface"));
        assert!(contains(req, &4_242_424_i32.to_le_bytes()));
    }
    assert!(format!("{:?}", got.token).contains("redacted"));
}

#[tokio::test]
async fn send_code_errors_are_typed_and_not_retried() {
    let inv = mock(vec![Err(rpc(400, "API_ID_INVALID", None))]);
    let session = AbyssSession::from_snapshot(SessionSnapshot::detached()).unwrap();
    let res = send_code(&inv, &session, &creds(), "+447700900123", |_| {}).await;
    assert_eq!(
        res.map(|_| ()),
        Err(AuthError::Credentials(CredentialError::ApiIdInvalid))
    );
    assert_eq!(inv.requests.lock().unwrap().len(), 1);
}

/// Round 11, finding 3: `auth.signIn` succeeds, then the key is revoked
/// before `updates.getState`. The login reports the revocation (so core
/// tears the session down) instead of swallowing it as "no update state".
#[tokio::test]
async fn revocation_between_authorization_and_completion_is_reported() {
    let authorized = tl::enums::auth::Authorization::Authorization(ty::Authorization {
        setup_password_required: false,
        otherwise_relogin_days: None,
        tmp_sessions: None,
        future_auth_token: None,
        user: tl::enums::User::Empty(tl::types::UserEmpty { id: 7 }),
    });
    let inv = mock(vec![
        Ok(authorized.to_bytes()),
        Err(rpc(401, "SESSION_REVOKED", None)),
    ]);
    let session = AbyssSession::from_snapshot(SessionSnapshot::detached()).unwrap();
    let token = GrammersLoginToken::for_test("+447700900123", "h4sh");
    let res = sign_in(&inv, &session, &token, "12345").await;
    assert_eq!(
        res.map(|_| ()),
        Err(AuthError::Backend(BackendError::Unauthorized))
    );
}
