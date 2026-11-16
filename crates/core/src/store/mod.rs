//! In-memory, bounded store of chat list and message windows.

mod dialogs;
mod windows;

pub use dialogs::{Bump, Dialogs, Last, MAX_DIALOGS, Rest, Unread};
pub use windows::{
    Change, MAX_MESSAGES_PER_CHAT, MAX_STORE_BYTES, MAX_WINDOWS, Stale, ViewGen, Window, Windows,
};
