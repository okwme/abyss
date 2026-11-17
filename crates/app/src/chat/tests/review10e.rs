//! Regression tests for the round 17 re-check of task 10d (plan section 15),
//! the chat shell's half. Each fails on the 10d code.

use std::time::Duration;

use super::*;
use crate::chat::retry::AUTO_RESTART_GAP;

/// Finding 5: a refresh that replaces the window (a new view) keeps late
/// live messages above the request's top. The user, scrolled up, reads one
/// of them (110, 7 px into it); the replacement used to reset the list, so
/// the scroll jumped. Now the reading position survives the new view.
#[gpui::test]
fn a_replaced_view_keeps_the_message_being_read(cx: &mut TestAppContext) {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    for id in 101..=130 {
        shell.apply(live(id));
    }
    let row = |shell: &ChatShell, id| shell.window().unwrap().index_of(MsgId(id)).unwrap();
    shell.rows().scroll_to(ListOffset {
        item_ix: row(&shell, 110),
        offset_in_item: px(7.),
    });
    lay_out(cx, &shell);
    // What core's replacing tail sends: the server's whole history now
    // (1..=30, everything newer deleted) and the late messages on top.
    shell.apply(Event::History {
        chat: CHAT,
        view: ViewGen(2),
        dir: HistoryDir::Tail,
        messages: (1..=30).chain(101..=130).map(msg).collect(),
        removed: Vec::new(),
        has_more: false,
    });
    for _ in 0..2 {
        let top = shell.rows().logical_scroll_top();
        let shown = shell.window().unwrap().get(top.item_ix).map(|m| m.id);
        assert_eq!(shown, Some(MsgId(110)), "still reading 110");
        assert_eq!(top.offset_in_item, px(7.), "at the same offset");
        lay_out(cx, &shell);
    }
    // Following the tail, a new view stays at the tail.
    shell.rows().set_follow_mode(gpui::FollowMode::Tail);
    shell.apply(history(3, HistoryDir::Tail, 101..=130));
    assert!(shell.rows().is_following_tail());
}

/// Finding 6: window activation used to reset every backoff, so toggling
/// focus against a full queue asked again on every activation. Now an
/// activation keeps a waiting retry's deadline, and restarts retries that
/// gave up at most once per `AUTO_RESTART_GAP`.
#[test]
fn rapid_activation_keeps_retries_bounded() {
    let mut core = Core::default();
    let mut shell = open(&mut core);
    let top = Some(Seen {
        first_row: 0,
        last_message: Some(49),
    });
    let loads = std::cell::Cell::new(0usize);
    let mut refuse = |cmd: Command| {
        if matches!(cmd, Command::LoadOlder(_)) {
            loads.set(loads.get() + 1);
        }
        Err(Refused::Busy)
    };
    let step = Duration::from_millis(100);
    let span = Duration::from_secs(100);
    let mut t = now();
    let mut steps = 0u32;
    while t < now() + span {
        let _ = shell.set_focus(WindowState::Inactive, t, &mut refuse);
        let _ = shell.set_focus(WindowState::Active, t, &mut refuse);
        let _ = shell.frame_seen(top, t, &mut refuse);
        steps += 1;
        if steps == 2 {
            // The first refusal's backoff (`FIRST_DELAY`) still runs.
            assert_eq!(loads.get(), 1, "activation kept the deadline");
        }
        t += step;
    }
    let windows = span.as_secs().div_ceil(AUTO_RESTART_GAP.as_secs()) + 1;
    let bound = usize::from(MAX_FAILURES) * usize::try_from(windows).unwrap();
    let asked = loads.get();
    assert!(
        asked <= bound,
        "{asked} attempts in 100 s (at most {bound})"
    );
    // The user's Retry is not limited: it asks at once.
    shell.retry_loads();
    let _ = shell.frame_seen(top, t, &mut refuse);
    assert_eq!(loads.get(), asked + 1);
}
