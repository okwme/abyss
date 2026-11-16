//! Rendering for the main window. Placeholder styling: a light palette and
//! system font until the design pass.

use std::time::Instant;

use abyss_core::api::UserError;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, SharedString, Stateful, Window, div,
    prelude::*, px, rgb, rgba, white,
};

use super::auth::AuthView;
use super::chat_view::chat_shell;
use super::session::Session;
use super::shutdown::Closing;
use super::{Failure, Root, State};
use crate::ui::copy;

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
    div()
        .size_full()
        .relative()
        .child(screen)
        .children(session.notice.clone().map(toast))
        .into_any_element()
}

/// The auth router: one arm per state, each delegating to its own screen
/// function. New states (credentials onboarding, welcome) add an arm and a
/// screen function; nothing else changes.
fn auth_screen(session: &Session, cx: &mut Context<'_, Root>) -> AnyElement {
    match &session.auth {
        AuthView::Connecting => connecting_screen(),
        AuthView::Phone { error } => phone_screen(session, *error, cx),
        AuthView::Code { phone_hint, error } => code_screen(session, phone_hint, *error, cx),
        AuthView::Password { hint, error } => password_screen(session, hint.as_deref(), *error, cx),
        AuthView::FloodWait { until, .. } => flood_wait_screen(*until),
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
        error,
        submit: Session::submit_phone,
    };
    form.render(cx)
}

fn code_screen(
    session: &Session,
    phone_hint: &str,
    error: Option<UserError>,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let form = LoginForm {
        heading: copy::CODE_TITLE,
        detail: copy::code_detail(phone_hint),
        input: session.code.clone().into_any_element(),
        error,
        submit: Session::submit_code,
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
        error,
        submit: Session::submit_password,
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

/// A login step: heading, explanation, one input, inline error, Continue.
struct LoginForm {
    heading: &'static str,
    detail: SharedString,
    input: AnyElement,
    error: Option<UserError>,
    submit: SubmitFn,
}

impl LoginForm {
    fn render(self, cx: &mut Context<'_, Root>) -> AnyElement {
        let submit = self.submit;
        let on_click = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| submit(s, window, cx), cx);
        });
        let error = self
            .error
            .map(|e| div().text_color(rgb(ERROR)).child(copy::user_error(e)));
        centered()
            .child(
                card()
                    .child(title(self.heading))
                    .child(muted(self.detail))
                    .child(field(self.input))
                    .children(error)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .child(button("continue", copy::CONTINUE).on_click(on_click)),
                    ),
            )
            .into_any_element()
    }
}

fn flood_wait_screen(until: Instant) -> AnyElement {
    let left = until.saturating_duration_since(Instant::now()).as_secs() + 1;
    centered()
        .child(
            card()
                .child(title(copy::FLOOD_TITLE))
                .child(muted(copy::flood_detail(left))),
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

fn closing_overlay(closing: Closing) -> Option<AnyElement> {
    let content = match closing {
        Closing::Open => return None,
        Closing::Waiting => card().items_center().child(title(copy::CLOSING)),
        Closing::Unconfirmed => card()
            .child(title(copy::UNCONFIRMED_TITLE))
            .child(muted(copy::UNCONFIRMED_DETAIL))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .child(button("quit", copy::QUIT).on_click(|_: &ClickEvent, _, cx| cx.quit())),
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
