//! Rendering for the main window, in the tonal theme (`docs/brand/brand.md`):
//! one hue, no accent color, emphasis by contrast only.

use std::time::Instant;

use abyss_core::api::UserError;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, SharedString, Stateful, Window, div,
    prelude::*, px,
};

use super::appearance;
use super::auth::AuthView;
use super::chat_view::chat_shell;
use super::session::Session;
use super::shutdown::Closing;
use super::welcome::welcome_screen;
use super::{Failure, Root, State};
use crate::ui::copy;
use crate::ui::mark::mark;
use crate::ui::theme::{self, Depth, Mode, Theme, Themes};

type SubmitFn = fn(&mut Session, &mut Window, &mut Context<'_, Root>);

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = &Themes::of(window, cx);
        let (body, overlay) = match &self.state {
            State::Failed(failure) => (failure_screen(failure).into_any_element(), None),
            State::Running(session) => (
                session_body(session, theme, cx),
                closing_overlay(session.closing, theme),
            ),
        };
        let panel = self
            .appearance_open
            .then(|| appearance::panel(theme, Mode::of(window), Themes::settings(cx), cx));
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.surface(Depth::NOW))
            .text_color(theme.text)
            .font_family(theme::BODY_FONT)
            .text_size(px(14.))
            .child(body)
            .children(panel)
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

pub(super) fn card() -> Div {
    div().w(px(420.)).flex().flex_col().gap_3().p_6()
}

pub(super) fn title(text: impl Into<SharedString>) -> Div {
    div()
        .text_xl()
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// Quiet text: whatever color it inherits, at reduced strength.
pub(super) fn muted(text: impl Into<SharedString>) -> Div {
    div().opacity(theme::QUIET).child(text.into())
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

fn session_body(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let screen = auth_screen(session, theme, cx);
    div()
        .size_full()
        .relative()
        .child(screen)
        .children(session.notice.clone().map(|text| toast(text, theme)))
        .into_any_element()
}

/// The auth router: one arm per state, each delegating to its own screen
/// function. New states (credentials onboarding, welcome) add an arm and a
/// screen function; nothing else changes.
fn auth_screen(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    match &session.auth {
        AuthView::Connecting => connecting_screen(),
        AuthView::Phone { .. } if !session.welcomed => welcome_screen(session, theme, cx),
        AuthView::Phone { error } => phone_screen(session, *error, theme, cx),
        AuthView::Code { phone_hint, error } => code_screen(session, phone_hint, *error, theme, cx),
        AuthView::Password { hint, error } => {
            password_screen(session, hint.as_deref(), *error, theme, cx)
        }
        AuthView::FloodWait { until, .. } => flood_wait_screen(*until),
        AuthView::SignedIn => chat_shell(session, theme, cx),
    }
}

fn connecting_screen() -> AnyElement {
    centered().child(muted(copy::CONNECTING)).into_any_element()
}

fn phone_screen(
    session: &Session,
    error: Option<UserError>,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let form = LoginForm {
        heading: copy::PHONE_TITLE,
        detail: copy::PHONE_DETAIL.into(),
        input: session.phone.clone().into_any_element(),
        error,
        submit: Session::submit_phone,
    };
    form.render(theme, cx)
}

fn code_screen(
    session: &Session,
    phone_hint: &str,
    error: Option<UserError>,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let form = LoginForm {
        heading: copy::CODE_TITLE,
        detail: copy::code_detail(phone_hint),
        input: session.code.clone().into_any_element(),
        error,
        submit: Session::submit_code,
    };
    form.render(theme, cx)
}

fn password_screen(
    session: &Session,
    hint: Option<&str>,
    error: Option<UserError>,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let form = LoginForm {
        heading: copy::PASSWORD_TITLE,
        detail: copy::password_detail(hint),
        input: session.password.clone().into_any_element(),
        error,
        submit: Session::submit_password,
    };
    form.render(theme, cx)
}

/// An input's frame: the same wash as an incoming message.
pub(super) fn field(input: AnyElement, theme: &Theme) -> Div {
    div()
        .w_full()
        .px_3()
        .py_2()
        .border_1()
        .border_color(theme::line(theme.text))
        .rounded(px(theme::RADIUS))
        .bg(theme.incoming)
        .text_color(theme.text)
        .child(input)
}

/// The strongest fill on screen, the same as the user's own messages.
pub(super) fn button(id: &'static str, label: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded(px(theme::RADIUS))
        .bg(theme.text)
        .text_color(theme.on_text)
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label)
}

/// An inline error. No color marks it (the theme has none to spare): a rule
/// and a heavier weight do.
fn error_line(error: UserError, theme: &Theme) -> Div {
    div()
        .pl_2()
        .border_l_2()
        .border_color(theme.text)
        .font_weight(FontWeight::MEDIUM)
        .child(copy::user_error(error))
}

/// The mark, the wordmark and the tagline, above each sign-in step.
pub(super) fn brand(theme: &Theme) -> Div {
    div()
        .pb_4()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(mark(theme, 34.))
                .child(
                    div()
                        .font_family(theme::NAME_FONT)
                        .text_3xl()
                        .child(crate::chat::copy::WORDMARK),
                ),
        )
        .child(muted(copy::TAGLINE))
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
    fn render(self, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
        let submit = self.submit;
        let on_click = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| submit(s, window, cx), cx);
        });
        centered()
            .child(
                card()
                    .child(brand(theme))
                    .child(title(self.heading))
                    .child(muted(self.detail))
                    .child(field(self.input, theme))
                    .children(self.error.map(|e| error_line(e, theme)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .child(button("continue", copy::CONTINUE, theme).on_click(on_click)),
                    )
                    .child(muted(copy::DESCRIPTOR).text_xs().pt_4()),
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

/// A passing notice: outlined, on the base surface, quieter than a message.
fn toast(text: SharedString, theme: &Theme) -> Div {
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
                .rounded_full()
                .border_1()
                .border_color(theme::line(theme.text))
                .bg(theme.surface(Depth::NOW))
                .text_color(theme.text)
                .child(text),
        )
}

fn closing_overlay(closing: Closing, theme: &Theme) -> Option<AnyElement> {
    let content = match closing {
        Closing::Open => return None,
        Closing::Waiting => card().items_center().child(title(copy::CLOSING)),
        Closing::Unconfirmed => card()
            .child(title(copy::UNCONFIRMED_TITLE))
            .child(muted(copy::UNCONFIRMED_DETAIL))
            .child(div().flex().justify_end().child(
                button("quit", copy::QUIT, theme).on_click(|_: &ClickEvent, _, cx| cx.quit()),
            )),
    };
    Some(
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(theme::faded(theme.surface(Depth::NOW), 0.9))
            .flex()
            .justify_center()
            .items_center()
            .child(content)
            .into_any_element(),
    )
}
