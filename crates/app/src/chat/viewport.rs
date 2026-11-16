//! What the message list actually shows. Each row records the bounds it was
//! painted at; after the frame (`shell::chats`), those are intersected with
//! the list's viewport. `ListState` alone cannot answer this: a list pinned
//! to its bottom has no scroll anchor, so `bounds_for_item` knows no row and
//! `item_is_below_viewport` calls every row, even one above the viewport,
//! "not below".

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{Bounds, Div, IntoElement, ParentElement, Pixels, Styled, canvas, div, px};

/// Rows recorded per frame: far more than a viewport plus overdraw holds.
const MAX_PAINTED: usize = 1024;

/// What the last painted frame showed of the message list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    /// First row that intersects the viewport.
    pub(crate) first_row: usize,
    /// Last message row (not echo) that intersects the viewport.
    pub(crate) last_message: Option<usize>,
}

/// Where rows were painted in the current frame, in window coordinates.
#[derive(Clone, Debug, Default)]
pub(crate) struct Painted(Rc<RefCell<PaintedRows>>);

/// Row index and painted bounds, in paint order.
pub(crate) type PaintedRows = Vec<(usize, Bounds<Pixels>)>;

impl Painted {
    fn record(&self, ix: usize, bounds: Bounds<Pixels>) {
        if let Ok(mut rows) = self.0.try_borrow_mut()
            && rows.len() < MAX_PAINTED
        {
            rows.push((ix, bounds));
        }
    }

    /// The rows painted since the last call.
    pub(crate) fn take(&self) -> PaintedRows {
        self.0
            .try_borrow_mut()
            .map(|mut rows| std::mem::take(&mut *rows))
            .unwrap_or_default()
    }

    /// Wraps row `ix` so its painted bounds are recorded.
    pub(crate) fn track(&self, ix: usize, row: impl IntoElement) -> Div {
        let painted = self.clone();
        let probe = canvas(
            |_, _, _| {},
            move |bounds, (), _, _| painted.record(ix, bounds),
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div().relative().child(row).child(probe)
    }
}

/// The rows of `painted` that intersect `viewport`, of which the first
/// `messages` are message rows. `None` when no row is on screen.
pub(crate) fn seen(
    painted: &[(usize, Bounds<Pixels>)],
    viewport: Bounds<Pixels>,
    messages: usize,
) -> Option<Seen> {
    let visible = painted.iter().filter(|(_, row)| {
        row.size.height > px(0.) && row.bottom() > viewport.top() && row.top() < viewport.bottom()
    });
    let mut first_row: Option<usize> = None;
    let mut last_message: Option<usize> = None;
    for &(ix, _) in visible {
        first_row = Some(first_row.map_or(ix, |f| f.min(ix)));
        if ix < messages {
            last_message = Some(last_message.map_or(ix, |l| l.max(ix)));
        }
    }
    Some(Seen {
        first_row: first_row?,
        last_message,
    })
}
