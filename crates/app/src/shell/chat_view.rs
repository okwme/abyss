//! Rendering of the signed-in shell: the chat list (plan section 7) and the
//! open chat (section 8) with its composer and pending sends (section 9).
//! State and decisions live in `crate::chat`; this only draws them.

use std::ops::Range;
use std::time::Instant;

use abyss_core::model::{Body, ChatKind, ChatSummary, Message, MsgId};
use abyss_core::rich_text::RichText;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Div, ElementId, FontWeight, Hsla,
    SharedString, Stateful, StyledText, WeakEntity, Window, canvas, div, font, hsla, list,
    prelude::*, px, rgb, rgba, uniform_list, white,
};
use jiff::tz::TimeZone;

use super::session::Session;
use super::view::{ACCENT, BORDER, FG, MUTED, SELECTED, SURFACE, centered, field, muted};
use super::{Root, State};
use crate::chat::messages::MessageWindow;
use crate::chat::rich::{self, Click, Fonts, Palette, Spoilers};
use crate::chat::sends::Echo;
use crate::chat::{Row, copy, format, list as chat_list};

const CHAT_ROW_HEIGHT: f32 = 56.;
const SIDEBAR_WIDTH: f32 = 300.;
const BUBBLE_MAX_WIDTH: f32 = 560.;
const OUTGOING_BG: u32 = 0xdd_f4ff;
const INCOMING_BG: u32 = 0xf6_f8fa;
const SPOILER: u32 = 0x8c_959f;
const CODE_BG: u32 = 0x1f23_2814;

fn fonts() -> Fonts {
    Fonts {
        body: font(".SystemUIFont"),
        mono: font("Menlo"),
    }
}

fn palette() -> Palette {
    Palette {
        text: rgb(FG).into(),
        muted: rgb(MUTED).into(),
        link: rgb(ACCENT).into(),
        mention: rgb(ACCENT).into(),
        code_bg: rgba(CODE_BG).into(),
        spoiler: rgb(SPOILER).into(),
    }
}

/// A color for an avatar or a sender name, from an id hash.
fn id_color(hue: f32) -> Hsla {
    hsla(hue, 0.55, 0.45, 1.)
}

pub(super) fn chat_shell(session: &Session, cx: &mut Context<'_, Root>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_row()
        .child(sidebar(session, cx))
        .child(chat_pane(session, cx))
        .into_any_element()
}

fn sidebar(session: &Session, cx: &mut Context<'_, Root>) -> Div {
    let count = session.shell.list.len();
    let body = if count == 0 {
        centered().child(muted(copy::NO_CHATS)).into_any_element()
    } else {
        uniform_list("chats", count, cx.processor(chat_rows))
            .flex_1()
            .into_any_element()
    };
    let retry = session
        .shell
        .list
        .gave_up()
        .then(|| retry_button("retry-chats", copy::RETRY_CHATS, cx).bottom_3());
    div()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .relative()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(rgb(BORDER))
        .bg(rgb(SURFACE))
        .child(
            div()
                .p_3()
                .flex()
                .justify_between()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(copy::CHATS))
                .child(muted(copy::conn_label(session.conn))),
        )
        .child(body)
        // Last, so it paints over the rows and takes their clicks.
        .children(retry)
}

fn chat_rows(
    root: &mut Root,
    range: Range<usize>,
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> Vec<Stateful<Div>> {
    let State::Running(session) = &mut root.state else {
        return Vec::new();
    };
    session.dialogs_seen(range.end.saturating_sub(1), window, cx);
    let (tz, now) = (format::local_tz(), format::now());
    let selected = session.shell.selected();
    let chats = session.shell.list.as_slice();
    chats
        .get(range.clone())
        .unwrap_or_default()
        .iter()
        .zip(range)
        .map(|(chat, ix)| {
            let row = ChatRow {
                chat,
                selected: selected == Some(chat.id),
                time: format::list_time(chat.last_date, now, &tz),
            };
            row.render(ix, cx)
        })
        .collect()
}

struct ChatRow<'a> {
    chat: &'a ChatSummary,
    selected: bool,
    time: SharedString,
}

impl ChatRow<'_> {
    fn render(self, ix: usize, cx: &mut Context<'_, Root>) -> Stateful<Div> {
        let chat = self.chat;
        let id = chat.id;
        let bg = if self.selected {
            rgb(SELECTED)
        } else {
            rgba(0)
        };
        let tag = copy::kind_label(chat.kind).map(|t| muted(t).text_xs().flex_none());
        let title = div()
            .flex()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(chat.title.clone()),
            )
            .children(tag)
            .child(muted(self.time).text_xs().flex_none());
        let preview = div()
            .flex()
            .gap_2()
            .child(
                muted(chat_list::preview_text(chat.last.as_ref()))
                    .text_xs()
                    .flex_1()
                    .min_w_0()
                    .truncate(),
            )
            .children(unread_badge(chat));
        div()
            .id(ix)
            .h(px(CHAT_ROW_HEIGHT))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .bg(bg)
            .cursor_pointer()
            .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                root.with_session(|s, cx| s.select_chat(id, window, cx), cx);
            }))
            .child(avatar(chat))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(title)
                    .child(preview),
            )
    }
}

fn unread_badge(chat: &ChatSummary) -> Option<Div> {
    (chat.unread > 0).then(|| {
        let mention = if chat.unread_mentions > 0 { "@ " } else { "" };
        div()
            .px_2()
            .flex_none()
            .rounded_full()
            .bg(rgb(ACCENT))
            .text_color(white())
            .text_xs()
            .child(format!("{mention}{}", copy::unread_badge(chat.unread)))
    })
}

fn avatar(chat: &ChatSummary) -> Div {
    div()
        .size(px(36.))
        .flex_none()
        .rounded_full()
        .bg(id_color(chat_list::hue(chat.id)))
        .text_color(white())
        .text_sm()
        .flex()
        .justify_center()
        .items_center()
        .child(chat_list::initials(&chat.title))
}

fn chat_pane(session: &Session, cx: &mut Context<'_, Root>) -> AnyElement {
    let Some(chat) = session.shell.selected_chat() else {
        return centered()
            .child(muted(copy::SELECT_CHAT))
            .into_any_element();
    };
    let window = session.shell.window();
    let loaded = window.is_some_and(MessageWindow::is_loaded);
    let body = if session.shell.row_count() > 0 {
        list(session.shell.rows().clone(), cx.processor(message_row))
            .size_full()
            .into_any_element()
    } else if loaded {
        centered().child(muted(copy::EMPTY_CHAT)).into_any_element()
    } else {
        centered()
            .child(muted(copy::LOADING_HISTORY))
            .into_any_element()
    };
    let detached = window.is_some_and(MessageWindow::has_newer);
    // Once this pane is painted, check what the list shows (mark read,
    // load more, live tail): the layout is then the one on screen.
    let this = cx.weak_entity();
    let after_paint = canvas(
        |_, _, _| {},
        move |_, (), window, cx| super::chats::after_paint(this, window, cx),
    )
    .absolute()
    .size_0();
    div()
        .flex_1()
        .h_full()
        .min_w_0()
        .flex()
        .flex_col()
        .child(pane_header(chat))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(body)
                .children(detached.then(|| jump_button(cx)))
                .children(
                    session
                        .shell
                        .history_gave_up()
                        .then(|| retry_button("retry-history", copy::RETRY_HISTORY, cx).top_3()),
                )
                .child(after_paint),
        )
        .child(composer_area(session, chat))
        .into_any_element()
}

fn pane_header(chat: &ChatSummary) -> Div {
    let forum = matches!(chat.kind, ChatKind::Supergroup { forum: true, .. });
    div()
        .p_3()
        .border_b_1()
        .border_color(rgb(BORDER))
        .flex()
        .gap_2()
        .items_baseline()
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .truncate()
                .child(chat.title.clone()),
        )
        .children(copy::kind_label(chat.kind).map(|t| muted(t).text_xs()))
        .children(forum.then(|| muted(copy::FORUM_TOPICS).text_xs()))
}

fn jump_button(cx: &mut Context<'_, Root>) -> Stateful<Div> {
    pill("jump-to-latest", copy::JUMP_TO_LATEST)
        .absolute()
        .bottom_3()
        .right_3()
        .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.jump_to_latest(window, cx), cx);
        }))
}

/// Offered once automatic retries gave up: retries everything now. An
/// overlay: rendered after what it covers, it blocks the mouse for the
/// rows below and stops the click there (task 10e).
fn retry_button(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    pill(id, label)
        .debug_selector(|| id.to_owned())
        .absolute()
        .left_3()
        .occlude()
        .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            root.with_session(|s, cx| s.retry_loads(window, cx), cx);
        }))
}

fn pill(id: impl Into<ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_full()
        .bg(rgb(ACCENT))
        .text_color(white())
        .text_sm()
        .cursor_pointer()
        .child(label)
}

fn composer_area(session: &Session, chat: &ChatSummary) -> Div {
    let area = div().p_3().border_t_1().border_color(rgb(BORDER));
    if chat.can_send {
        area.child(field(session.composer.clone().into_any_element()))
    } else {
        area.child(muted(copy::READ_ONLY).text_sm())
    }
}

/// Renders row `ix` of the open chat's message list.
fn message_row(
    root: &mut Root,
    ix: usize,
    _window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let State::Running(session) = &root.state else {
        return div().into_any_element();
    };
    let shell = &session.shell;
    let Some(row) = shell.row(ix) else {
        return div().into_any_element();
    };
    let ctx = RowCtx {
        this: cx.weak_entity(),
        tz: format::local_tz(),
        groupish: shell
            .selected_chat()
            .is_some_and(|c| !matches!(c.kind, ChatKind::Private | ChatKind::Bot)),
    };
    let prev_date = ix
        .checked_sub(1)
        .and_then(|p| shell.row(p))
        .map(|r| r.date());
    let separator = ctx.separator(row.date(), prev_date);
    let start = (ix == 0
        && shell
            .window()
            .is_some_and(|w| w.is_loaded() && !w.has_older()))
    .then(|| day_separator(copy::START_OF_HISTORY.into()));
    let content = match row {
        Row::Message(m) => {
            let reply = m
                .reply_to
                .and_then(|r| shell.window().and_then(|w| w.by_id(r)));
            ctx.message(m, shell.spoilers(m.id), reply)
        }
        Row::Echo(e) => echo_row(e, cx),
    };
    let row = div()
        .w_full()
        .px_4()
        .py_1()
        .flex()
        .flex_col()
        .children(start)
        .children(separator)
        .child(content);
    // Painted bounds feed the after-paint check (mark read, live tail).
    shell.painted().track(ix, row).w_full().into_any_element()
}

fn day_separator(label: SharedString) -> Div {
    div()
        .w_full()
        .py_2()
        .flex()
        .justify_center()
        .child(muted(label).text_xs())
}

/// Which side of the pane a bubble sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Incoming,
    Outgoing,
}

/// A message bubble, right-aligned for outgoing messages.
fn in_bubble(side: Side, content: Vec<AnyElement>) -> AnyElement {
    let bg = if side == Side::Outgoing {
        OUTGOING_BG
    } else {
        INCOMING_BG
    };
    let inner = div()
        .max_w(px(BUBBLE_MAX_WIDTH))
        .px_3()
        .py_2()
        .rounded_lg()
        .bg(rgb(bg))
        .flex()
        .flex_col()
        .gap_1()
        .children(content);
    let row = div().w_full().flex();
    let row = if side == Side::Outgoing {
        row.justify_end()
    } else {
        row.justify_start()
    };
    row.child(inner).into_any_element()
}

fn media_chip(label: &str) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(white())
        .text_sm()
        .child(format!("[{label}]"))
}

fn reply_quote(target: &Message) -> Div {
    let text: SharedString = match &target.body {
        Body::Text(rt) => chat_list::preview_text(Some(&abyss_core::model::Preview::Text(
            abyss_core::sanitize::clean(rt.text(), abyss_core::sanitize::MAX_PREVIEW_BYTES),
        ))),
        Body::Media { kind, .. } => format!("[{}]", copy::media_label(kind)).into(),
        Body::Service(_) | Body::Unsupported => copy::SERVICE_FALLBACK.into(),
    };
    div()
        .pl_2()
        .border_l_2()
        .border_color(rgb(ACCENT))
        .text_xs()
        .text_color(rgb(MUTED))
        .truncate()
        .child(text)
}

/// Per-row rendering context.
struct RowCtx {
    this: WeakEntity<Root>,
    tz: TimeZone,
    /// Groups show sender names.
    groupish: bool,
}

impl RowCtx {
    /// A day separator when this row starts a new local day.
    fn separator(&self, date: i64, prev: Option<i64>) -> Option<Div> {
        let day = format::day(date, &self.tz)?;
        let today = format::day(format::now(), &self.tz)?;
        let prev = prev.and_then(|p| format::day(p, &self.tz));
        (prev != Some(day)).then(|| day_separator(format::day_label(day, today)))
    }

    fn message(&self, m: &Message, spoilers: Spoilers, reply: Option<&Message>) -> AnyElement {
        let body = match &m.body {
            Body::Service(text) => {
                return div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(muted(text.clone()).text_sm().italic())
                    .into_any_element();
            }
            Body::Text(rt) => self.rich(m.id, rt, spoilers),
            Body::Media { kind, caption } => div()
                .flex()
                .flex_col()
                .gap_1()
                .child(media_chip(&copy::media_label(kind)))
                .children((!caption.text().is_empty()).then(|| self.rich(m.id, caption, spoilers)))
                .into_any_element(),
            Body::Unsupported => muted(copy::UNSUPPORTED).italic().into_any_element(),
        };
        let sender = m
            .sender
            .as_ref()
            .filter(|_| self.groupish && !m.outgoing)
            .map(|s| {
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(id_color(chat_list::hue(s.id)))
                    .child(s.name.clone())
                    .into_any_element()
            });
        let time = format::time_of_day(m.date, &self.tz);
        let footer = if m.edited() {
            format!("{} · {time}", copy::EDITED)
        } else {
            time.to_string()
        };
        let mut content: Vec<AnyElement> = Vec::with_capacity(4);
        content.extend(sender);
        content.extend(reply.map(|r| reply_quote(r).into_any_element()));
        content.push(body);
        content.push(muted(footer).text_xs().into_any_element());
        let side = if m.outgoing {
            Side::Outgoing
        } else {
            Side::Incoming
        };
        in_bubble(side, content)
    }

    /// Formatted text: a link click copies its target (links never open), a
    /// spoiler click reveals the message's spoilers.
    fn rich(&self, id: MsgId, rt: &RichText, spoilers: Spoilers) -> AnyElement {
        let this = self.this.clone();
        let element_id =
            ElementId::NamedInteger("msg".into(), u64::try_from(id.0).unwrap_or_default());
        rich::style(rt, &fonts(), &palette(), spoilers)
            .interactive(element_id, move |click, window, cx| {
                on_text_click(&this, id, &click, window, cx);
            })
            .into_any_element()
    }
}

/// An own send without a server copy yet: its state, and Retry and Discard
/// where core accepts them; text core refused offers Put back and Dismiss.
fn echo_row(e: &Echo, cx: &mut Context<'_, Root>) -> AnyElement {
    let local_id = e.local_id;
    let status = div()
        .flex()
        .gap_2()
        .items_center()
        .justify_end()
        .child(muted(copy::send_label(e, Instant::now())).text_xs());
    let retry = e.can_retry().then(|| {
        let retry = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.retry_send(local_id, window, cx), cx);
        });
        pill(("retry", local_id.0), copy::RETRY).on_click(retry)
    });
    let discard = e.can_discard().then(|| {
        let discard = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.discard_send(local_id, window, cx), cx);
        });
        pill(("discard", local_id.0), copy::DISCARD).on_click(discard)
    });
    let returned = e.is_returned().then(|| {
        let put_back = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.put_back(local_id, window, cx), cx);
        });
        let dismiss = cx.listener(move |root, _: &ClickEvent, _, cx| {
            root.with_session(|s, cx| s.dismiss_returned(local_id, cx), cx);
        });
        [
            pill(("put-back", local_id.0), copy::PUT_BACK).on_click(put_back),
            pill(("dismiss", local_id.0), copy::DISMISS).on_click(dismiss),
        ]
    });
    let status = status
        .children(retry)
        .children(discard)
        .children(returned.into_iter().flatten());
    let text = rich::plain(&e.text, &fonts(), &palette());
    let body = StyledText::new(text.text).with_runs(text.runs);
    in_bubble(
        Side::Outgoing,
        vec![body.into_any_element(), status.into_any_element()],
    )
}

fn on_text_click(
    this: &WeakEntity<Root>,
    id: MsgId,
    click: &Click,
    window: &mut Window,
    cx: &mut App,
) {
    if let Click::Copy(target) = click {
        cx.write_to_clipboard(ClipboardItem::new_string(target.to_string()));
    }
    this.update(cx, |root, cx| {
        root.with_session(
            |s, cx| match click {
                Click::Copy(_) => s.show_notice(copy::LINK_COPIED.into(), window, cx),
                Click::Reveal => s.shell.reveal(id),
            },
            cx,
        );
        cx.notify();
    })
    .ok();
}
