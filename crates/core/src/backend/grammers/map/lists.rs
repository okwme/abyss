//! Responses to pages and updates: dialogs, history, live updates and the
//! `messages.sendMessage` result.

use std::collections::HashMap;

use grammers_client::session::types::PeerInfo;
use grammers_client::tl;

use super::message::{message, raw_date, raw_key};
use super::peers::{Directory, channel_chat, chat_id};
use crate::backend::{DialogsCursor, HistoryPage, HistoryQuery, Update};
use crate::model::{ChatId, ChatSummary, Message, MsgId, Preview};

/// Ids accepted from one deletion update. Telegram batches at most 100 per
/// request; the bound only guards against a malformed update.
pub(crate) const MAX_DELETED_IDS: usize = 10_000;

/// The parts of a `messages.Dialogs` response.
#[derive(Debug)]
pub(crate) struct DialogsParts<'a> {
    pub(crate) dialogs: &'a [tl::enums::Dialog],
    pub(crate) messages: &'a [tl::enums::Message],
    pub(crate) users: &'a [tl::enums::User],
    pub(crate) chats: &'a [tl::enums::Chat],
    /// The server sent the whole list.
    pub(crate) complete: bool,
}

pub(crate) fn dialogs_parts(resp: &tl::enums::messages::Dialogs) -> DialogsParts<'_> {
    use tl::enums::messages::Dialogs as D;
    match resp {
        D::Dialogs(d) => DialogsParts {
            dialogs: &d.dialogs,
            messages: &d.messages,
            users: &d.users,
            chats: &d.chats,
            complete: true,
        },
        D::Slice(d) => DialogsParts {
            dialogs: &d.dialogs,
            messages: &d.messages,
            users: &d.users,
            chats: &d.chats,
            complete: false,
        },
        D::NotModified(_) => DialogsParts {
            dialogs: &[],
            messages: &[],
            users: &[],
            chats: &[],
            complete: true,
        },
    }
}

/// A mapped dialogs page.
#[derive(Debug, Default)]
pub(crate) struct MappedDialogs {
    pub(crate) chats: Vec<ChatSummary>,
    pub(crate) next: Option<DialogsCursor>,
    /// Broadcast channels left out (plan section 7), for a debug count.
    pub(crate) excluded: usize,
    /// Peers of the listed chats, to pin in the session.
    pub(crate) pins: Vec<PeerInfo>,
}

/// Maps a dialogs page requested with `limit`. Folders and communities are
/// skipped, broadcast channels excluded; the cursor continues after the last
/// raw dialog, excluded or not, as `iter_dialogs` does.
pub(crate) fn dialogs(parts: &DialogsParts<'_>, dir: &Directory, limit: usize) -> MappedDialogs {
    let tops: HashMap<(ChatId, i32), &tl::enums::Message> = parts
        .messages
        .iter()
        .filter_map(|m| Some((raw_key(m)?, m)))
        .collect();
    let mut out = MappedDialogs {
        chats: Vec::with_capacity(parts.dialogs.len()),
        ..MappedDialogs::default()
    };
    let mut last = None;
    let mut raw_count = 0usize;
    for d in parts.dialogs {
        let tl::enums::Dialog::Dialog(d) = d else {
            continue;
        };
        raw_count += 1;
        let Some(chat) = chat_id(&d.peer) else {
            continue;
        };
        let top = tops.get(&(chat, d.top_message)).copied();
        last = Some((chat, d.top_message, top.map_or(0, raw_date)));
        let Some(entry) = dir.get(chat) else {
            continue;
        };
        if entry.broadcast {
            out.excluded += 1;
            continue;
        }
        let last_message = top.and_then(|m| message(m, dir));
        let summary = ChatSummary {
            id: chat,
            title: entry.title.clone(),
            kind: entry.kind,
            pinned: d.pinned,
            unread: count(d.unread_count),
            unread_mentions: count(d.unread_mentions_count),
            last: last_message.as_ref().map(Preview::of),
            last_date: last_message.as_ref().map_or(0, |m| m.date),
            can_send: entry.can_send,
        };
        if summary.is_valid() {
            out.pins.push(entry.info.clone());
            out.chats.push(summary);
        }
    }
    let more = !parts.complete && raw_count >= limit;
    out.next = last
        .filter(|_| more)
        .map(|(chat, offset_id, offset_date)| DialogsCursor {
            offset_date,
            offset_id,
            offset_peer: Some(chat),
            index: 0,
        });
    out
}

fn count(n: i32) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

/// The parts of a `messages.Messages` response.
#[derive(Debug)]
pub(crate) struct MessagesParts<'a> {
    pub(crate) messages: &'a [tl::enums::Message],
    pub(crate) users: &'a [tl::enums::User],
    pub(crate) chats: &'a [tl::enums::Chat],
    pub(crate) complete: bool,
}

pub(crate) fn messages_parts(resp: &tl::enums::messages::Messages) -> MessagesParts<'_> {
    use tl::enums::messages::Messages as M;
    match resp {
        M::Messages(m) => MessagesParts {
            messages: &m.messages,
            users: &m.users,
            chats: &m.chats,
            complete: true,
        },
        M::Slice(m) => MessagesParts {
            messages: &m.messages,
            users: &m.users,
            chats: &m.chats,
            complete: false,
        },
        M::ChannelMessages(m) => MessagesParts {
            messages: &m.messages,
            users: &m.users,
            chats: &m.chats,
            complete: false,
        },
        M::NotModified(_) => MessagesParts {
            messages: &[],
            users: &[],
            chats: &[],
            complete: true,
        },
    }
}

/// Maps a history page of `chat` requested with `limit`: only messages of that
/// chat on the requested side of the anchor, ascending, without duplicates.
pub(crate) fn history(
    parts: &MessagesParts<'_>,
    dir: &Directory,
    chat: ChatId,
    query: HistoryQuery,
    limit: usize,
) -> HistoryPage {
    let mut messages: Vec<Message> = parts
        .messages
        .iter()
        .filter_map(|m| message(m, dir))
        .filter(|m| m.thread.chat == chat && m.is_valid())
        .filter(|m| match query {
            HistoryQuery::Tail => true,
            HistoryQuery::Before(id) => m.id < id,
            HistoryQuery::After(id) => m.id > id,
        })
        .collect();
    messages.sort_unstable_by_key(|m| m.id);
    messages.dedup_by_key(|m| m.id);
    let has_more = !parts.complete && parts.messages.len() >= limit;
    HistoryPage { messages, has_more }
}

/// Maps a live update. `None` for everything Stage 1 ignores (counted by the
/// caller at debug level), and for broadcast-channel traffic.
pub(crate) fn update(raw: &tl::enums::Update, dir: &Directory) -> Option<Update> {
    use tl::enums::Update as U;
    let live = |m: &tl::enums::Message| message(m, dir).filter(Message::is_valid);
    match raw {
        U::NewMessage(u) => live(&u.message).map(Update::NewMessage),
        U::NewChannelMessage(u) => live(&u.message).map(Update::NewMessage),
        U::EditMessage(u) => live(&u.message).map(Update::Edited),
        U::EditChannelMessage(u) => live(&u.message).map(Update::Edited),
        U::DeleteMessages(u) => Some(Update::Deleted {
            chat: None,
            ids: deleted_ids(&u.messages),
        }),
        U::DeleteChannelMessages(u) => {
            let chat = channel_chat(u.channel_id)?;
            if dir.get(chat).is_some_and(|e| e.broadcast) {
                return None;
            }
            Some(Update::Deleted {
                chat: Some(chat),
                ids: deleted_ids(&u.messages),
            })
        }
        // A per-topic read (`top_msg_id`) does not change the chat's count.
        U::ReadHistoryInbox(u) if u.top_msg_id.is_none() => Some(Update::ReadInbox {
            chat: chat_id(&u.peer)?,
            still_unread: count(u.still_unread_count),
        }),
        U::ReadChannelInbox(u) => Some(Update::ReadInbox {
            chat: channel_chat(u.channel_id)?,
            still_unread: count(u.still_unread_count),
        }),
        _ => None,
    }
}

fn deleted_ids(ids: &[i32]) -> Vec<MsgId> {
    ids.iter()
        .take(MAX_DELETED_IDS)
        .copied()
        .map(MsgId)
        .collect()
}

/// The server id `messages.sendMessage` assigned to `random_id`, if the
/// response carries the mapping (plan section 9).
pub(crate) fn sent_id(resp: &tl::enums::Updates, random_id: i64) -> Option<MsgId> {
    use tl::enums::Updates as U;
    let from_list = |updates: &[tl::enums::Update]| {
        updates.iter().find_map(|u| match u {
            tl::enums::Update::MessageId(m) if m.random_id == random_id => Some(MsgId(m.id)),
            _ => None,
        })
    };
    match resp {
        U::UpdateShortSentMessage(u) => Some(MsgId(u.id)),
        U::Updates(u) => from_list(&u.updates),
        U::Combined(u) => from_list(&u.updates),
        U::UpdateShort(u) => from_list(std::slice::from_ref(&u.update)),
        U::TooLong | U::UpdateShortMessage(_) | U::UpdateShortChatMessage(_) => None,
    }
}
