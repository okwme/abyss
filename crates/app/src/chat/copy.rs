//! User-facing strings for the chat list and message view. Kept apart from
//! `ui::copy` (auth and shutdown) so the two can change independently.

use abyss_core::api::{ConnState, SendState};
use abyss_core::model::{ChatKind, MediaKind};
use gpui::SharedString;

pub(crate) const CHATS: &str = "Chats";
pub(crate) const SELECT_CHAT: &str = "Select a chat";
pub(crate) const NO_CHATS: &str = "No chats yet";
pub(crate) const LOADING_HISTORY: &str = "Loading…";
pub(crate) const START_OF_HISTORY: &str = "Start of the conversation";
pub(crate) const EMPTY_CHAT: &str = "No messages yet";
pub(crate) const EDITED: &str = "edited";
pub(crate) const UNSUPPORTED: &str = "This message type isn't supported yet.";
pub(crate) const SERVICE_FALLBACK: &str = "Service message";
pub(crate) const READ_ONLY: &str = "Only admins can post in this group.";
pub(crate) const JUMP_TO_LATEST: &str = "Jump to latest";
pub(crate) const RETRY: &str = "Retry";
pub(crate) const DISCARD: &str = "Discard";
pub(crate) const LINK_COPIED: &str = "Link copied. abyss never opens links.";
pub(crate) const FORUM_TOPICS: &str = "Topics arrive in a later version.";

pub(crate) fn conn_label(conn: Option<ConnState>) -> SharedString {
    match conn {
        None | Some(ConnState::Connecting) => "Connecting…".into(),
        Some(ConnState::Online) => "Online".into(),
        Some(ConnState::Rebuilding { attempt }) => {
            format!("Reconnecting (attempt {attempt})…").into()
        }
        Some(ConnState::Offline) => "Offline. Retrying…".into(),
    }
}

/// The kind tag next to a chat title; private chats have none.
pub(crate) fn kind_label(kind: ChatKind) -> Option<&'static str> {
    match kind {
        ChatKind::Private => None,
        ChatKind::Bot => Some("bot"),
        ChatKind::Group
        | ChatKind::Supergroup {
            forum: false,
            gigagroup: false,
        } => Some("group"),
        ChatKind::Supergroup {
            gigagroup: true, ..
        } => Some("broadcast group"),
        ChatKind::Supergroup { forum: true, .. } => Some("forum"),
    }
}

/// `[Photo]`, `[Document: report.pdf]`, ... for previews and media chips.
pub(crate) fn media_label(kind: &MediaKind) -> String {
    let name = match kind {
        MediaKind::Photo => "Photo",
        MediaKind::Video => "Video",
        MediaKind::Voice => "Voice message",
        MediaKind::Audio => "Audio",
        MediaKind::Document { name, size } => {
            return format!("Document: {name} ({})", byte_size(*size));
        }
        MediaKind::Sticker => "Sticker",
        MediaKind::Gif => "GIF",
        MediaKind::Poll => "Poll",
        MediaKind::Location => "Location",
        MediaKind::Contact => "Contact",
        MediaKind::Other => "Attachment",
    };
    name.to_owned()
}

/// `512 B`, `1.5 KB`, `12.0 MB`, `2.3 GB`.
pub(crate) fn byte_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut unit = 0;
    let mut tenths = bytes.saturating_mul(10);
    while tenths >= 10_240 && unit + 1 < UNITS.len() {
        tenths /= 1024;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{bytes} {name}")
    } else {
        format!("{}.{} {name}", tenths / 10, tenths % 10)
    }
}

/// The status line under an outgoing message that has no server copy yet.
pub(crate) fn send_label(state: SendState) -> SharedString {
    match state {
        SendState::Sending => "Sending…".into(),
        SendState::Sent { .. } => "Sent".into(),
        SendState::SentUnconfirmed => "Sent, unconfirmed".into(),
        SendState::Unknown { attempts } => {
            format!("Not sure it was sent ({attempts} attempts)").into()
        }
        SendState::Failed => "Not sent".into(),
        SendState::FloodWait { seconds } => {
            format!("Waiting {seconds} s (Telegram rate limit)").into()
        }
    }
}

pub(crate) fn unread_badge(unread: u32) -> SharedString {
    if unread > 999 {
        "999+".into()
    } else {
        unread.to_string().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(byte_size(0), "0 B");
        assert_eq!(byte_size(1023), "1023 B");
        assert_eq!(byte_size(1536), "1.5 KB");
        assert_eq!(byte_size(12 * 1024 * 1024), "12.0 MB");
        assert!(
            byte_size(u64::MAX).ends_with(" GB"),
            "saturates, no overflow"
        );
    }
}
