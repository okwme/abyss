//! Core → UI event channel, bounded both by count and by retained bytes.
//!
//! The sender reserves byte permits before queueing an event and the permit
//! travels with it; the receiver drops the permit as soon as it takes the event.
//! A slow UI therefore makes core wait instead of letting queued payloads grow
//! without limit.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::api::Event;

/// Maximum queued events.
pub const EVENT_CAPACITY: usize = 1024;
/// Maximum retained bytes across queued events.
pub const EVENT_BYTE_BUDGET: usize = 16 * 1024 * 1024;

/// Permits are counted in KiB so a single acquire stays within `u32`.
const PERMIT_UNIT: usize = 1024;
const TOTAL_PERMITS: usize = EVENT_BYTE_BUDGET / PERMIT_UNIT;

fn permits_for(event: &Event) -> u32 {
    let units = event
        .retained_bytes()
        .div_ceil(PERMIT_UNIT)
        .clamp(1, TOTAL_PERMITS);
    u32::try_from(units).unwrap_or(u32::MAX)
}

/// An event and the byte budget it holds until the receiver takes it.
type Queued = (Event, OwnedSemaphorePermit);

#[derive(Clone, Debug)]
pub struct EventSender {
    tx: mpsc::Sender<Queued>,
    budget: Arc<Semaphore>,
}

#[derive(Debug)]
pub struct EventReceiver {
    rx: mpsc::Receiver<Queued>,
}

/// The receiver is gone; core is shutting down or the UI dropped its handle.
#[derive(Debug, PartialEq, Eq)]
pub struct Closed;

pub fn channel() -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::channel(EVENT_CAPACITY);
    let budget = Arc::new(Semaphore::new(TOTAL_PERMITS));
    (EventSender { tx, budget }, EventReceiver { rx })
}

impl EventSender {
    /// Queues `event`, waiting for both a slot and byte budget. If the receiver
    /// is gone, the permit is dropped with the rejected event, returning its budget.
    pub async fn send(&self, event: Event) -> Result<(), Closed> {
        let permit = Arc::clone(&self.budget)
            .acquire_many_owned(permits_for(&event))
            .await
            .map_err(|_| Closed)?;
        self.tx.send((event, permit)).await.map_err(|_| Closed)
    }
}

impl EventReceiver {
    /// Waits for the next event; `None` once core has stopped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await.map(|(event, _permit)| event)
    }

    /// Polls for the next event; `Ready(None)` once core has stopped.
    pub fn poll_recv(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Event>> {
        self.rx
            .poll_recv(cx)
            .map(|item| item.map(|(event, _permit)| event))
    }

    /// Takes an event if one is queued, without waiting.
    pub fn try_recv(&mut self) -> Option<Event> {
        self.rx.try_recv().ok().map(|(event, _permit)| event)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Body, ChatId, Message, MsgId, PeerKind, ThreadKey};
    use crate::rich_text::RichText;

    fn big_message() -> Event {
        let chat = ChatId {
            kind: PeerKind::User,
            id: 1,
        };
        Event::Edited(Message {
            id: MsgId(1),
            thread: ThreadKey { chat, topic: None },
            sender: None,
            date: 0,
            edit_date: None,
            outgoing: false,
            reply_to: None,
            body: Body::Text(RichText::plain(
                &"x".repeat(crate::rich_text::MAX_TEXT_BYTES),
            )),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn byte_budget_blocks_sender_until_consumed() {
        let (tx, mut rx) = channel();
        let per = usize::try_from(permits_for(&big_message())).unwrap();
        let fits = TOTAL_PERMITS / per;
        for _ in 0..fits {
            tx.send(big_message()).await.unwrap();
        }
        let blocked =
            tokio::time::timeout(std::time::Duration::from_secs(1), tx.send(big_message())).await;
        assert!(
            blocked.is_err(),
            "sender must wait when the byte budget is spent"
        );
        assert!(rx.try_recv().is_some());
        tx.send(big_message()).await.unwrap();
    }

    #[tokio::test]
    async fn closed_receiver_returns_budget() {
        let (tx, rx) = channel();
        drop(rx);
        assert_eq!(tx.send(big_message()).await, Err(Closed));
        assert_eq!(tx.budget.available_permits(), TOTAL_PERMITS);
    }
}
