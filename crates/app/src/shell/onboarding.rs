//! Onboarding and login actions of the session (plan section 17.3): the
//! UI-only welcome and guide steps, credential entry, Back, resend, the Done
//! screen and Logout. Core decides everything else.

use abyss_core::api::{ApiHash, ApiId, Command};
use gpui::{Context, Window};

use super::Root;
use super::auth::{AuthView, Back, Onboarding, Stored, back};
use super::session::Session;
use crate::ui::copy::onboarding as copy;
use crate::ui::text_input::Mask;

/// What a credential entry holds, checked like core checks it.
pub(crate) fn check_entry(api_id: &str, api_hash: &str) -> Result<(ApiId, ApiHash), &'static str> {
    let id = ApiId::parse(api_id).map_err(copy::api_id_invalid)?;
    let hash = ApiHash::parse(api_hash).map_err(|_| copy::HASH_INVALID)?;
    Ok((id, hash))
}

impl Session {
    fn show_step(&mut self, step: Onboarding, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.entry_error = None;
        self.set_auth(AuthView::Onboarding(step), window, cx);
    }

    /// Welcome → the my.telegram.org guide.
    pub(super) fn connect(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.auth == AuthView::Onboarding(Onboarding::Welcome) {
            self.show_step(Onboarding::Guide, window, cx);
        }
    }

    /// Guide → credential entry.
    pub(super) fn have_credentials(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.auth == AuthView::Onboarding(Onboarding::Guide) {
            let step = Onboarding::Entry {
                error: None,
                stored: Stored::No,
            };
            self.show_step(step, window, cx);
        }
    }

    /// Opens the hardcoded my.telegram.org page in the system browser: the
    /// only URL onboarding opens directly.
    pub(super) fn open_site(cx: &mut Context<'_, Root>) {
        cx.open_url(copy::MY_TELEGRAM_URL);
    }

    pub(super) fn toggle_hash_mask(&mut self, cx: &mut Context<'_, Root>) {
        self.api_hash.update(cx, |input, cx| {
            let next = match input.mask() {
                Mask::Hidden => Mask::Shown,
                Mask::Shown => Mask::Hidden,
            };
            input.set_mask(next, cx);
        });
        cx.notify();
    }

    /// Validates inline, then sends `SubmitCredentials`. The fields are
    /// cleared once sent.
    pub(super) fn submit_credentials(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Onboarding(Onboarding::Entry { .. })) {
            return;
        }
        let id = self.api_id.read(cx).text().to_owned();
        let hash = zeroize::Zeroizing::new(self.api_hash.read(cx).text().to_owned());
        match check_entry(&id, &hash) {
            Ok((api_id, api_hash)) => {
                self.entry_error = None;
                self.api_id.update(cx, |input, cx| input.clear(window, cx));
                self.api_hash
                    .update(cx, |input, cx| input.clear(window, cx));
                let api_id = api_id.get();
                self.send(Command::SubmitCredentials { api_id, api_hash }, window, cx);
            }
            Err(problem) => self.entry_error = Some(problem.into()),
        }
        cx.notify();
    }

    /// The saved pair: use it (reload) or forget it.
    pub(super) fn use_saved(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.send(Command::ReloadCredentials, window, cx);
    }

    pub(super) fn forget_saved(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.send(Command::ForgetStoredCredentials, window, cx);
    }

    pub(super) fn back(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        match back(&self.auth, self.source) {
            Back::None => {}
            Back::Local(view) => {
                self.entry_error = None;
                self.set_auth(view, window, cx);
            }
            Back::Core => self.send(Command::Back, window, cx),
        }
    }

    pub(super) fn resend_code(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if matches!(self.auth, AuthView::Code { .. }) {
            self.send(Command::ResendCode, window, cx);
        }
    }

    /// Done → the chats. The Done screen already said whether the session
    /// was saved.
    pub(super) fn open_chats(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if matches!(self.auth, AuthView::Done { .. }) {
            self.set_auth(AuthView::SignedIn, window, cx);
        }
    }

    pub(super) fn logout(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if matches!(self.auth, AuthView::SignedIn | AuthView::Done { .. }) {
            self.send(Command::Logout, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_is_checked_like_core() {
        let hash = "0123456789ABCDEF0123456789abcdef";
        let (id, h) = check_entry(" 42 ", hash).unwrap();
        assert_eq!(id.get(), 42);
        assert_eq!(h.expose(), hash.to_lowercase());
        assert_eq!(
            check_entry("", hash).unwrap_err(),
            copy::api_id_invalid(abyss_core::api::ApiIdError::Empty)
        );
        assert_eq!(
            check_entry("99999999999", hash).unwrap_err(),
            copy::api_id_invalid(abyss_core::api::ApiIdError::TooLong)
        );
        assert_eq!(check_entry("42", "abc").unwrap_err(), copy::HASH_INVALID);
    }
}
