//! Shutdown from the UI side (plan section 5).
//!
//! GPUI gives `on_app_quit` futures only 200 ms, so closing starts earlier:
//! the window's should-close hook and our own `Quit` action both ask core to
//! stop, show "Closing…", and wait for core's ack here, up to a UI timeout.

use std::future::Future;
use std::pin::pin;
use std::time::Duration;

use abyss_core::ShutdownReport;
use futures::future::{Either, select};

/// The UI gives up waiting for core after this long and quits anyway.
pub(crate) const UI_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Closing {
    Open,
    /// Waiting for core's ack; the "Closing…" overlay is up.
    Waiting,
    /// Core could not confirm the session was saved; the user decides when to quit.
    Unconfirmed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ack(ShutdownReport),
    /// Core's thread ended without acking (it panicked or was already gone).
    NoAck,
    TimedOut,
}

/// Waits for core's ack or the timer, whichever comes first.
pub(crate) async fn await_ack<A, T>(ack: Option<A>, timer: T) -> Outcome
where
    A: Future<Output = Option<ShutdownReport>>,
    T: Future<Output = ()>,
{
    let Some(ack) = ack else {
        return Outcome::NoAck;
    };
    match select(pin!(ack), pin!(timer)).await {
        Either::Left((Some(report), _)) => Outcome::Ack(report),
        Either::Left((None, _)) => Outcome::NoAck,
        Either::Right(((), _)) => Outcome::TimedOut,
    }
}

/// What the UI does with an outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    Quit,
    /// Show the unconfirmed-save message and wait for the user.
    ShowUnconfirmed,
}

pub(crate) fn next(outcome: Outcome) -> Next {
    match outcome {
        // A confirmed save quits; so does the UI timeout (plan section 5).
        Outcome::Ack(ShutdownReport::Confirmed) | Outcome::TimedOut => Next::Quit,
        // We never claim a save we didn't observe.
        Outcome::Ack(ShutdownReport::Unconfirmed) | Outcome::NoAck => Next::ShowUnconfirmed,
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use futures::future::{pending, ready};

    use super::*;

    #[test]
    fn ack_wins_over_a_pending_timer() {
        let out = block_on(await_ack(
            Some(ready(Some(ShutdownReport::Confirmed))),
            pending(),
        ));
        assert_eq!(out, Outcome::Ack(ShutdownReport::Confirmed));
        assert_eq!(next(out), Next::Quit);
    }

    #[test]
    fn timer_wins_over_a_stalled_core() {
        let out = block_on(await_ack(
            Some(pending::<Option<ShutdownReport>>()),
            ready(()),
        ));
        assert_eq!(out, Outcome::TimedOut);
        assert_eq!(next(out), Next::Quit);
    }

    #[test]
    fn unconfirmed_or_missing_ack_is_shown() {
        let out = block_on(await_ack(
            Some(ready(Some(ShutdownReport::Unconfirmed))),
            pending(),
        ));
        assert_eq!(next(out), Next::ShowUnconfirmed);
        let out = block_on(await_ack(Some(ready(None::<ShutdownReport>)), pending()));
        assert_eq!(out, Outcome::NoAck);
        assert_eq!(next(out), Next::ShowUnconfirmed);
        let none: Option<futures::future::Ready<Option<ShutdownReport>>> = None;
        assert_eq!(block_on(await_ack(none, pending())), Outcome::NoAck);
    }
}
