//! The 2FA password field (plan section 10).
//!
//! A separate element from the vendored editor, which builds plaintext
//! temporaries on every edit. Here the plaintext lives only in a fixed
//! [`SecretBuf`](secret::SecretBuf); everything the platform, IME, text
//! services or the screen can see is bullets. Copy and cut are not handled;
//! paste is, for password managers.

mod secret;
mod secure_input;
#[cfg(test)]
mod tests;

use std::ops::Range;

use gpui::{
    App, Bounds, Context, CursorStyle, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, InspectorElementId,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, Subscription, TextAlign, TextRun, TextStyle, UTF16Selection,
    UnderlineStyle, Window, div, fill, point, prelude::*, px, relative, size,
};
use zeroize::{Zeroize, Zeroizing};

use self::secret::{BULLET, MaskedEdit, Overflow, bullets};
use self::secure_input::SecureInputGuard;
use super::text_input::{
    Backspace, Cancel, Delete, End, Home, KEY_CONTEXT, Left, Paste, Right, SelectAll, SelectLeft,
    SelectRight, Submit,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PasswordEvent {
    /// Enter was pressed. The owner takes the secret with
    /// [`PasswordInput::take_secret`], which wipes the field.
    Submit,
}

pub(crate) struct PasswordInput {
    focus_handle: FocusHandle,
    edit: MaskedEdit,
    placeholder: SharedString,
    /// The shaped bullets from the last paint.
    last_line: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    /// Held while the field is focused in the active window.
    secure: Option<SecureInputGuard>,
    _subscriptions: [Subscription; 3],
}

impl EventEmitter<PasswordEvent> for PasswordInput {}

impl PasswordInput {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let subscriptions = [
            cx.on_focus(&focus_handle, window, |this, window, _| {
                this.sync_secure_input(window);
            }),
            cx.on_blur(&focus_handle, window, |this, window, _| {
                this.sync_secure_input(window);
            }),
            cx.observe_window_activation(window, |this, window, _| {
                this.sync_secure_input(window);
            }),
        ];
        Self {
            focus_handle,
            edit: MaskedEdit::default(),
            placeholder: super::copy::PASSWORD_PLACEHOLDER.into(),
            last_line: None,
            last_bounds: None,
            is_selecting: false,
            secure: None,
            _subscriptions: subscriptions,
        }
    }

    /// Secure Event Input is on exactly while this field is focused in the
    /// active window; holding it longer would block keyboard monitoring
    /// (accessibility tools, other apps' shortcuts) system-wide.
    fn sync_secure_input(&mut self, window: &Window) {
        let wanted = self.focus_handle.is_focused(window) && window.is_window_active();
        if wanted && self.secure.is_none() {
            self.secure = Some(SecureInputGuard::new());
        } else if !wanted {
            self.secure = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.edit.is_empty()
    }

    /// Hands the secret over and wipes the field.
    pub(crate) fn take_secret(&mut self, cx: &mut Context<'_, Self>) -> Zeroizing<Vec<u8>> {
        let secret = self.edit.take();
        cx.notify();
        secret
    }

    /// Wipes the field.
    pub(crate) fn clear(&mut self, cx: &mut Context<'_, Self>) {
        self.edit.clear();
        cx.notify();
    }

    fn edited(
        &mut self,
        result: Result<(), Overflow>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if result.is_err() {
            window.play_system_bell();
        }
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn moved(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<'_, Self>) {
        let sel = self.edit.selected();
        let to = if sel.is_empty() {
            sel.start.saturating_sub(1)
        } else {
            sel.start
        };
        self.edit.move_to(to);
        self.moved(window, cx);
    }

    fn right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<'_, Self>) {
        let sel = self.edit.selected();
        let to = if sel.is_empty() { sel.end + 1 } else { sel.end };
        self.edit.move_to(to);
        self.moved(window, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.edit.select_to(self.edit.cursor().saturating_sub(1));
        self.moved(window, cx);
    }

    fn select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.edit.select_to(self.edit.cursor() + 1);
        self.moved(window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.edit.set_selected(&(0..self.edit.len()));
        self.moved(window, cx);
    }

    fn home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.edit.move_to(0);
        self.moved(window, cx);
    }

    fn end(&mut self, _: &End, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.edit.move_to(self.edit.len());
        self.moved(window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<'_, Self>) {
        let sel = self.edit.selected();
        let range = if sel.is_empty() {
            sel.start.saturating_sub(1)..sel.start
        } else {
            sel
        };
        if range.is_empty() {
            window.play_system_bell();
            return;
        }
        let result = self.edit.replace(Some(&range), "");
        self.edited(result, window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<'_, Self>) {
        let sel = self.edit.selected();
        let range = if sel.is_empty() {
            self.edit.clamp(&(sel.end..sel.end + 1))
        } else {
            sel
        };
        if range.is_empty() {
            window.play_system_bell();
            return;
        }
        let result = self.edit.replace(Some(&range), "");
        self.edited(result, window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<'_, Self>) {
        // The clipboard copy and this temporary are outside our control
        // (plan risk 9); the temporary is at least wiped.
        if let Some(mut text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let result = self.edit.replace(None, &text);
            text.zeroize();
            self.edited(result, window, cx);
        }
    }

    #[expect(clippy::unused_self, reason = "GPUI action listeners receive the view")]
    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<'_, Self>) {
        cx.emit(PasswordEvent::Submit);
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.edit.clear();
        self.edited(Ok(()), window, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.is_selecting = true;
        let at = self.index_for_position(event.position);
        if event.modifiers.shift {
            self.edit.select_to(at);
        } else {
            self.edit.move_to(at);
        }
        self.moved(window, cx);
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<'_, Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.is_selecting {
            let at = self.index_for_position(event.position);
            self.edit.select_to(at);
            self.moved(window, cx);
        }
    }

    /// The current shaped bullets, if they match the secret's length.
    fn line(&self) -> Option<&ShapedLine> {
        self.last_line
            .as_ref()
            .filter(|l| l.len() == masked_offset(self.edit.len()))
    }

    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        match (self.line(), self.last_bounds) {
            (Some(line), Some(bounds)) => {
                char_offset(line.closest_index_for_x(position.x - bounds.left()))
            }
            _ => self.edit.cursor(),
        }
    }
}

/// Byte offset in the bullet string of char `i`.
fn masked_offset(i: usize) -> usize {
    i * BULLET.len_utf8()
}

/// Char index of a byte offset in the bullet string.
fn char_offset(byte: usize) -> usize {
    byte / BULLET.len_utf8()
}

impl EntityInputHandler for PasswordInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<String> {
        let (masked, actual) = self.edit.text_for_range(&range_utf16);
        actual_range.replace(actual);
        Some(masked)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.edit.selected(),
            reversed: self.edit.reversed(),
        })
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.edit.set_selected(&range_utf16);
        self.moved(window, cx);
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<usize> {
        Some(self.edit.len())
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<Range<usize>> {
        self.edit.marked()
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.edit.unmark();
        self.edited(Ok(()), window, cx);
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let result = self.edit.replace(range_utf16.as_ref(), new_text);
        self.edited(result, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let result = self.edit.replace_and_mark(
            range_utf16.as_ref(),
            new_text,
            new_selected_range_utf16.as_ref(),
        );
        self.edited(result, window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.line()?;
        let r = self.edit.clamp(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + line.x_for_index(masked_offset(r.start)),
                bounds.top(),
            ),
            point(
                bounds.left() + line.x_for_index(masked_offset(r.end)),
                bounds.bottom(),
            ),
        ))
    }

    fn selection_bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Bounds<Pixels>> {
        self.bounds_for_range(range_utf16, bounds, window, cx)
            .into_iter()
            .collect()
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<usize> {
        let local = self.last_bounds?.localize(&point)?;
        Some(char_offset(self.line()?.closest_index_for_x(local.x)))
    }
}

/// The bullets (or placeholder) to shape, with the marked range underlined.
fn masked_runs(input: &PasswordInput, style: &TextStyle) -> (SharedString, Vec<TextRun>) {
    let len = input.edit.len();
    let (text, color) = if len == 0 {
        (input.placeholder.clone(), style.color.opacity(0.4))
    } else {
        (SharedString::from(bullets(len)), style.color)
    };
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let Some(m) = input.edit.marked().filter(|_| len > 0) else {
        return (text, vec![run]);
    };
    let runs = [
        TextRun {
            len: masked_offset(m.start),
            ..run.clone()
        },
        TextRun {
            len: masked_offset(m.len()),
            underline: Some(UnderlineStyle {
                color: Some(color),
                thickness: px(1.),
                wavy: false,
            }),
            ..run.clone()
        },
        TextRun {
            len: masked_offset(len.saturating_sub(m.end)),
            ..run
        },
    ]
    .into_iter()
    .filter(|r| r.len > 0)
    .collect();
    (text, runs)
}

struct PasswordElement {
    input: Entity<PasswordInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl Element for PasswordElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let style = window.text_style();
        let (text, runs) = masked_runs(input, &style);
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(text, font_size, &runs, None);
        let sel = input.edit.selected();
        let x = |i: usize| bounds.left() + line.x_for_index(masked_offset(i));
        let (selection, cursor) = if sel.is_empty() {
            let at = point(x(input.edit.cursor()), bounds.top());
            (
                None,
                Some(fill(
                    Bounds::new(at, size(px(1.5), bounds.size.height)),
                    style.color,
                )),
            )
        } else {
            let quad = fill(
                Bounds::from_corners(
                    point(x(sel.start), bounds.top()),
                    point(x(sel.end), bounds.bottom()),
                ),
                style.color.opacity(0.2),
            );
            (Some(quad), None)
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let Some(line) = prepaint.line.take() else {
            return;
        };
        let lh = window.line_height();
        if line
            .paint(bounds.origin, lh, TextAlign::Left, None, window, cx)
            .is_err()
        {
            tracing::warn!("password input: failed to paint");
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        let len = self.input.read(cx).edit.len();
        self.input.update(cx, |input, _| {
            // Placeholder layouts are not bullet layouts.
            input.last_line = (len > 0).then_some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for PasswordInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .flex()
            .w_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(PasswordElement { input: cx.entity() })
    }
}

impl Focusable for PasswordInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
