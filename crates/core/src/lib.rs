//! abyss-core: Telegram client core. No UI dependencies.

mod actor;
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

pub use crate::actor::{CALL_DEADLINE, MAX_CONCURRENT, READ_FLUSH};
pub use crate::core::{
    BackendChoice, CacheConfig, CoreConfig, CoreHandle, Credentials, SendError, ShutdownReport,
    StartError, start,
};
