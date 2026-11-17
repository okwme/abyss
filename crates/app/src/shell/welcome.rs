//! The Welcome screen (plan section 17.3): what abyss is and an honest threat
//! model for the network being connected, shown before the first sign-in step
//! of a signed-out run. It is also the in-app introduction that Telegram's
//! API terms ask to carry the "uses the Telegram API" statement.

use gpui::{AnyElement, ClickEvent, Context, Div, FontWeight, div, prelude::*, px};

use super::session::Session;
use super::view::{brand, button, card, centered, muted, title};
use super::{Connect, Root};
use crate::onboarding::copy::{self, Network};
use crate::ui::copy as ui_copy;
use crate::ui::theme::Theme;

/// The key context in which Enter means [`Connect`].
pub(super) const KEY_CONTEXT: &str = "Welcome";

const WIDTH: f32 = 520.;

pub(super) fn welcome_screen(
    session: &Session,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let model = copy::threat_model(Network::Telegram);
    let on_click = cx.listener(|root, _: &ClickEvent, window, cx| {
        root.with_session(|s, cx| s.accept_welcome(window, cx), cx);
    });
    let on_enter = cx.listener(|root, _: &Connect, window, cx| {
        root.with_session(|s, cx| s.accept_welcome(window, cx), cx);
    });
    centered()
        .key_context(KEY_CONTEXT)
        .track_focus(&session.welcome_focus)
        .on_action(on_enter)
        .child(
            card()
                .w(px(WIDTH))
                .child(brand(theme))
                .child(title(copy::WELCOME_TITLE))
                .children(model.parts().map(|(label, text)| part(label, text)))
                .child(
                    div()
                        .pt_2()
                        .flex()
                        .justify_end()
                        .child(button("connect", model.connect, theme).on_click(on_click)),
                )
                .child(muted(ui_copy::DESCRIPTOR).text_xs().pt_4()),
        )
        .into_any_element()
}

/// One part of the threat model: its heading, then the statement.
fn part(label: &'static str, text: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .child(div().font_weight(FontWeight::MEDIUM).child(label))
        .child(muted(text))
}
