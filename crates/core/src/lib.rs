//! abyss-core: Telegram client core. No UI dependencies.

mod actor;
pub mod api;
mod auth_types;
pub mod backend;
pub mod cache;
pub mod credentials;
mod core;
pub mod events;
pub mod model;
pub mod rich_text;
pub mod sanitize;
pub mod secret;
pub mod session;
pub mod store;

pub use crate::actor::{CALL_DEADLINE, MAX_CONCURRENT, READ_FLUSH, SHUTDOWN_DEADLINE};
pub use crate::core::{
    BackendChoice, CacheConfig, CoreConfig, CoreEvents, CoreHandle, CoreSender,
    SendError, ShutdownReport, StartError, start,
};
#[cfg(feature = "fake")]
pub use crate::core::{FAKE_FAULTS_VAR, FakeFaults};
