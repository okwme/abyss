//! Rendering for the main window. Placeholder styling: a light palette and
//! system font until the design pass.

use std::time::Instant;

use abyss_core::api::UserError;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, SharedString, Stateful, Window, div,
    prelude::*, px, rgb, rgba, white,
};

use super::auth::{AuthView, Back, back};
use super::chat_view::chat_shell;
use super::session::Session;
use super::shutdown::Closing;
use super::{Failure, Root, State};
use crate::chat::copy as chat_copy;
use crate::ui::copy;

mod onboarding;

pub(super) const FG: u32 = 0x1f_2328;
pub(super) const MUTED: u32 = 0x65_6d76;
pub(super) const BORDER: u32 = 0xd0_d7de;
pub(super) const SURFACE: u32 = 0xf6_f8fa;
pub(super) const ACCENT: u32 = 0x09_69da;
const ERROR: u32 = 0xcf_222e;
pub(super) const SELECTED: u32 = 0xdd_f4ff;

type SubmitFn = fn(&mut Session, &mut Window, &mut Context<'_, Root>);

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let (body, overlay) = match &self.state {
            State::Failed(failure) => (failure_screen(failure).into_any_element(), None),
            State::Running(session) => {
                (session_body(session, cx), closing_overlay(session.closing))
            }
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(white())
            .text_color(rgb(FG))
            .text_size(px(14.))
            .child(body)
            .children(overlay)
    }
}

pub(super) fn centered() -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .justify_center()
        .items_center()
}

fn card() -> Div {
    div().w(px(420.)).flex().flex_col().gap_3().p_6()
}

fn title(text: impl Into<SharedString>) -> Div {
    div()
        .text_xl()
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

pub(super) fn muted(text: impl Into<SharedString>) -> Div {
    div().text_color(rgb(MUTED)).child(text.into())
}

fn failure_screen(failure: &Failure) -> impl IntoElement {
    let (heading, lines) = match failure {
        Failure::Launch(error) => copy::launch_error(error),
        Failure::Start(error) => copy::start_error(error),
    };
    centered().child(
        card()
            .w(px(560.))
            .child(title(heading))
            .children(lines.into_iter().map(muted)),
    )
}

/// What the session shows (task 6m).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Screen {
    /// The auth router (`SignedIn` is the chat shell).
    Auth,
    /// Core stopped with account state kept: the local recovery view (the
    /// chat shell, nothing asked of core) under an honest banner.
    Recovery,
    /// Core stopped with no account state kept: restart.
    Stopped,
}

impl Session {
    pub(super) fn screen(&self) -> Screen {
        if !self.shell.is_stopped() {
            Screen::Auth
        } else if self.auth.retains_account() {
            Screen::Recovery
        } else {
            Screen::Stopped
        }
    }

    /// The recovery view's banner: honest about a logout core stopped in.
    pub(super) fn stopped_banner(&self) -> &'static str {
        match self.auth {
            AuthView::LogoutStarting => chat_copy::STOPPED_SIGNING_OUT,
            AuthView::LogoutNotStarted { .. } => chat_copy::STOPPED_SIGN_OUT_NOT_STARTED,
            _ => chat_copy::CORE_STOPPED_COPY,
        }
    }
}

fn session_body(session: &Session, cx: &mut Context<'_, Root>) -> AnyElement {
    let screen = match session.screen() {
        Screen::Auth => auth_screen(session, cx),
        Screen::Recovery => recovery_view(session, cx),
        Screen::Stopped => centered()
            .child(muted(chat_copy::STOPPED_RESTART))
            .into_any_element(),
    };
    let live = session.screen() == Screen::Auth;
    // The Logout control of the signed-in shell (plan section 17.3); none
    // once core stopped (it needs core).
    let logout = (live && session.auth == AuthView::SignedIn).then(|| {
        div().absolute().top_2().right_4().child(on(
            secondary("logout", copy::onboarding::LOGOUT),
            Session::logout,
            cx,
        ))
    });
    let confirm =
        (live && session.confirming_logout()).then(|| logout_confirm(cx).into_any_element());
    let credential = live.then(|| credential_banner(session, cx)).flatten();
    div()
        .size_full()
        .relative()
        .child(screen)
        .children(credential)
        .children(logout)
        .children(confirm)
        .children(session.notice.clone().map(toast))
        .into_any_element()
}

/// Core stopped with account state kept (task 6m): the chat shell, local
/// only, under a banner. Replaces a logout overlay, whose actions need core.
fn recovery_view(session: &Session, cx: &mut Context<'_, Root>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .w_full()
                .px_4()
                .py_2()
                .border_b_1()
                .child(muted(session.stopped_banner())),
        )
        .child(div().flex_1().min_h_0().child(chat_shell(session, cx)))
        .into_any_element()
}

/// The credential item's failed save or delete, with Retry (finding 6).
fn credential_banner(session: &Session, cx: &mut Context<'_, Root>) -> Option<Div> {
    let text = copy::onboarding::credential_item(session.cred_item)?;
    let retry = super::onboarding::credential_retry(session.cred_item).map(|_| {
        on(
            secondary("credential-retry", copy::onboarding::RETRY),
            Session::retry_credential_item,
            cx,
        )
    });
    Some(
        div()
            .absolute()
            .top_2()
            .left_4()
            .flex()
            .gap_2()
            .items_center()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(BORDER))
            .child(div().text_color(rgb(FG)).child(text))
            .children(retry),
    )
}

/// Logout deletes local data, so it asks first.
fn logout_confirm(cx: &mut Context<'_, Root>) -> Div {
    let cancel = on(
        secondary("logout-cancel", copy::CANCEL),
        Session::cancel_logout,
        cx,
    );
    let confirm = on(
        button("logout-confirm", copy::onboarding::LOGOUT),
        Session::confirm_logout,
        cx,
    );
    modal(
        card()
            .bg(white())
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .child(title(copy::LOGOUT_CONFIRM_TITLE))
            .child(muted(copy::LOGOUT_CONFIRM_DETAIL))
            .child(footer(None, vec![cancel, confirm])),
    )
}

/// "Logout incomplete": the local deletion failed; Retry runs it again.
fn logout_incomplete_screen(
    remote: abyss_core::api::RemoteLogout,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    use abyss_core::api::RemoteLogout;
    let remote = matches!(remote, RemoteLogout::Failed(_) | RemoteLogout::TimedOut)
        .then(|| muted(copy::LOGOUT_REMOTE_UNCONFIRMED));
    let retry = on(
        button("retry-logout", copy::onboarding::RETRY),
        Session::retry_logout,
        cx,
    );
    centered()
        .child(
            card()
                .child(title(copy::LOGOUT_INCOMPLETE_TITLE))
                .child(error_line(copy::LOGOUT_INCOMPLETE_DETAIL))
                .children(remote)
                .child(footer(None, vec![retry])),
        )
        .into_any_element()
}

/// "Couldn't start logout": nothing changed. Retry writes the marker again;
/// the user's own logout may stay signed in instead (core's Back).
fn logout_not_started_screen(
    session: &Session,
    trigger: abyss_core::api::LogoutTrigger,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    use abyss_core::api::LogoutTrigger;
    let detail = match trigger {
        LogoutTrigger::User => copy::LOGOUT_NOT_STARTED_USER,
        LogoutTrigger::Server => copy::LOGOUT_NOT_STARTED_SERVER,
    };
    let stay = (back(&session.auth, session.source) == Back::Core).then(|| {
        on(
            secondary("stay-signed-in", copy::STAY_SIGNED_IN),
            Session::back,
            cx,
        )
    });
    let retry = on(
        button("retry-logout", copy::onboarding::RETRY),
        Session::retry_logout,
        cx,
    );
    centered()
        .child(
            card()
                .child(title(copy::LOGOUT_NOT_STARTED_TITLE))
                .child(error_line(detail))
                .child(footer(stay, vec![retry])),
        )
        .into_any_element()
}

/// "Couldn't read Keychain" at startup: Retry reads the marker again.
fn startup_blocked_screen(cx: &mut Context<'_, Root>) -> AnyElement {
    let retry = on(
        button("retry-startup", copy::onboarding::RETRY),
        Session::retry_startup,
        cx,
    );
    centered()
        .child(
            card()
                .child(title(copy::STARTUP_BLOCKED_TITLE))
                .child(error_line(copy::STARTUP_BLOCKED_DETAIL))
                .child(footer(None, vec![retry])),
        )
        .into_any_element()
}

/// The auth router: one arm per state, each delegating to its own screen
/// function. The onboarding screens live in [`onboarding`].
fn auth_screen(session: &Session, cx: &mut Context<'_, Root>) -> AnyElement {
    use copy::onboarding as text;
    match &session.auth {
        AuthView::Connecting => connecting_screen(),
        AuthView::Onboarding(step) => onboarding::screen(session, step, cx),
        AuthView::Phone { error } => phone_screen(session, *error, cx),
        AuthView::Code {
            phone_hint,
            sent,
            error,
        } => onboarding::code_screen(session, phone_hint, sent.as_ref(), *error, cx),
        AuthView::Password { hint, error } => password_screen(session, hint.as_deref(), *error, cx),
        AuthView::FloodWait { until, .. } => flood_wait_screen(session, *until, cx),
        AuthView::Verifying { error } => onboarding::verifying_screen(*error),
        AuthView::SignUpRequired => onboarding::dead_end_screen(
            session,
            text::SIGN_UP_TITLE,
            text::SIGN_UP_DETAIL,
            None,
            cx,
        ),
        AuthView::OfficialAppRequired { url } => onboarding::dead_end_screen(
            session,
            text::OFFICIAL_APP_TITLE,
            text::OFFICIAL_APP_DETAIL,
            url.as_deref().map(text::official_app_url),
            cx,
        ),

        AuthView::LoggingOut | AuthView::LogoutStarting => onboarding::logging_out_screen(),
        AuthView::LogoutIncomplete { remote } => logout_incomplete_screen(*remote, cx),
        AuthView::LogoutNotStarted { trigger } => logout_not_started_screen(session, *trigger, cx),
        AuthView::StartupBlocked => startup_blocked_screen(cx),
        AuthView::Done {
            has_password,
            saved,
        } => onboarding::done_screen(*has_password, *saved, cx),
        AuthView::SignedIn => chat_shell(session, cx),
    }
}

fn connecting_screen() -> AnyElement {
    centered().child(muted(copy::CONNECTING)).into_any_element()
}

fn phone_screen(
    session: &Session,
    error: Option<UserError>,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let form = LoginForm {
        heading: copy::PHONE_TITLE,
        detail: copy::PHONE_DETAIL.into(),
        input: session.phone.clone().into_any_element(),
        extra: Vec::new(),
        error,
        submit: Session::submit_phone,
        back: back_button(session, cx),
    };
    form.render(cx)
}

fn password_screen(
    session: &Session,
    hint: Option<&str>,
    error: Option<UserError>,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let form = LoginForm {
        heading: copy::PASSWORD_TITLE,
        detail: copy::password_detail(hint),
        input: session.password.clone().into_any_element(),
        extra: Vec::new(),
        error,
        submit: Session::submit_password,
        back: back_button(session, cx),
    };
    form.render(cx)
}

pub(super) fn field(input: AnyElement) -> Div {
    div()
        .w_full()
        .px_3()
        .py_2()
        .border_1()
        .border_color(rgb(BORDER))
        .rounded_md()
        .bg(white())
        .child(input)
}

fn button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(ACCENT))
        .text_color(white())
        .cursor_pointer()
        .child(label)
}

fn secondary(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(SURFACE))
        .cursor_pointer()
        .child(label)
}

/// Wires a button to a session action.
fn on(el: Stateful<Div>, action: SubmitFn, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    el.on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
        root.with_session(|s, cx| action(s, window, cx), cx);
    }))
}

/// Back, where the screen has one (plan section 17.3).
fn back_button(session: &Session, cx: &mut Context<'_, Root>) -> Option<Stateful<Div>> {
    match back(&session.auth, session.source) {
        Back::None => None,
        Back::Local(_) | Back::Core => Some(on(
            secondary("back", copy::onboarding::BACK),
            Session::back,
            cx,
        )),
    }
}

/// A footer row: Back (if any) on the left, the actions on the right.
fn footer(back: Option<Stateful<Div>>, actions: Vec<Stateful<Div>>) -> Div {
    div()
        .flex()
        .justify_between()
        .gap_2()
        .child(div().children(back))
        .child(div().flex().gap_2().children(actions))
}

fn error_line(text: impl Into<SharedString>) -> Div {
    div().text_color(rgb(ERROR)).child(text.into())
}

/// A login step: heading, explanation, one input, extra lines, inline
/// error, Back and Continue.
struct LoginForm {
    heading: &'static str,
    detail: SharedString,
    input: AnyElement,
    extra: Vec<AnyElement>,
    error: Option<UserError>,
    submit: SubmitFn,
    back: Option<Stateful<Div>>,
}

impl LoginForm {
    fn render(self, cx: &mut Context<'_, Root>) -> AnyElement {
        let error = self.error.map(|e| error_line(copy::user_error(e)));
        let submit = on(button("continue", copy::CONTINUE), self.submit, cx);
        centered()
            .child(
                card()
                    .child(title(self.heading))
                    .child(muted(self.detail))
                    .child(field(self.input))
                    .children(self.extra)
                    .children(error)
                    .child(footer(self.back, vec![submit])),
            )
            .into_any_element()
    }
}

fn flood_wait_screen(session: &Session, until: Instant, cx: &mut Context<'_, Root>) -> AnyElement {
    let left = until.saturating_duration_since(Instant::now()).as_secs() + 1;
    centered()
        .child(
            card()
                .child(title(copy::FLOOD_TITLE))
                .child(muted(copy::flood_detail(left)))
                .child(footer(back_button(session, cx), Vec::new())),
        )
        .into_any_element()
}

fn toast(text: SharedString) -> Div {
    div()
        .absolute()
        .bottom_4()
        .left_0()
        .w_full()
        .flex()
        .justify_center()
        .child(
            div()
                .px_4()
                .py_2()
                .rounded_md()
                .bg(rgb(FG))
                .text_color(white())
                .child(text),
        )
}

/// A blocking notice with a Quit button.
fn quit_card(heading: &'static str, detail: &'static str) -> Div {
    card().child(title(heading)).child(muted(detail)).child(
        div()
            .flex()
            .justify_end()
            .child(button("quit", copy::QUIT).on_click(|_: &ClickEvent, _, cx| cx.quit())),
    )
}

fn closing_overlay(closing: Closing) -> Option<AnyElement> {
    let content = match closing {
        Closing::Open => return None,
        Closing::Waiting => card().items_center().child(title(copy::CLOSING)),
        Closing::Unconfirmed => quit_card(copy::UNCONFIRMED_TITLE, copy::UNCONFIRMED_DETAIL),
        Closing::LogoutUnfinished => quit_card(
            copy::LOGOUT_UNFINISHED_TITLE,
            copy::LOGOUT_UNFINISHED_DETAIL,
        ),
    };
    Some(modal(content).into_any_element())
}

/// A layer over the window that blocks the content under it.
fn modal(content: Div) -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .bg(rgba(0xffff_ffe6))
        .flex()
        .justify_center()
        .items_center()
        .child(content)
}
