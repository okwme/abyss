//! User-facing strings for the startup, auth and shutdown screens, in one
//! place for review and later localization. Nothing here may include user
//! data beyond what the caller passes in for display (a masked phone hint, a
//! server-provided password hint).

use abyss_core::StartError;
use abyss_core::api::UserError;
use gpui::SharedString;

use crate::launch::{API_HASH_VAR, API_ID_VAR, LaunchError};

pub(crate) const CONNECTING: &str = "Connecting…";
pub(crate) const CONTINUE: &str = "Continue";

pub(crate) const PHONE_TITLE: &str = "Sign in to Telegram";
pub(crate) const PHONE_DETAIL: &str = "Your phone number, in international format.";
pub(crate) const PHONE_PLACEHOLDER: &str = "Phone number, e.g. +44 20 7946 0000";

pub(crate) const CODE_TITLE: &str = "Enter the code";
pub(crate) const CODE_PLACEHOLDER: &str = "Code";

pub(crate) fn code_detail(phone_hint: &str) -> SharedString {
    format!("Telegram sent a code to {phone_hint}.").into()
}

pub(crate) const PASSWORD_TITLE: &str = "Two-step verification";
pub(crate) const PASSWORD_PLACEHOLDER: &str = "Password";
/// The `--fake` demo's stand-in for a server password hint.
#[cfg(feature = "fake")]
pub(crate) const DEMO_PASSWORD_HINT: &str = "demo: any non-empty password";

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
    }
}

/// Title and paragraphs for a launch problem.
pub(crate) fn launch_error(error: &LaunchError) -> (&'static str, Vec<SharedString>) {
    match error {
        LaunchError::MissingCredentials(missing) => (
            "Telegram API credentials are missing",
            vec![
                format!("Not set: {}.", missing.join(", ")).into(),
                format!(
                    "abyss connects with your own API credentials. Create them at \
                     my.telegram.org (API development tools), then launch with \
                     {API_ID_VAR} and {API_HASH_VAR} set in the environment."
                )
                .into(),
                "To try abyss without an account, run a development build with --fake \
                 (`cargo dev`)."
                    .into(),
            ],
        ),
        LaunchError::InvalidApiId => (
            "ABYSS_API_ID is not valid",
            vec![
                format!("{API_ID_VAR} must be the positive number shown on my.telegram.org.")
                    .into(),
            ],
        ),
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
