//! User-facing strings for the startup, auth and shutdown screens, in one
//! place for review and later localization. Nothing here may include user
//! data beyond what the caller passes in for display (a masked phone hint, a
//! server-provided password hint).

use abyss_core::StartError;
use abyss_core::api::{LocalDeletion, RemoteLogout, StorageError, UserError};
use gpui::SharedString;

use crate::launch::LaunchError;

pub(crate) mod onboarding;

pub(crate) use onboarding::PHONE_DETAIL;

pub(crate) const CONNECTING: &str = "Connecting…";
pub(crate) const CONTINUE: &str = "Continue";

pub(crate) const PHONE_TITLE: &str = "Sign in to Telegram";
pub(crate) const PHONE_PLACEHOLDER: &str = "Phone number, e.g. +44 20 7946 0000";

pub(crate) const CODE_TITLE: &str = "Enter the code";
pub(crate) const CODE_PLACEHOLDER: &str = "Code";

pub(crate) const PASSWORD_TITLE: &str = "Two-step verification";
pub(crate) const PASSWORD_PLACEHOLDER: &str = "Password";
/// `--fake`: the demo account's login code, shown when the code screen opens.
#[cfg(feature = "fake")]
pub(crate) const FAKE_CODE_HINT: &str = "Demo account: the code is 12345.";
/// The password token was used up; Enter fetches new sign-in parameters.
pub(crate) const PASSWORD_PARAMS: &str =
    "Press Enter to get new sign-in parameters, then enter your password again.";
pub(crate) const SIGNED_IN_UNSAVED: &str =
    "Signed in, but the session couldn't be saved. You'll need to sign in again next time.";

/// The result of a logout, as a notice.
pub(crate) fn logged_out(remote: RemoteLogout, local: LocalDeletion) -> SharedString {
    let remote = match remote {
        RemoteLogout::Revoked | RemoteLogout::NotAttempted => "",
        RemoteLogout::Failed(_) | RemoteLogout::TimedOut => {
            " Telegram didn't confirm it: end the session from Active Sessions on another device."
        }
    };
    let local = match local {
        LocalDeletion::Deleted => "",
        LocalDeletion::Failed(_) => {
            " Some local data couldn't be deleted; signing out again retries."
        }
    };
    format!("Signed out.{remote}{local}").into()
}

pub(crate) fn password_detail(hint: Option<&str>) -> SharedString {
    match hint {
        Some(h) => format!("Enter your cloud password (hint: {h}).").into(),
        None => "Enter your cloud password.".into(),
    }
}

pub(crate) const FLOOD_TITLE: &str = "Too many attempts";

pub(crate) fn flood_detail(seconds_left: u64) -> SharedString {
    format!("Telegram asks us to wait. Try again in {seconds_left} s.").into()
}

pub(crate) const COMPOSER_PLACEHOLDER: &str = "Write a message…";
pub(crate) const CORE_STOPPED: &str = "abyss's core stopped unexpectedly.";
pub(crate) const CORE_GONE: &str = "abyss's core has stopped.";

pub(crate) const CLOSING: &str = "Closing…";
pub(crate) const UNCONFIRMED_TITLE: &str = "Couldn't confirm the session was saved";
pub(crate) const UNCONFIRMED_DETAIL: &str = "You may need to sign in again next time.";
pub(crate) const LOGOUT_UNFINISHED_TITLE: &str = "Sign-out didn't finish";
pub(crate) const LOGOUT_UNFINISHED_DETAIL: &str = "Some of this account's data may still be on \
     this Mac. Open abyss again and sign out to finish deleting it.";
pub(crate) const QUIT: &str = "Quit";

/// A sentence for a core error.
pub(crate) fn user_error(error: UserError) -> SharedString {
    match error {
        UserError::Busy => "Busy right now. Try again in a moment.".into(),
        UserError::Network => "Network problem. Reconnecting…".into(),
        UserError::FloodWait { seconds } => {
            format!("Too many attempts. Try again in {seconds} s.").into()
        }
        UserError::Rejected => "That wasn't accepted. Check it and try again.".into(),
        UserError::NotFound => "Not found.".into(),
        UserError::ShuttingDown => "abyss is closing.".into(),
        UserError::Unsupported => "Not available in this build yet.".into(),
        UserError::Internal => "Something went wrong inside abyss.".into(),
        UserError::AccountMigrated => {
            "Telegram moved this account to another data center. Sign in again.".into()
        }
        UserError::PhoneInvalid => "That phone number isn't valid. Use international format, \
                                    e.g. +44 20 7946 0000."
            .into(),
        UserError::PhoneBanned => "Telegram has banned this phone number.".into(),
        UserError::InvalidCode => {
            "That code is wrong or expired. Check it, or ask for a new one.".into()
        }
        UserError::InvalidPassword => "That password is wrong.".into(),
        UserError::SignUpRequired => "This number has no Telegram account. Create one in an \
                                      official Telegram app first."
            .into(),
        UserError::SessionRevoked => {
            "This session was ended (signed out elsewhere). Sign in again.".into()
        }
        UserError::Credentials(e) => onboarding::credential_error(e, None),
        UserError::PhoneFlood => {
            "Too many sign-in attempts for this number. Try again later.".into()
        }
        UserError::CredentialsNotSaved(e) => format!(
            "Your API credentials couldn't be saved to the Keychain ({}). abyss retries \
             after the next sign-in step.",
            storage_error(e)
        )
        .into(),
        UserError::CredentialsNotForgotten(e) => format!(
            "The saved API credentials couldn't be deleted ({}).",
            storage_error(e)
        )
        .into(),
        UserError::SignOutFirst => "Sign out first to change the API credentials.".into(),
        UserError::Storage(e) => storage_error(e),
    }
}

pub(crate) fn storage_error(error: StorageError) -> SharedString {
    match error {
        StorageError::Keychain => "The Keychain refused the request.".into(),
        StorageError::Corrupt => "Saved data was unreadable and was ignored.".into(),
        StorageError::Busy => "Too many saves at once; one was dropped.".into(),
        StorageError::Stopped => "Local storage stopped.".into(),
        StorageError::Random => "The system random number generator failed.".into(),
        StorageError::Cache => "The message cache couldn't be closed; nothing was deleted.".into(),
        StorageError::CacheFiles => {
            "The message cache is unreadable, but its files couldn't be deleted.".into()
        }
        StorageError::Internal => "Something went wrong with local storage.".into(),
    }
}

/// Title and paragraphs for a launch problem.
pub(crate) fn launch_error(error: &LaunchError) -> (&'static str, Vec<SharedString>) {
    match error {
        LaunchError::FakeNotBuilt => (
            "This build has no demo backend",
            vec!["--fake needs a build with the `fake` feature. Use `cargo dev`.".into()],
        ),
    }
}

/// Title and paragraphs for a core that failed to start.
pub(crate) fn start_error(error: &StartError) -> (&'static str, Vec<SharedString>) {
    ("abyss couldn't start", vec![error.to_string().into()])
}
