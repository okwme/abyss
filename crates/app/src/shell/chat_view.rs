//! Rendering of the signed-in shell: the chat list (plan section 7) and the
//! open chat (section 8) with its composer and pending sends (section 9).
//! State and decisions live in `crate::chat`; this only draws them.

use std::ops::Range;

use abyss_core::model::{Body, ChatKind, ChatSummary, Message, MsgId};
use abyss_core::rich_text::RichText;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Div, ElementId, Hsla, InteractiveText,
    Rgba, SharedString, Stateful, StyledText, WeakEntity, Window, div, font, list, prelude::*, px,
    transparent_black, uniform_list,
};
use jiff::tz::TimeZone;

use super::session::Session;
use super::view::{centered, field, muted};
use super::{Root, State};
use crate::chat::messages::MessageWindow;
use crate::chat::rich::{self, Fonts, Palette, Spoilers};
use crate::chat::sends::Echo;
use crate::chat::{Row, copy, format, list as chat_list};
use crate::ui::mark::mark;
use crate::ui::theme::{self, Depth, Theme, Themes};

const CHAT_ROW_HEIGHT: f32 = 56.;
const SIDEBAR_WIDTH: f32 = 300.;
const BUBBLE_MAX_WIDTH: f32 = 560.;

fn fonts() -> Fonts {
    Fonts {
        body: font(theme::BODY_FONT),
        mono: font(theme::MONO_FONT),
    }
}

/// Rich-text colors for text set in `ink`. All one color: a link is marked
/// by its underline and a mention by its weight. A hidden spoiler is an
/// opaque block, so the text under it cannot show through.
fn palette(ink: Rgba, theme: &Theme) -> Palette {
    Palette {
        text: ink.into(),
        muted: theme::quiet(ink).into(),
        link: ink.into(),
        mention: ink.into(),
        code_bg: theme::faded(ink, 0.1).into(),
        spoiler: theme.hidden.into(),
    }
}

/// Tone tracks age: the depth of something dated `unix`.
fn depth_of(unix: i64, now: i64, tz: &TimeZone) -> Depth {
    Depth::from_days(format::days_ago(unix, now, tz))
}

pub(super) fn chat_shell(
    session: &Session,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_row()
        .child(sidebar(session, theme, cx))
        .child(chat_pane(session, theme, cx))
        .into_any_element()
}

fn sidebar(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let chats = session.shell.list.as_slice();
    let body = if chats.is_empty() {
        centered().child(muted(copy::NO_CHATS)).into_any_element()
    } else {
        uniform_list("chats", chats.len(), cx.processor(chat_rows))
            .flex_1()
            .into_any_element()
    };
    // Below the last chat the column keeps that chat's tone, so the list
    // reads as one descent with no lighter gap under it.
    let floor = chats.last().map_or(Depth::NOW, |c| {
        depth_of(c.last_date, format::now(), &format::local_tz())
    });
    div()
        .w(px(SIDEBAR_WIDTH))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .bg(theme.surface(floor))
        .text_color(theme.text_on(floor))
        .child(
            div()
                .p_3()
                .flex()
                .justify_between()
                .items_baseline()
                .bg(theme.surface(Depth::NOW))
                .text_color(theme.text)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(mark(theme, 18.))
                        .child(
                            div()
                                .font_family(theme::NAME_FONT)
                                .text_lg()
                                .child(copy::WORDMARK),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_3()
                        .child(muted(copy::conn_label(session.conn)).text_xs())
                        .child(appearance_button(cx)),
                ),
        )
        .child(body)
}

/// Opens the appearance panel; also in the app menu and on Cmd+comma.
fn appearance_button(cx: &mut Context<'_, Root>) -> Stateful<Div> {
    div()
        .id("appearance")
        .text_xs()
        .cursor_pointer()
        .child(crate::ui::copy::APPEARANCE)
        .on_click(cx.listener(|root, _: &ClickEvent, _, cx| {
            root.toggle_appearance(cx);
        }))
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
    let theme = &Themes::of(window, cx);
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
                depth: depth_of(chat.last_date, now, &tz),
            };
            row.render(ix, theme, cx)
        })
        .collect()
}

struct ChatRow<'a> {
    chat: &'a ChatSummary,
    selected: bool,
    time: SharedString,
    /// How far the chat has sunk: its surface darkens with its age.
    depth: Depth,
}

impl ChatRow<'_> {
    fn render(self, ix: usize, theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
        let chat = self.chat;
        let id = chat.id;
        let (surface, ink) = (theme.surface(self.depth), theme.text_on(self.depth));
        // The selected chat is marked by a rule in its own text color.
        let mark: Hsla = if self.selected {
            ink.into()
        } else {
            transparent_black()
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
                    .font_family(theme::NAME_FONT)
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
            .children(unread_badge(chat, surface, ink));
        div()
            .id(ix)
            .w_full()
            .h(px(CHAT_ROW_HEIGHT))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .border_l_2()
            .border_color(mark)
            .bg(surface)
            .text_color(ink)
            .cursor_pointer()
            .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                root.with_session(|s, cx| s.select_chat(id, window, cx), cx);
            }))
            .child(avatar(chat, ink))
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

/// An unread count: the row's own two colors, swapped.
fn unread_badge(chat: &ChatSummary, surface: Rgba, ink: Rgba) -> Option<Div> {
    (chat.unread > 0).then(|| {
        let mention = if chat.unread_mentions > 0 { "@ " } else { "" };
        div()
            .px_2()
            .flex_none()
            .rounded_full()
            .bg(ink)
            .text_color(surface)
            .text_xs()
            .child(format!("{mention}{}", copy::unread_badge(chat.unread)))
    })
}

/// Initials on a faint disc of the row's text color (no images in Stage 1).
fn avatar(chat: &ChatSummary, ink: Rgba) -> Div {
    div()
        .size(px(36.))
        .flex_none()
        .rounded_full()
        .bg(theme::faded(ink, 0.14))
        .text_sm()
        .flex()
        .justify_center()
        .items_center()
        .child(chat_list::initials(&chat.title))
}

fn chat_pane(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
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
    // Above the oldest loaded message the pane keeps that message's tone.
    let oldest = session.shell.row(0).map_or(Depth::NOW, |r| {
        depth_of(r.date(), format::now(), &format::local_tz())
    });
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
                .bg(theme.surface(oldest))
                .text_color(theme.text_on(oldest))
                .child(body)
                .children(detached.then(|| jump_button(theme, cx))),
        )
        .child(composer_area(session, chat, theme))
        .into_any_element()
}

fn pane_header(chat: &ChatSummary) -> Div {
    let forum = matches!(chat.kind, ChatKind::Supergroup { forum: true, .. });
    div()
        .p_3()
        .flex()
        .gap_2()
        .items_baseline()
        .child(
            div()
                .font_family(theme::NAME_FONT)
                .text_lg()
                .truncate()
                .child(chat.title.clone()),
        )
        .children(copy::kind_label(chat.kind).map(|t| muted(t).text_xs()))
        .children(forum.then(|| muted(copy::FORUM_TOPICS).text_xs()))
}

fn jump_button(theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    pill(
        "jump-to-latest",
        copy::JUMP_TO_LATEST,
        theme.text,
        theme.on_text,
    )
    .absolute()
    .bottom_3()
    .right_3()
    .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
        root.with_session(|s, cx| s.jump_to_latest(window, cx), cx);
    }))
}

/// A small action in `ink` on `fill`.
fn pill(id: impl Into<ElementId>, label: &'static str, fill: Rgba, ink: Rgba) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_full()
        .bg(fill)
        .text_color(ink)
        .text_sm()
        .cursor_pointer()
        .child(label)
}

fn composer_area(session: &Session, chat: &ChatSummary, theme: &Theme) -> Div {
    let area = div().p_3();
    if chat.can_send {
        area.child(field(session.composer.clone().into_any_element(), theme))
    } else {
        area.child(muted(copy::READ_ONLY).text_sm())
    }
}

/// Renders row `ix` of the open chat's message list.
fn message_row(
    root: &mut Root,
    ix: usize,
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let theme = Themes::of(window, cx);
    let State::Running(session) = &root.state else {
        return div().into_any_element();
    };
    let shell = &session.shell;
    let Some(row) = shell.row(ix) else {
        return div().into_any_element();
    };
    let ctx = RowCtx {
        this: cx.weak_entity(),
        theme,
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
        Row::Echo(e) => echo_row(e, &theme, cx),
    };
    // Each row carries the tone of its own age, so history darkens upward.
    let depth = depth_of(row.date(), format::now(), &ctx.tz);
    div()
        .w_full()
        .px_4()
        .py_1()
        .flex()
        .flex_col()
        .bg(theme.surface(depth))
        .text_color(theme.text_on(depth))
        .children(start)
        .children(separator)
        .child(content)
        .into_any_element()
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

impl Side {
    /// The user's own words carry the most contrast on screen; whatever
    /// arrives sits on a quiet wash.
    fn fill(self, theme: &Theme) -> Rgba {
        match self {
            Self::Incoming => theme.incoming,
            Self::Outgoing => theme.text,
        }
    }

    /// The text color inside a bubble on this side.
    fn ink(self, theme: &Theme) -> Rgba {
        match self {
            Self::Incoming => theme.text,
            Self::Outgoing => theme.on_text,
        }
    }
}

/// A message bubble, right-aligned for outgoing messages.
fn in_bubble(side: Side, theme: &Theme, content: Vec<AnyElement>) -> AnyElement {
    let inner = div()
        .max_w(px(BUBBLE_MAX_WIDTH))
        .px_3()
        .py_2()
        .rounded(px(theme::RADIUS))
        .bg(side.fill(theme))
        .text_color(side.ink(theme))
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

fn media_chip(label: &str, ink: Rgba) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(theme::line(ink))
        .text_sm()
        .child(format!("[{label}]"))
}

fn reply_quote(target: &Message, ink: Rgba) -> Div {
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
        .border_color(ink)
        .text_xs()
        .opacity(theme::QUIET)
        .truncate()
        .child(text)
}

/// Per-row rendering context.
struct RowCtx {
    this: WeakEntity<Root>,
    theme: Theme,
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
        let side = if m.outgoing {
            Side::Outgoing
        } else {
            Side::Incoming
        };
        let ink = side.ink(&self.theme);
        let body = match &m.body {
            Body::Service(text) => {
                return div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(muted(text.clone()).text_sm().italic())
                    .into_any_element();
            }
            Body::Text(rt) => self.rich(m.id, rt, spoilers, ink),
            Body::Media { kind, caption } => div()
                .flex()
                .flex_col()
                .gap_1()
                .child(media_chip(&copy::media_label(kind), ink))
                .children(
                    (!caption.text().is_empty()).then(|| self.rich(m.id, caption, spoilers, ink)),
                )
                .into_any_element(),
            Body::Unsupported => muted(copy::UNSUPPORTED).italic().into_any_element(),
        };
        let sender = m
            .sender
            .as_ref()
            .filter(|_| self.groupish && !m.outgoing)
            .map(|s| {
                div()
                    .text_sm()
                    .font_family(theme::NAME_FONT)
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
        content.extend(reply.map(|r| reply_quote(r, ink).into_any_element()));
        content.push(body);
        content.push(muted(footer).text_xs().into_any_element());
        in_bubble(side, &self.theme, content)
    }

    /// Formatted text: a link click copies its target (links never open), a
    /// spoiler click reveals the message's spoilers.
    fn rich(&self, id: MsgId, rt: &RichText, spoilers: Spoilers, ink: Rgba) -> AnyElement {
        let rich::Body {
            text,
            runs,
            links,
            spoilers: hidden,
        } = rich::style(rt, &fonts(), &palette(ink, &self.theme), spoilers);
        let (mut ranges, targets): (Vec<Range<usize>>, Vec<SharedString>) =
            links.into_iter().unzip();
        ranges.extend(hidden);
        let this = self.this.clone();
        let element_id =
            ElementId::NamedInteger("msg".into(), u64::try_from(id.0).unwrap_or_default());
        InteractiveText::new(element_id, StyledText::new(text).with_runs(runs))
            .on_click(ranges, move |range_ix, window, cx| {
                on_text_click(&this, id, targets.get(range_ix), window, cx);
            })
            .into_any_element()
    }
}

/// An own send without a server copy yet: its state, and Retry and Discard
/// where core accepts them.
fn echo_row(e: &Echo, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let local_id = e.local_id;
    let status = div()
        .flex()
        .gap_2()
        .items_center()
        .justify_end()
        .child(muted(copy::send_label(e.state)).text_xs());
    let status = if e.actionable() {
        let retry = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.retry_send(local_id, window, cx), cx);
        });
        let discard = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.discard_send(local_id, window, cx), cx);
        });
        status
            .child(
                pill(
                    ("retry", local_id.0),
                    copy::RETRY,
                    theme.on_text,
                    theme.text,
                )
                .on_click(retry),
            )
            .child(
                pill(
                    ("discard", local_id.0),
                    copy::DISCARD,
                    theme.on_text,
                    theme.text,
                )
                .on_click(discard),
            )
    } else {
        status
    };
    let text = rich::plain(
        &e.text,
        &fonts(),
        &palette(Side::Outgoing.ink(theme), theme),
    );
    let body = StyledText::new(text.text).with_runs(text.runs);
    in_bubble(
        Side::Outgoing,
        theme,
        vec![body.into_any_element(), status.into_any_element()],
    )
}

fn on_text_click(
    this: &WeakEntity<Root>,
    id: MsgId,
    target: Option<&SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(target) = target {
        cx.write_to_clipboard(ClipboardItem::new_string(target.to_string()));
    }
    this.update(cx, |root, cx| {
        root.with_session(
            |s, cx| match target {
                Some(_) => s.show_notice(copy::LINK_COPIED.into(), window, cx),
                None => s.shell.reveal(id),
            },
            cx,
        );
        cx.notify();
    })
    .ok();
}
