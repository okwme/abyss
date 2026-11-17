//! Chat folders: `messages.getDialogFilters` to [`ChatFolder`]s (folders
//! plan, part B).

use grammers_client::tl;

use super::lists::Oversized;
use super::peers::{channel_chat, group_chat, user_chat};
use crate::model::{ChatFolder, ChatId, FolderRules};
use crate::sanitize::display_name;

/// Folders accepted from one response. Telegram allows 30 (with Premium);
/// the bound only guards against a malformed response.
pub(crate) const MAX_FOLDERS: usize = 64;
/// Peers accepted per folder, across its pinned, included and excluded
/// lists. Telegram allows 200 included chats (with Premium) plus as many
/// excluded ones.
pub(crate) const MAX_FOLDER_PEERS: usize = 512;

/// The chat an `InputPeer` of a folder names, if its id is in range.
/// `inputPeerSelf` is the signed-in user (Saved Messages).
fn folder_peer(peer: &tl::enums::InputPeer, self_id: Option<i64>) -> Option<ChatId> {
    use tl::enums::InputPeer as P;
    match peer {
        P::User(p) => user_chat(p.user_id),
        P::UserFromMessage(p) => user_chat(p.user_id),
        P::Chat(p) => group_chat(p.chat_id),
        P::Channel(p) => channel_chat(p.channel_id),
        P::ChannelFromMessage(p) => channel_chat(p.channel_id),
        P::PeerSelf => user_chat(self_id?),
        P::Empty => None,
    }
}

fn peers(list: &[tl::enums::InputPeer], self_id: Option<i64>) -> Vec<ChatId> {
    list.iter()
        .filter_map(|p| folder_peer(p, self_id))
        .collect()
}

fn title(t: &tl::enums::TextWithEntities) -> String {
    let tl::enums::TextWithEntities::Entities(t) = t;
    display_name(&t.text)
}

fn rules(f: &tl::types::DialogFilter) -> FolderRules {
    [
        (f.contacts, FolderRules::CONTACTS),
        (f.non_contacts, FolderRules::NON_CONTACTS),
        (f.groups, FolderRules::GROUPS),
        (f.broadcasts, FolderRules::BROADCASTS),
        (f.bots, FolderRules::BOTS),
        (f.exclude_muted, FolderRules::EXCLUDE_MUTED),
        (f.exclude_read, FolderRules::EXCLUDE_READ),
        (f.exclude_archived, FolderRules::EXCLUDE_ARCHIVED),
    ]
    .into_iter()
    .filter(|(set, _)| *set)
    .fold(FolderRules::default(), |acc, (_, rule)| acc.with(rule))
}

/// The raw folders of a response, or [`Oversized`] when it carries more
/// folders, or a folder more peers, than abyss accepts. Checked before
/// anything is mapped.
pub(crate) fn raw_folders(
    resp: &tl::enums::messages::DialogFilters,
) -> Result<&[tl::enums::DialogFilter], Oversized> {
    let tl::enums::messages::DialogFilters::Filters(resp) = resp;
    if resp.filters.len() > MAX_FOLDERS {
        return Err(Oversized);
    }
    for f in &resp.filters {
        let n = match f {
            tl::enums::DialogFilter::Filter(f) => {
                f.pinned_peers.len() + f.include_peers.len() + f.exclude_peers.len()
            }
            tl::enums::DialogFilter::Chatlist(c) => c.pinned_peers.len() + c.include_peers.len(),
            tl::enums::DialogFilter::Default => 0,
        };
        if n > MAX_FOLDER_PEERS {
            return Err(Oversized);
        }
    }
    Ok(&resp.filters)
}

/// Maps checked raw folders in server order. The "All chats" position
/// (`dialogFilterDefault`) is not a folder; a folder whose title is not
/// valid after sanitizing is left out.
pub(crate) fn chat_folders(
    raw: &[tl::enums::DialogFilter],
    self_id: Option<i64>,
) -> Vec<ChatFolder> {
    raw.iter()
        .filter_map(|f| match f {
            tl::enums::DialogFilter::Filter(f) => Some(ChatFolder {
                id: f.id,
                title: title(&f.title),
                rules: rules(f),
                pinned: peers(&f.pinned_peers, self_id),
                include: peers(&f.include_peers, self_id),
                exclude: peers(&f.exclude_peers, self_id),
                shared: false,
            }),
            tl::enums::DialogFilter::Chatlist(c) => Some(ChatFolder {
                id: c.id,
                title: title(&c.title),
                rules: FolderRules::default(),
                pinned: peers(&c.pinned_peers, self_id),
                include: peers(&c.include_peers, self_id),
                exclude: Vec::new(),
                shared: true,
            }),
            tl::enums::DialogFilter::Default => None,
        })
        .filter(ChatFolder::is_valid)
        .collect()
}
