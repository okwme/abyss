//! The mark: the Well, the app icon's shape drawn live in the theme's own
//! hue (`docs/brand/brand.md`, `assets/icon/`).

use gpui::{Div, div, prelude::*, px};

use super::theme::{MARK_STEPS, Theme};

/// Each level as left, top, width and corner radius on a 100-unit tile,
/// outermost first: the tile shape repeated inward, drifting down. These are
/// the same numbers as `assets/icon/well.svg`.
const WELL: [(f32, f32, f32, f32); MARK_STEPS] = [
    (0., 0., 100., 22.5),
    (10., 12., 80., 18.),
    (19., 23., 62., 14.),
    (27., 33., 46., 10.5),
    (34.5, 42., 31., 7.),
    (41., 50., 18., 4.),
];

/// The mark at `size` points. Its centre stays empty and dark: nothing looks
/// back.
pub(crate) fn mark(theme: &Theme, size: f32) -> Div {
    let unit = size / 100.;
    let levels = WELL.iter().zip(theme.mark).map(|(level, tone)| {
        let (left, top, width, radius) = *level;
        div()
            .absolute()
            .left(px(left * unit))
            .top(px(top * unit))
            .size(px(width * unit))
            .rounded(px(radius * unit))
            .bg(tone)
    });
    div().relative().flex_none().size(px(size)).children(levels)
}
