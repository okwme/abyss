//! The login screens' view of auth state (plan section 4, UI side).
//!
//! Core's `AuthState` has only `SignedIn` so far (tasks 5–6 add the rest), so
//! the router keeps its own view of the plan's states. With `--fake`, login is
//! simulated here ([`demo`]) because core rejects login commands with
//! `Unsupported` until task 6.

use std::time::{Duration, Instant};

use abyss_core::api::{AuthState, UserError};
use gpui::SharedString;

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "fake"),
    expect(
        dead_code,
        reason = "core's AuthState has only SignedIn until tasks 5-6; the --fake demo builds the rest"
    )
)]
pub(crate) enum AuthView {
    Connecting,
    Phone {
        error: Option<UserError>,
    },
    Code {
        phone_hint: SharedString,
        error: Option<UserError>,
    },
    Password {
        hint: Option<SharedString>,
        error: Option<UserError>,
    },
    FloodWait {
        until: Instant,
        resume: Box<AuthView>,
    },
    SignedIn,
}

impl From<AuthState> for AuthView {
    fn from(state: AuthState) -> Self {
        match state {
            AuthState::SignedIn => AuthView::SignedIn,
        }
    }
}

impl AuthView {
    /// Shows `error` on the current login screen, if it belongs there. A
    /// flood wait becomes a countdown that resumes this screen.
    pub(crate) fn with_error(&self, error: UserError, now: Instant) -> Option<AuthView> {
        if let UserError::FloodWait { seconds } = error
            && self.is_login_form()
        {
            return Some(AuthView::FloodWait {
                until: now + Duration::from_secs(u64::from(seconds)),
                resume: Box::new(self.clone()),
            });
        }
        let mut next = self.clone();
        match &mut next {
            AuthView::Phone { error: slot }
            | AuthView::Code { error: slot, .. }
            | AuthView::Password { error: slot, .. } => *slot = Some(error),
            AuthView::Connecting | AuthView::FloodWait { .. } | AuthView::SignedIn => return None,
        }
        Some(next)
    }

    fn is_login_form(&self) -> bool {
        matches!(
            self,
            AuthView::Phone { .. } | AuthView::Code { .. } | AuthView::Password { .. }
        )
    }

    /// The same screen without its error, as shown again after a retry.
    pub(crate) fn cleared(&self) -> AuthView {
        let mut next = self.clone();
        if let AuthView::Phone { error }
        | AuthView::Code { error, .. }
        | AuthView::Password { error, .. } = &mut next
        {
            *error = None;
        }
        next
    }
}

/// Masks a phone number for display: country prefix and last two digits,
/// e.g. `+44 •••• ••12`. Short input is masked entirely.
#[cfg(any(feature = "fake", test))]
pub(crate) fn mask_phone(phone: &str) -> SharedString {
    let chars: Vec<char> = phone.chars().collect();
    let n = chars.len();
    if n <= 5 {
        return "•".repeat(n).into();
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars.iter().skip(n - 2).collect();
    let hidden = n - 5;
    let first = hidden.min(4);
    let rest = hidden - first;
    format!("{head} {} {}{tail}", "•".repeat(first), "•".repeat(rest)).into()
}

/// Simulated login for `--fake`: any phone with 5+ digits, any 5+ digit code
/// (`00000` triggers a flood wait), any non-empty password.
#[cfg(feature = "fake")]
pub(crate) mod demo {
    use super::{AuthView, Duration, Instant, UserError, mask_phone};

    pub(crate) const FLOOD_CODE: &str = "00000";
    const FLOOD: Duration = Duration::from_secs(10);
    const MIN_DIGITS: usize = 5;

    pub(crate) fn phone(phone: &str) -> AuthView {
        if phone.chars().filter(char::is_ascii_digit).count() < MIN_DIGITS {
            return AuthView::Phone {
                error: Some(UserError::Rejected),
            };
        }
        AuthView::Code {
            phone_hint: mask_phone(phone),
            error: None,
        }
    }

    pub(crate) fn code(current: &AuthView, code: &str, now: Instant) -> AuthView {
        let AuthView::Code { phone_hint, .. } = current else {
            return current.clone();
        };
        if code == FLOOD_CODE {
            return AuthView::FloodWait {
                until: now + FLOOD,
                resume: Box::new(current.cleared()),
            };
        }
        if code.len() < MIN_DIGITS {
            return AuthView::Code {
                phone_hint: phone_hint.clone(),
                error: Some(UserError::Rejected),
            };
        }
        AuthView::Password {
            hint: Some(crate::ui::copy::DEMO_PASSWORD_HINT.into()),
            error: None,
        }
    }

    pub(crate) fn password(current: &AuthView, len: usize) -> AuthView {
        match current {
            AuthView::Password { .. } if len == 0 => current
                .with_error(UserError::Rejected, Instant::now())
                .unwrap_or_else(|| current.clone()),
            AuthView::Password { .. } => AuthView::SignedIn,
            _ => current.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_phone_numbers() {
        assert_eq!(mask_phone("+442079460012"), "+44 •••• ••••12");
        assert_eq!(mask_phone("+15550100"), "+15 •••• 00");
        assert_eq!(mask_phone("+155501"), "+15 •• 01");
        assert_eq!(mask_phone("12345"), "•••••");
        assert_eq!(mask_phone(""), "");
    }

    #[test]
    fn errors_land_on_the_current_form() {
        let now = Instant::now();
        let phone = AuthView::Phone { error: None };
        assert_eq!(
            phone.with_error(UserError::Rejected, now),
            Some(AuthView::Phone {
                error: Some(UserError::Rejected)
            })
        );
        match phone.with_error(UserError::FloodWait { seconds: 30 }, now) {
            Some(AuthView::FloodWait { until, resume }) => {
                assert_eq!(until - now, Duration::from_secs(30));
                assert_eq!(*resume, phone);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(AuthView::SignedIn.with_error(UserError::Busy, now), None);
    }

    #[test]
    fn core_signed_in_maps() {
        assert_eq!(AuthView::from(AuthState::SignedIn), AuthView::SignedIn);
    }

    #[cfg(feature = "fake")]
    #[test]
    fn demo_flow_reaches_signed_in() {
        let now = Instant::now();
        assert!(matches!(
            demo::phone("+1 2"),
            AuthView::Phone { error: Some(_) }
        ));
        let code = demo::phone("+15550100");
        assert!(matches!(code, AuthView::Code { error: None, .. }));
        assert!(matches!(
            demo::code(&code, "12", now),
            AuthView::Code { error: Some(_), .. }
        ));
        match demo::code(&code, demo::FLOOD_CODE, now) {
            AuthView::FloodWait { resume, .. } => assert_eq!(*resume, code),
            other => panic!("{other:?}"),
        }
        let password = demo::code(&code, "12345", now);
        assert!(matches!(password, AuthView::Password { error: None, .. }));
        assert!(matches!(
            demo::password(&password, 0),
            AuthView::Password { error: Some(_), .. }
        ));
        assert_eq!(demo::password(&password, 3), AuthView::SignedIn);
    }
}
