//! Raw invocation and the adapter's retry classes (plan section 4, "Retries").
//!
//! The pool runs with `NoRetries`, so every retry is decided here, per call
//! class, and bounded:
//! - `Read` (dialogs, history, read marks): a flood wait of at most 5 s is
//!   slept, at most twice; an I/O error is retried once.
//! - `Never` (sends, and auth in task 6): never retried here. Sends are retried
//!   by core with the same `random_id` (plan section 9); auth must not be.

use std::future::Future;
use std::time::Duration;

use grammers_client::InvocationError;
use grammers_client::tl;

use super::errors::{ErrorKind, kind, loggable_name, rpc_code};

/// Longest flood wait the `Read` class sleeps through.
pub(crate) const MAX_READ_FLOOD: Duration = Duration::from_secs(5);
/// Flood waits slept per `Read` call.
pub(crate) const READ_FLOOD_RETRIES: u8 = 2;
/// I/O errors retried per `Read` call.
pub(crate) const READ_IO_RETRIES: u8 = 1;

/// Sends one TL request. Implemented by grammers' `Client`; tests use a mock.
pub(crate) trait Invoke: Send + Sync + 'static {
    fn invoke<R>(
        &self,
        request: &R,
    ) -> impl Future<Output = Result<R::Return, InvocationError>> + Send
    where
        R: tl::RemoteCall + Send + Sync,
        R::Return: Send;
}

impl Invoke for grammers_client::Client {
    fn invoke<R>(
        &self,
        request: &R,
    ) -> impl Future<Output = Result<R::Return, InvocationError>> + Send
    where
        R: tl::RemoteCall + Send + Sync,
        R::Return: Send,
    {
        grammers_client::Client::invoke(self, request)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryClass {
    Read,
    Never,
}

#[derive(Debug, Default)]
struct Budget {
    floods: u8,
    ios: u8,
}

impl Budget {
    /// How long to wait before retrying, or `None` to give up.
    fn retry_after(&mut self, class: RetryClass, kind: ErrorKind) -> Option<Duration> {
        if class == RetryClass::Never {
            return None;
        }
        match kind {
            ErrorKind::Flood { seconds } if self.floods < READ_FLOOD_RETRIES => {
                let wait = Duration::from_secs(u64::from(seconds));
                (wait <= MAX_READ_FLOOD).then(|| {
                    self.floods += 1;
                    wait
                })
            }
            ErrorKind::Io if self.ios < READ_IO_RETRIES => {
                self.ios += 1;
                Some(Duration::ZERO)
            }
            _ => None,
        }
    }
}

/// Invokes `request` under `class`. The same request value is sent on every
/// attempt, so a retried write is byte-identical (same `random_id`).
pub(crate) async fn call<I, R>(
    invoker: &I,
    class: RetryClass,
    request: &R,
) -> Result<R::Return, InvocationError>
where
    I: Invoke,
    R: tl::RemoteCall + Send + Sync,
    R::Return: Send,
{
    let mut budget = Budget::default();
    // At most 1 + READ_FLOOD_RETRIES + READ_IO_RETRIES attempts.
    loop {
        let err = match invoker.invoke(request).await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let wait = budget.retry_after(class, kind(&err));
        tracing::debug!(
            error = loggable_name(&err),
            code = rpc_code(&err),
            retry = wait.is_some(),
            "telegram call failed"
        );
        match wait {
            Some(d) => tokio::time::sleep(d).await,
            None => return Err(err),
        }
    }
}
