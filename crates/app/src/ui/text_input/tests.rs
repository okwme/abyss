//! Key handling through GPUI's test platform: real key bindings, focus and
//! the input handler path that typed text takes.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{Entity, Focusable, TestAppContext, VisualTestContext};

use super::{Limits, TextInput, TextInputEvent, key_bindings};

fn focused_input(
    limits: Limits,
    cx: &mut TestAppContext,
) -> (Entity<TextInput>, &mut VisualTestContext, Rc<Cell<usize>>) {
    cx.update(|cx| cx.bind_keys(key_bindings()));
    let (input, cx) = cx.add_window_view(|_, cx| TextInput::new(limits, "", cx));
    let submits = Rc::new(Cell::new(0));
    let seen = Rc::clone(&submits);
    cx.update(|window, cx| {
        window.focus(&input.focus_handle(cx), cx);
        cx.subscribe(&input, move |_, event: &TextInputEvent, _| {
            if *event == TextInputEvent::Submit {
                seen.set(seen.get() + 1);
            }
        })
        .detach();
    });
    cx.run_until_parked();
    (input, cx, submits)
}

fn text(input: &Entity<TextInput>, cx: &mut VisualTestContext) -> String {
    input.read_with(cx, |i, _| i.text().to_owned())
}

#[gpui::test]
fn composer_enter_submits_and_shift_enter_breaks_lines(cx: &mut TestAppContext) {
    let (input, cx, submits) = focused_input(Limits::COMPOSER, cx);
    cx.simulate_input("hello");
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("world");
    assert_eq!(text(&input, cx), "hello\nworld");
    assert_eq!(submits.get(), 0);
    cx.simulate_keystrokes("enter");
    assert_eq!(submits.get(), 1);
    // Submitting leaves the text for the owner to take.
    assert_eq!(text(&input, cx), "hello\nworld");
    let taken = input.update_in(cx, TextInput::take_text);
    assert_eq!(taken, "hello\nworld");
    assert_eq!(text(&input, cx), "");
}

#[gpui::test]
fn composer_editing_keys(cx: &mut TestAppContext) {
    let (input, cx, _) = focused_input(Limits::COMPOSER, cx);
    cx.simulate_input("a🙂b");
    cx.simulate_keystrokes("left backspace");
    assert_eq!(text(&input, cx), "ab");
    cx.simulate_keystrokes("cmd-a backspace");
    assert_eq!(text(&input, cx), "");
}

#[gpui::test]
fn single_line_ignores_shift_enter_and_filters_input(cx: &mut TestAppContext) {
    let (input, cx, submits) = focused_input(Limits::PHONE, cx);
    cx.simulate_input("+44 20 7946 0000");
    cx.simulate_keystrokes("shift-enter");
    assert_eq!(text(&input, cx), "+442079460000");
    cx.simulate_keystrokes("enter");
    assert_eq!(submits.get(), 1);
}

#[gpui::test]
fn composer_grows_then_caps_its_height(cx: &mut TestAppContext) {
    let (input, cx, _) = focused_input(Limits::COMPOSER, cx);
    let height = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        input.read_with(cx, |i, _| i.last_bounds.map(|b| b.size.height))
    };
    cx.simulate_input("one");
    let one = height(cx).expect("painted");
    for _ in 0..3 {
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("more");
    }
    let four = height(cx).expect("painted");
    assert!(four > one * 3.5, "{four:?} vs {one:?}");
    for _ in 0..20 {
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("more");
    }
    let capped = height(cx).expect("painted");
    assert!(capped < one * 8.5, "{capped:?} vs {one:?}");
    // The caret (at the end) is scrolled into view.
    assert!(input.read_with(cx, |i, _| i.scroll_top) > gpui::px(0.));
}
