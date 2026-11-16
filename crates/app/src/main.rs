use gpui::{App, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px, size};
use gpui_platform::application;

struct Root;

impl Render for Root {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child("abyss")
    }
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Root),
        );
        if let Err(err) = opened {
            report_startup_error(&err);
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}

#[expect(
    clippy::print_stderr,
    reason = "no logger exists before the first window; tracing lands with plan section 11"
)]
fn report_startup_error(err: &dyn std::fmt::Display) {
    eprintln!("abyss: failed to open main window: {err:#}");
}
