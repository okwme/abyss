//! Core → UI event pump (plan section 1, "Bridge to GPUI").
//!
//! One foreground task awaits the next core event, drains up to
//! [`DRAIN_MAX`] queued events without waiting, and applies the batch in one
//! entity update, so a burst costs one re-render instead of hundreds.

use std::cell::RefCell;
use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::{self, Poll};

use abyss_core::api::{Command, Event};
use abyss_core::{CoreHandle, SendError, ShutdownReport};
use gpui::{AsyncWindowContext, Context, Task, WeakEntity, Window};

/// Events applied per wake.
pub(crate) const DRAIN_MAX: usize = 256;

/// Resolves with core's shutdown report, or `None` if core ended without one.
pub(crate) type ShutdownAck = Pin<Box<dyn Future<Output = Option<ShutdownReport>>>>;

/// What the UI needs from core. [`CoreHandle`] in the app; a same-thread
/// double in tests, because GPUI's test scheduler rejects wakeups from core's
/// own threads as non-determinism.
pub(crate) trait CoreLink {
    /// Queues a command without waiting.
    fn send(&self, cmd: Command) -> Result<(), SendError>;
    /// The next event; `Ready(None)` once core has stopped.
    fn poll_recv(&mut self, task: &mut task::Context<'_>) -> Poll<Option<Event>>;
    /// An already queued event, without waiting.
    fn try_recv(&mut self) -> Option<Event>;
    /// Asks core to stop; `None` if already asked.
    fn begin_shutdown(&mut self) -> Option<ShutdownAck>;
}

impl CoreLink for CoreHandle {
    fn send(&self, cmd: Command) -> Result<(), SendError> {
        CoreHandle::send(self, cmd)
    }

    /// `CoreHandle` has only `async fn recv`, so each poll builds a fresh
    /// `recv` future. That is sound because tokio's channel receive is cancel
    /// safe and keeps the latest waker registered after the future is dropped.
    fn poll_recv(&mut self, task: &mut task::Context<'_>) -> Poll<Option<Event>> {
        pin!(self.recv()).poll(task)
    }

    fn try_recv(&mut self) -> Option<Event> {
        CoreHandle::try_recv(self)
    }

    fn begin_shutdown(&mut self) -> Option<ShutdownAck> {
        let done = CoreHandle::begin_shutdown(self)?;
        Some(Box::pin(async move { done.await.ok() }))
    }
}

/// The UI's link to core, shared between the pump and the views that send
/// commands. It lives on the GPUI main thread only and is never borrowed
/// across an await.
pub(crate) type SharedCore = Rc<RefCell<Box<dyn CoreLink>>>;

/// Receives batches of events on the main thread.
pub(crate) trait EventSink: 'static + Sized {
    fn apply(&mut self, events: Vec<Event>, window: &mut Window, cx: &mut Context<'_, Self>);
    /// Core's event stream ended.
    fn core_stopped(&mut self, window: &mut Window, cx: &mut Context<'_, Self>);
}

/// Waits for the next event, borrowing the link only inside each poll.
async fn next_event(core: &SharedCore) -> Option<Event> {
    poll_fn(|task| {
        if let Ok(mut link) = core.try_borrow_mut() {
            return link.poll_recv(task);
        }
        // Unreachable on one thread without awaits under the borrow; retry on
        // the next tick rather than panic.
        task.waker().wake_by_ref();
        Poll::Pending
    })
    .await
}

/// The first event plus whatever else is already queued, up to [`DRAIN_MAX`].
pub(crate) fn drain(core: &SharedCore, first: Event) -> Vec<Event> {
    // Most wakes carry one or two events; the batch grows up to DRAIN_MAX.
    let mut batch = Vec::with_capacity(16);
    batch.push(first);
    let Ok(mut link) = core.try_borrow_mut() else {
        return batch;
    };
    while batch.len() < DRAIN_MAX {
        match link.try_recv() {
            Some(event) => batch.push(event),
            None => break,
        }
    }
    batch
}

/// Starts the pump. It ends when core's stream ends or the sink is released.
pub(crate) fn spawn_pump<T: EventSink>(
    core: SharedCore,
    window: &Window,
    cx: &Context<'_, T>,
) -> Task<()> {
    cx.spawn_in(
        window,
        async move |this: WeakEntity<T>, cx: &mut AsyncWindowContext| {
            loop {
                let Some(first) = next_event(&core).await else {
                    this.update_in(cx, T::core_stopped).ok();
                    return;
                };
                let batch = drain(&core, first);
                if this
                    .update_in(cx, |sink, window, cx| sink.apply(batch, window, cx))
                    .is_err()
                {
                    return;
                }
            }
        },
    )
}
