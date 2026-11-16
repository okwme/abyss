//! Logging setup (plan section 11): `tracing` to stderr only, no log files.
//!
//! Every event from a `grammers*` target is dropped by a per-layer filter
//! that `RUST_LOG` cannot override, because grammers logs whole update
//! payloads (message text) at WARN. Debug builds lift it with
//! `ABYSS_DEBUG_PROTOCOL=1`.

use tracing::Metadata;
use tracing_subscriber::filter::{EnvFilter, filter_fn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

pub(crate) const DEFAULT_FILTER: &str = "warn,abyss=info,abyss_core=info";
const PROTOCOL_DEBUG_VAR: &str = "ABYSS_DEBUG_PROTOCOL";

/// Whether grammers' own events may pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Suppressed,
    Visible,
}

fn protocol_policy() -> Protocol {
    let requested = std::env::var_os(PROTOCOL_DEBUG_VAR).is_some_and(|v| v == "1");
    if cfg!(debug_assertions) && requested {
        Protocol::Visible
    } else {
        Protocol::Suppressed
    }
}

fn is_grammers(meta: &Metadata<'_>) -> bool {
    meta.target().starts_with("grammers")
}

fn subscriber<W>(filter: EnvFilter, protocol: Protocol, writer: W) -> impl tracing::Subscriber
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_filter(filter_fn(move |meta| {
            protocol == Protocol::Visible || !is_grammers(meta)
        }));
    tracing_subscriber::registry().with(filter).with(layer)
}

/// Installs the global subscriber. `RUST_LOG` overrides the default filter.
pub(crate) fn init() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    // Fails only if a subscriber is already set, which leaves logging working.
    let _ = tracing::subscriber::set_global_default(subscriber(
        filter,
        protocol_policy(),
        std::io::stderr,
    ));
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'w> MakeWriter<'w> for Buf {
        type Writer = Buf;
        fn make_writer(&'w self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture(protocol: Protocol, filter: &str) -> String {
        let buf = Buf::default();
        let sub = subscriber(EnvFilter::new(filter), protocol, buf.clone());
        tracing::subscriber::with_default(sub, || {
            tracing::warn!(target: "grammers_client::client::messages", "MARKER-PAYLOAD");
            tracing::warn!(target: "grammers_mtsender", "MARKER-SENDER");
            tracing::info!(target: "abyss", "MARKER-OURS");
        });
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn grammers_events_never_reach_the_output() {
        let out = capture(Protocol::Suppressed, DEFAULT_FILTER);
        assert!(!out.contains("MARKER-PAYLOAD"), "{out}");
        assert!(!out.contains("MARKER-SENDER"), "{out}");
        assert!(out.contains("MARKER-OURS"), "{out}");
        // Even an explicit filter asking for grammers at trace level.
        let out = capture(Protocol::Suppressed, "grammers_client=trace,abyss=info");
        assert!(!out.contains("MARKER-PAYLOAD"), "{out}");
    }

    #[test]
    fn protocol_debug_lifts_the_filter() {
        let out = capture(Protocol::Visible, DEFAULT_FILTER);
        assert!(out.contains("MARKER-PAYLOAD"), "{out}");
    }
}
