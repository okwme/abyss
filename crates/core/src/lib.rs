//! abyss-core: Telegram client core. No UI dependencies.

mod actor;
mod auth_types;
pub mod api;
pub mod backend;
pub mod cache;
mod core;
pub mod events;
pub mod model;
pub mod rich_text;
pub mod sanitize;
pub mod secret;
pub mod session;
pub mod store;

pub use crate::actor::{CALL_DEADLINE, MAX_CONCURRENT, READ_FLUSH, SHUTDOWN_DEADLINE};
#[cfg(feature = "fake")]
pub use crate::core::{FAKE_FAULTS_VAR, FakeFaults};
pub use crate::core::{
    BackendChoice, CacheConfig, CoreConfig, CoreEvents, CoreHandle, CoreSender, Credentials,
    SendError, ShutdownReport, StartError, start,
};
