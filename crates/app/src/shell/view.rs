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

fn session_body(session: &Session, cx: &mut Context<'_, Root>) -> AnyElement {
    let screen = auth_screen(session, cx);
    // The Logout control of the signed-in shell (plan section 17.3).
    let logout = (session.auth == AuthView::SignedIn).then(|| {
        div().absolute().top_2().right_4().child(on(
            secondary("logout", copy::onboarding::LOGOUT),
            Session::logout,
            cx,
        ))
    });
    div()
        .size_full()
        .relative()
        .child(screen)
        .children(logout)
        .children(session.notice.clone().map(toast))
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
        } => onboarding::code_screen(session, phone_hint, *sent, *error, cx),
        AuthView::Password { hint, error } => password_screen(session, hint.as_deref(), *error, cx),
        AuthView::FloodWait { until, .. } => flood_wait_screen(session, *until, cx),
        AuthView::Verifying { error } => onboarding::verifying_screen(*error),
        AuthView::SignUpRequired => {
            onboarding::dead_end_screen(session, text::SIGN_UP_TITLE, text::SIGN_UP_DETAIL, cx)
        }
        AuthView::OfficialAppRequired => onboarding::dead_end_screen(
            session,
            text::OFFICIAL_APP_TITLE,
            text::OFFICIAL_APP_DETAIL,
            cx,
        ),
        AuthView::LoggingOut => onboarding::logging_out_screen(),
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
    Some(
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
            .into_any_element(),
    )
}
