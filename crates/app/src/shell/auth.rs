//! The login screens' view of core's auth state (plan section 4, UI side).
//!
//! Core's [`AuthState`] is the source of truth; [`project`] maps it onto the
//! screens the view draws. States without a screen of their own (sign-up
//! required, logout, an unsaved login) land on the nearest screen plus a
//! notice.

use std::time::{Duration, Instant, SystemTime};

use abyss_core::api::{AuthState, UserError};
use gpui::SharedString;

use crate::ui::copy;

#[derive(Clone, Debug, PartialEq, Eq)]
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

/// One core auth state, as the UI shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Projection {
    pub(crate) view: AuthView,
    /// Shown as a toast, for states without a screen of their own.
    pub(crate) notice: Option<SharedString>,
    /// The password token is gone: the next submission asks core for new
    /// parameters instead of sending the password.
    pub(crate) needs_params: bool,
}

impl Projection {
    fn view(view: AuthView) -> Self {
        Self {
            view,
            notice: None,
            needs_params: false,
        }
    }

    fn with_notice(view: AuthView, notice: SharedString) -> Self {
        Self {
            view,
            notice: Some(notice),
            needs_params: false,
        }
    }
}

/// Maps core's state onto a screen. `hint` is the last password hint shown,
/// kept for `PasswordTokenUnavailable`, which carries none.
pub(crate) fn project(state: AuthState, hint: Option<SharedString>, now: Instant) -> Projection {
    match state {
        AuthState::Connecting => Projection::view(AuthView::Connecting),
        AuthState::NeedCredentials { error } => Projection::view(AuthView::Phone {
            error: error.map(UserError::Credentials),
        }),
        AuthState::Phone { error } => Projection::view(AuthView::Phone { error }),
        AuthState::Code { phone_hint, error } => Projection::view(AuthView::Code {
            phone_hint: phone_hint.into(),
            error,
        }),
        AuthState::Password { hint, error } => Projection::view(AuthView::Password {
            hint: hint.map(Into::into),
            error,
        }),
        AuthState::PasswordTokenUnavailable { error } => Projection {
            view: AuthView::Password {
                hint,
                error: Some(error),
            },
            notice: Some(copy::PASSWORD_PARAMS.into()),
            needs_params: true,
        },
        AuthState::FloodWait { until, resume } => {
            let left = until
                .duration_since(SystemTime::now())
                .unwrap_or(Duration::ZERO);
            let resume = project(*resume, hint, now).view;
            Projection::view(AuthView::FloodWait {
                until: now + left,
                resume: Box::new(resume),
            })
        }
        AuthState::SignUpRequired => Projection::view(AuthView::Phone {
            error: Some(UserError::SignUpRequired),
        }),
        AuthState::SignedIn { .. } => Projection::view(AuthView::SignedIn),
        AuthState::SignedInUnsaved { .. } => {
            Projection::with_notice(AuthView::SignedIn, copy::SIGNED_IN_UNSAVED.into())
        }
        AuthState::LoggingOut => {
            Projection::with_notice(AuthView::Connecting, copy::LOGGING_OUT.into())
        }
        AuthState::LoggedOut { remote, local } => {
            Projection::with_notice(AuthView::Connecting, copy::logged_out(remote, local))
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

#[cfg(test)]
mod tests {
    use abyss_core::api::{CredentialError, LocalDeletion, RemoteLogout};

    use super::*;

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
    fn core_states_project_onto_screens() {
        let now = Instant::now();
        let signed_in = AuthState::SignedIn {
            display_name: "A".into(),
        };
        assert_eq!(project(signed_in, None, now).view, AuthView::SignedIn);
        let unsaved = project(
            AuthState::SignedInUnsaved {
                display_name: "A".into(),
            },
            None,
            now,
        );
        assert_eq!(unsaved.view, AuthView::SignedIn);
        assert!(unsaved.notice.is_some());
        let creds = AuthState::NeedCredentials {
            error: Some(CredentialError::ApiIdInvalid),
        };
        assert_eq!(
            project(creds, None, now).view,
            AuthView::Phone {
                error: Some(UserError::Credentials(CredentialError::ApiIdInvalid))
            }
        );
        let gone = AuthState::LoggedOut {
            remote: RemoteLogout::Revoked,
            local: LocalDeletion::Deleted,
        };
        assert!(project(gone, None, now).notice.is_some());
    }

    #[test]
    fn token_unavailable_keeps_the_hint_and_asks_for_params() {
        let state = AuthState::PasswordTokenUnavailable {
            error: UserError::Network,
        };
        let p = project(state, Some("h".into()), Instant::now());
        assert!(p.needs_params);
        assert_eq!(
            p.view,
            AuthView::Password {
                hint: Some("h".into()),
                error: Some(UserError::Network)
            }
        );
    }

    #[test]
    fn flood_wait_counts_down_to_the_resume_screen() {
        let now = Instant::now();
        let state = AuthState::FloodWait {
            until: SystemTime::now() + Duration::from_secs(60),
            resume: Box::new(AuthState::Phone { error: None }),
        };
        match project(state, None, now).view {
            AuthView::FloodWait { until, resume } => {
                assert!(until - now <= Duration::from_secs(60));
                assert!(until - now > Duration::from_secs(50));
                assert_eq!(*resume, AuthView::Phone { error: None });
            }
            other => panic!("{other:?}"),
        }
    }
}
