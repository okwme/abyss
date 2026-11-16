//! The onboarding and login-outcome screens (plan section 17.3). All text
//! comes from `ui::copy::onboarding`.

use std::time::Instant;

use abyss_core::api::{CredentialError, UserError};
use gpui::{AnyElement, Context, Div, Stateful, div, prelude::*, px};

use super::{
    LoginForm, back_button, button, card, centered, error_line, field, footer, muted, on,
    secondary, title,
};
use crate::shell::Root;
use crate::shell::auth::{Onboarding, Saved, SentView, Stored};
use crate::shell::session::Session;
use crate::ui::copy::onboarding as text;
use crate::ui::text_input::Mask;

/// The credential screens.
pub(super) fn screen(
    session: &Session,
    step: &Onboarding,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    match step {
        Onboarding::Welcome => welcome(cx),
        Onboarding::Guide => guide(session, cx),
        Onboarding::Entry { error, stored } => entry(session, *error, *stored, cx),
        Onboarding::Env { error } => env(*error),
        Onboarding::Keychain => keychain(cx),
    }
}

fn page(content: Div) -> AnyElement {
    centered().child(content).into_any_element()
}

fn welcome(cx: &mut Context<'_, Root>) -> AnyElement {
    let connect = on(button("connect", text::CONNECT), Session::connect, cx);
    page(
        card()
            .child(title(text::WELCOME_TITLE))
            .children(text::WELCOME_LINES.into_iter().map(muted))
            .child(footer(None, vec![connect])),
    )
}

fn guide(session: &Session, cx: &mut Context<'_, Root>) -> AnyElement {
    let open = on(
        secondary("open-site", text::OPEN_SITE),
        |_, _, cx| Session::open_site(cx),
        cx,
    );
    let next = on(
        button("have-credentials", text::HAVE_CREDENTIALS),
        Session::have_credentials,
        cx,
    );
    page(
        card()
            .w(px(560.))
            .child(title(text::GUIDE_TITLE))
            .child(muted(text::GUIDE_INTRO))
            .child(div().flex().child(open))
            .children(text::GUIDE_STEPS.into_iter().map(|s| div().child(s)))
            .child(div().pt_2().child(title(text::TROUBLE_TITLE)))
            .children(text::TROUBLE.into_iter().map(muted))
            .child(footer(back_button(session, cx), vec![next])),
    )
}

fn entry(
    session: &Session,
    error: Option<CredentialError>,
    stored: Stored,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let reveal_label = match session.api_hash.read(cx).mask() {
        Mask::Hidden => text::REVEAL,
        Mask::Shown => text::HIDE,
    };
    let reveal = on(
        secondary("reveal-hash", reveal_label),
        |s, _, cx| s.toggle_hash_mask(cx),
        cx,
    );
    let hash_row = div()
        .flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .child(field(session.api_hash.clone().into_any_element())),
        )
        .child(reveal);
    // A local check failure wins over core's last verdict.
    let problem = session
        .entry_error
        .clone()
        .or_else(|| error.map(|e| text::credential_error(e, session.source)));
    let submit = on(
        button("continue", crate::ui::copy::CONTINUE),
        Session::submit_credentials,
        cx,
    );
    page(
        card()
            .child(title(text::ENTRY_TITLE))
            .child(muted(text::ENTRY_DETAIL))
            .child(field(session.api_id.clone().into_any_element()))
            .child(hash_row)
            .children(problem.map(error_line))
            .children(stored_pair(stored, cx))
            .child(footer(back_button(session, cx), vec![submit])),
    )
}

/// A saved pair exists: use it or forget it.
fn stored_pair(stored: Stored, cx: &mut Context<'_, Root>) -> Option<Div> {
    if stored == Stored::No {
        return None;
    }
    let use_saved = on(
        secondary("use-saved", text::USE_SAVED),
        Session::use_saved,
        cx,
    );
    let forget = on(
        secondary("forget-saved", text::FORGET_SAVED),
        Session::forget_saved,
        cx,
    );
    Some(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(muted(text::STORED_NOTE))
            .child(div().flex().gap_2().child(use_saved).child(forget)),
    )
}

/// The env override is unusable: fix it outside abyss. No entry, no Back.
fn env(error: CredentialError) -> AnyElement {
    page(
        card()
            .w(px(560.))
            .child(title(text::ENV_TITLE))
            .children(text::env_detail(error).into_iter().map(muted)),
    )
}

/// The Keychain could not be read: retry, never re-onboard.
fn keychain(cx: &mut Context<'_, Root>) -> AnyElement {
    let retry = on(
        button("retry-keychain", text::RETRY),
        Session::use_saved,
        cx,
    );
    page(
        card()
            .child(title(text::KEYCHAIN_TITLE))
            .child(muted(text::KEYCHAIN_DETAIL))
            .child(footer(None, vec![retry])),
    )
}

/// The code screen: where the code went, the fixed warning, and resend.
pub(super) fn code_screen(
    session: &Session,
    phone_hint: &str,
    sent: Option<SentView>,
    error: Option<UserError>,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let mut extra = vec![
        div().child(text::CODE_WARNING).into_any_element(),
        muted(text::NEW_LOGIN_NOTICE).into_any_element(),
    ];
    if let Some(resend) = resend_row(sent, cx) {
        extra.push(resend.into_any_element());
    }
    let form = LoginForm {
        heading: crate::ui::copy::CODE_TITLE,
        detail: text::code_detail(phone_hint, sent.map(|s| s.delivery)),
        input: session.code.clone().into_any_element(),
        extra,
        error,
        submit: Session::submit_code,
        back: back_button(session, cx),
    };
    form.render(cx)
}

/// When Telegram offers another delivery: a countdown, then a button.
fn resend_row(sent: Option<SentView>, cx: &mut Context<'_, Root>) -> Option<Div> {
    let sent = sent?;
    let next = sent.next?;
    let left = sent
        .resend_at
        .map(|at| at.saturating_duration_since(Instant::now()))
        .filter(|d| !d.is_zero())
        .map(|d| d.as_secs() + 1);
    let mut row = div()
        .flex()
        .gap_2()
        .items_center()
        .child(muted(text::resend_detail(next, left)));
    if left.is_none() {
        let resend: Stateful<Div> = on(secondary("resend", text::RESEND), Session::resend_code, cx);
        row = row.child(resend);
    }
    Some(row)
}

pub(super) fn verifying_screen(error: Option<UserError>) -> AnyElement {
    page(
        card()
            .child(title(text::VERIFYING_TITLE))
            .child(muted(text::VERIFYING_DETAIL))
            .children(error.map(|e| error_line(crate::ui::copy::user_error(e)))),
    )
}

/// Sign-up required, or a step only an official app can do: explain, and
/// offer Back to the phone screen.
pub(super) fn dead_end_screen(
    session: &Session,
    heading: &'static str,
    detail: &'static str,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    page(
        card()
            .child(title(heading))
            .child(muted(detail))
            .child(footer(back_button(session, cx), Vec::new())),
    )
}

pub(super) fn logging_out_screen() -> AnyElement {
    page(
        card()
            .child(title(text::LOGGING_OUT_TITLE))
            .child(muted(text::LOGGING_OUT_DETAIL)),
    )
}

/// After a login in this run: tips, the 2FA recommendation, then the chats.
pub(super) fn done_screen(
    has_password: Option<bool>,
    saved: Saved,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let (heading, unsaved) = match saved {
        Saved::Yes => (text::DONE_TITLE, None),
        Saved::No => (
            text::UNSAVED_TITLE,
            Some(error_line(crate::ui::copy::SIGNED_IN_UNSAVED)),
        ),
    };
    let no_2fa = (has_password == Some(false)).then(|| div().child(text::RECOMMEND_2FA));
    let open = on(
        button("open-chats", text::OPEN_CHATS),
        Session::open_chats,
        cx,
    );
    page(
        card()
            .child(title(heading))
            .children(unsaved)
            .children(text::DONE_TIPS.into_iter().map(muted))
            .children(no_2fa)
            .child(footer(None, vec![open])),
    )
}
