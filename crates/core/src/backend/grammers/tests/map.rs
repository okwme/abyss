//! The pure TL → domain mappers.

use grammers_client::session::types::PeerInfo;
use grammers_client::tl;

use super::{
    basic_group, bold, channel, dialog, message, peer_channel, peer_chat, peer_user, reply_header,
    service, text_url, user, users,
};
use crate::backend::grammers::map::lists::{self, DialogsParts, MAX_DELETED_IDS, MessagesParts};
use crate::backend::grammers::map::message::{self as msg, media_kind, reply_and_topic, rich};
use crate::backend::grammers::map::peers::{Directory, chat_id, input_peer, user_chat};
use crate::backend::{HistoryQuery, Update};
use crate::model::{Body, ChatId, ChatKind, MediaKind, MsgId, PeerKind, Preview};
use crate::rich_text::{LinkTarget, MAX_ENTITIES, MAX_TEXT_BYTES, MAX_URL_BYTES, Style};
use crate::sanitize::MAX_SERVICE_BYTES;

fn uid(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::User,
        id,
    }
}

fn cid(id: i64) -> ChatId {
    ChatId {
        kind: PeerKind::Channel,
        id,
    }
}

fn map_one(m: tl::types::Message, dir: &Directory) -> crate::model::Message {
    msg::message(&tl::enums::Message::Message(m), dir).expect("mapped")
}

fn text_of(m: &crate::model::Message) -> &crate::rich_text::RichText {
    match &m.body {
        Body::Text(rt) | Body::Media { caption: rt, .. } => rt,
        other => panic!("not text: {other:?}"),
    }
}

// --- entities and the formatting budget ---

#[test]
fn entities_become_styled_runs_and_links() {
    let rt = rich(
        "hello world",
        Some(&[bold(0, 5), text_url(6, 5, "https://example.org")]),
    );
    assert_eq!(rt.text(), "hello world");
    let runs = rt.runs();
    assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), 11);
    assert!(runs[0].style.contains(Style::BOLD));
    assert_eq!(runs[0].len, 5);
    let link = runs.last().unwrap().link.expect("link run");
    assert_eq!(
        rt.links()[usize::from(link)],
        LinkTarget::Hidden("https://example.org".into())
    );
}

#[test]
fn entity_inside_a_surrogate_pair_is_dropped() {
    // "a😀b": 😀 is two UTF-16 units at offsets 1..3.
    let inside = rich("a😀b", Some(&[bold(2, 2)]));
    assert!(inside.runs().iter().all(|r| r.style == Style::default()));
    let whole = rich("a😀b", Some(&[bold(1, 2)]));
    let styled: Vec<_> = whole
        .runs()
        .iter()
        .filter(|r| r.style.contains(Style::BOLD))
        .collect();
    assert_eq!(styled.len(), 1);
    assert_eq!(styled[0].len, "😀".len());
}

#[test]
fn out_of_range_and_negative_entities_are_dropped() {
    let rt = rich("abc", Some(&[bold(-1, 2), bold(2, 50), bold(1, -1)]));
    assert_eq!(rt.runs().len(), 1);
    assert_eq!(rt.runs()[0].style, Style::default());
}

#[test]
fn oversized_text_is_plain_and_clamped() {
    let text = "é".repeat(MAX_TEXT_BYTES);
    let rt = rich(&text, Some(&[bold(0, 4)]));
    assert!(rt.text().len() <= MAX_TEXT_BYTES);
    assert!(text.starts_with(rt.text()));
    assert_eq!(rt.runs().len(), 1);
    assert!(rt.links().is_empty());
}

#[test]
fn too_many_entities_degrade_to_plain() {
    let entities: Vec<_> = (0..=MAX_ENTITIES)
        .map(|i| bold(i32::try_from(i).unwrap(), 1))
        .collect();
    let rt = rich(&"x".repeat(MAX_ENTITIES + 1), Some(&entities));
    assert_eq!(rt.runs().len(), 1);
    assert_eq!(rt.runs()[0].style, Style::default());
}

#[test]
fn an_overlong_hidden_url_is_never_shortened_into_the_budget() {
    // Multi-byte chars around the cut: a truncation to a char boundary below
    // the budget would make the link pass with a different target.
    let url = format!("https://e.org/{}", "é".repeat(MAX_URL_BYTES / 2));
    assert!(url.len() > MAX_URL_BYTES);
    let rt = rich("click", Some(&[text_url(0, 5, &url)]));
    assert!(rt.links().is_empty(), "over budget: plain text");
    let ok = rich("click", Some(&[text_url(0, 5, "https://e.org/x")]));
    assert_eq!(ok.links().len(), 1);
}

// --- messages ---

#[test]
fn text_message_maps_ids_dates_edits_and_sender() {
    let mut m = message(peer_channel(10), 5, "hi");
    m.from_id = Some(peer_user(7));
    m.edit_date = Some(1_700_000_100);
    m.reply_to = Some(tl::enums::MessageReplyHeader::Header(reply_header(
        Some(4),
        None,
    )));
    let dir = Directory::from_lists(&users(vec![user(7, "Ann", "Lee")]), &[]);
    let out = map_one(m, &dir);
    assert_eq!(out.id, MsgId(5));
    assert_eq!(out.thread.chat, cid(10));
    assert_eq!(out.thread.topic, None);
    assert_eq!(out.edit_date, Some(1_700_000_100));
    assert!(out.edited());
    assert_eq!(out.reply_to, Some(MsgId(4)));
    let sender = out.sender.as_ref().unwrap();
    assert_eq!((sender.id, sender.name.as_str()), (uid(7), "Ann Lee"));
    assert_eq!(text_of(&out).text(), "hi");
    assert!(out.is_valid());
}

#[test]
fn private_chat_sender_falls_back_to_the_chat_unless_outgoing() {
    let dir = Directory::from_lists(&users(vec![user(7, "Ann", "")]), &[]);
    let incoming = map_one(message(peer_user(7), 1, "x"), &dir);
    assert_eq!(incoming.sender.unwrap().id, uid(7));
    let mut out = message(peer_user(7), 2, "y");
    out.out = true;
    let outgoing = map_one(out, &dir);
    assert!(outgoing.outgoing);
    assert_eq!(outgoing.sender, None);
}

#[test]
fn bidi_controls_are_stripped_from_names_and_titles() {
    let dir = Directory::from_lists(
        &users(vec![user(7, "\u{202E}gnp.exe", "\u{2066}x\u{2069}")]),
        &[tl::enums::Chat::Channel(channel(10, "pay\u{202E}lapyap"))],
    );
    assert_eq!(dir.get(uid(7)).unwrap().name, "gnp.exe x");
    assert_eq!(dir.get(cid(10)).unwrap().title, "paylapyap");
    let mut m = message(peer_channel(10), 1, "hi");
    m.from_id = Some(peer_user(7));
    assert_eq!(map_one(m, &dir).sender.unwrap().name, "gnp.exe x");
}

#[test]
fn deleted_and_nameless_users_get_placeholder_names() {
    let mut deleted = user(1, "", "");
    deleted.deleted = true;
    let mut me = user(2, "Me", "");
    me.is_self = true;
    let dir = Directory::from_lists(&users(vec![deleted, me, user(3, "\u{202E}", "")]), &[]);
    assert_eq!(dir.get(uid(1)).unwrap().name, "Deleted Account");
    assert!(!dir.get(uid(1)).unwrap().can_send);
    assert_eq!(dir.get(uid(2)).unwrap().title, "Saved Messages");
    assert_eq!(dir.get(uid(2)).unwrap().name, "Me");
    assert_eq!(dir.get(uid(3)).unwrap().name, "Unknown");
}

fn document(attributes: Vec<tl::enums::DocumentAttribute>) -> tl::types::MessageMediaDocument {
    tl::types::MessageMediaDocument {
        nopremium: false,
        spoiler: false,
        video: false,
        round: false,
        voice: false,
        document: Some(tl::enums::Document::Document(tl::types::Document {
            id: 1,
            access_hash: 2,
            file_reference: Vec::new(),
            date: 0,
            mime_type: "application/octet-stream".into(),
            size: 1234,
            thumbs: None,
            video_thumbs: None,
            dc_id: 2,
            attributes,
        })),
        alt_documents: None,
        video_cover: None,
        video_timestamp: None,
        ttl_seconds: None,
    }
}

fn filename(name: &str) -> tl::enums::DocumentAttribute {
    tl::enums::DocumentAttribute::Filename(tl::types::DocumentAttributeFilename {
        file_name: name.into(),
    })
}

fn video_attr() -> tl::enums::DocumentAttribute {
    tl::enums::DocumentAttribute::Video(tl::types::DocumentAttributeVideo {
        round_message: false,
        supports_streaming: false,
        nosound: false,
        duration: 1.0,
        w: 1,
        h: 1,
        preload_prefix_size: None,
        video_start_ts: None,
        video_codec: None,
    })
}

fn audio_attr() -> tl::types::DocumentAttributeAudio {
    tl::types::DocumentAttributeAudio {
        voice: false,
        duration: 1,
        title: Some("t".into()),
        performer: None,
        waveform: None,
    }
}

#[test]
fn documents_map_to_inert_placeholders() {
    use tl::enums::DocumentAttribute as A;
    let doc = |attrs| media_kind(&tl::enums::MessageMedia::Document(document(attrs)));
    assert_eq!(
        doc(vec![filename("in\u{202E}fdp.exe")]),
        Some(MediaKind::Document {
            name: "infdp.exe".into(),
            size: 1234
        })
    );
    assert_eq!(
        doc(vec![]),
        Some(MediaKind::Document {
            name: "file".into(),
            size: 1234
        })
    );
    assert_eq!(
        doc(vec![video_attr(), filename("a.mp4")]),
        Some(MediaKind::Video)
    );
    assert_eq!(doc(vec![A::Animated, video_attr()]), Some(MediaKind::Gif));
    assert_eq!(
        doc(vec![A::Audio(tl::types::DocumentAttributeAudio {
            voice: true,
            ..audio_attr()
        })]),
        Some(MediaKind::Voice)
    );
    assert_eq!(doc(vec![A::Audio(audio_attr())]), Some(MediaKind::Audio));
    let sticker = A::Sticker(tl::types::DocumentAttributeSticker {
        mask: false,
        alt: "x".into(),
        stickerset: tl::enums::InputStickerSet::Empty,
        mask_coords: None,
    });
    assert_eq!(doc(vec![sticker, video_attr()]), Some(MediaKind::Sticker));
    let mut voice = document(vec![]);
    voice.voice = true;
    assert_eq!(
        media_kind(&tl::enums::MessageMedia::Document(voice)),
        Some(MediaKind::Voice)
    );
    let mut round = document(vec![]);
    round.round = true;
    assert_eq!(
        media_kind(&tl::enums::MessageMedia::Document(round)),
        Some(MediaKind::Video)
    );
    let mut expired = document(vec![]);
    expired.document = None;
    assert_eq!(
        media_kind(&tl::enums::MessageMedia::Document(expired)),
        Some(MediaKind::Other)
    );
}

#[test]
fn other_media_kinds_and_link_previews() {
    use tl::enums::MessageMedia as M;
    let photo = M::Photo(tl::types::MessageMediaPhoto {
        spoiler: false,
        live_photo: false,
        photo: None,
        ttl_seconds: None,
        video: None,
    });
    assert_eq!(media_kind(&photo), Some(MediaKind::Photo));
    let geo = M::Geo(tl::types::MessageMediaGeo {
        geo: tl::enums::GeoPoint::Empty,
    });
    assert_eq!(media_kind(&geo), Some(MediaKind::Location));
    let contact = M::Contact(tl::types::MessageMediaContact {
        phone_number: "+1".into(),
        first_name: "a".into(),
        last_name: String::new(),
        vcard: String::new(),
        user_id: 0,
    });
    assert_eq!(media_kind(&contact), Some(MediaKind::Contact));
    assert_eq!(media_kind(&M::Unsupported), Some(MediaKind::Other));
    assert_eq!(media_kind(&M::Empty), None);
}

#[test]
fn media_message_keeps_its_caption() {
    let mut m = message(peer_user(7), 3, "look");
    m.entities = Some(vec![bold(0, 4)]);
    m.media = Some(tl::enums::MessageMedia::Document(document(vec![filename(
        "a.pdf",
    )])));
    let out = map_one(m, &Directory::default());
    let Body::Media { kind, caption } = &out.body else {
        panic!("media body");
    };
    assert!(matches!(kind, MediaKind::Document { name, .. } if name == "a.pdf"));
    assert_eq!(caption.text(), "look");
    assert!(caption.runs()[0].style.contains(Style::BOLD));
}

#[test]
fn service_messages_are_described_and_bounded() {
    use tl::enums::MessageAction as A;
    let dir = Directory::from_lists(&users(vec![user(7, "Ann", ""), user(8, "Bob", "")]), &[]);
    let svc = |action, from| {
        msg::message(
            &tl::enums::Message::Service(service(peer_chat(3), 9, from, action)),
            &dir,
        )
        .expect("mapped")
    };
    let text = |m: crate::model::Message| match m.body {
        Body::Service(s) => s,
        other => panic!("not service: {other:?}"),
    };
    let joined = svc(
        A::ChatAddUser(tl::types::MessageActionChatAddUser { users: vec![7] }),
        Some(peer_user(7)),
    );
    assert_eq!(text(joined), "Ann joined the group");
    let added = svc(
        A::ChatAddUser(tl::types::MessageActionChatAddUser { users: vec![8] }),
        Some(peer_user(7)),
    );
    assert_eq!(text(added), "Ann added Bob");
    let renamed = svc(
        A::ChatEditTitle(tl::types::MessageActionChatEditTitle {
            title: "new\u{202E}eltit\nx".into(),
        }),
        Some(peer_user(7)),
    );
    assert_eq!(text(renamed), "Ann renamed the group to «neweltit x»");
    assert_eq!(text(svc(A::PinMessage, None)), "Someone pinned a message");
    assert_eq!(text(svc(A::Empty, None)), "Service message");
    let custom = svc(
        A::CustomAction(tl::types::MessageActionCustomAction {
            message: "y\u{0007}".repeat(MAX_SERVICE_BYTES),
        }),
        None,
    );
    assert!(custom.is_valid());
    assert!(text(custom).len() <= MAX_SERVICE_BYTES);
}

#[test]
fn forum_topics_and_replies() {
    use tl::enums::MessageReplyHeader as H;
    let mut topic_root = reply_header(Some(7), None);
    topic_root.forum_topic = true;
    assert_eq!(
        reply_and_topic(Some(&H::Header(topic_root))),
        (None, Some(7))
    );
    let mut in_topic = reply_header(Some(9), Some(7));
    in_topic.forum_topic = true;
    assert_eq!(
        reply_and_topic(Some(&H::Header(in_topic))),
        (Some(MsgId(9)), Some(7))
    );
    assert_eq!(
        reply_and_topic(Some(&H::Header(reply_header(Some(9), None)))),
        (Some(MsgId(9)), None)
    );
    let mut elsewhere = reply_header(Some(9), None);
    elsewhere.reply_to_peer_id = Some(peer_user(1));
    assert_eq!(reply_and_topic(Some(&H::Header(elsewhere))), (None, None));
    assert_eq!(reply_and_topic(None), (None, None));

    let created = tl::enums::Message::Service(service(
        peer_channel(10),
        42,
        None,
        tl::enums::MessageAction::TopicCreate(tl::types::MessageActionTopicCreate {
            title_missing: false,
            title: "News".into(),
            icon_color: 0,
            icon_emoji_id: None,
        }),
    ));
    let out = msg::message(&created, &Directory::default()).unwrap();
    assert_eq!(out.thread.topic, Some(42));
}

#[test]
fn empty_messages_and_out_of_range_peers_are_skipped() {
    let empty = tl::enums::Message::Empty(tl::types::MessageEmpty {
        id: 1,
        peer_id: None,
    });
    assert!(msg::message(&empty, &Directory::default()).is_none());
    let bad = tl::enums::Message::Message(message(peer_user(0), 1, "x"));
    assert!(msg::message(&bad, &Directory::default()).is_none());
    assert_eq!(chat_id(&peer_channel(i64::MAX)), None);
    assert_eq!(chat_id(&peer_chat(-5)), None);
}

#[test]
fn broadcast_posts_are_excluded() {
    let mut bc = channel(10, "news");
    bc.broadcast = true;
    bc.megagroup = false;
    let dir = Directory::from_lists(&[], &[tl::enums::Chat::Channel(bc)]);
    let post = message(peer_channel(10), 1, "x");
    assert!(msg::message(&tl::enums::Message::Message(post), &dir).is_none());
    let mut flagged = message(peer_channel(11), 1, "x");
    flagged.post = true;
    let none = Directory::default();
    assert!(msg::message(&tl::enums::Message::Message(flagged), &none).is_none());
}

// --- peers ---

#[test]
fn input_peers_need_a_matching_hash() {
    let with_hash = PeerInfo::User {
        id: 7,
        auth: Some(grammers_client::session::types::PeerAuth::from_hash(49)),
        bot: None,
        is_self: None,
    };
    let without = PeerInfo::User {
        id: 7,
        auth: None,
        bot: None,
        is_self: None,
    };
    assert!(matches!(
        input_peer(uid(7), Some(&with_hash)),
        Some(tl::enums::InputPeer::User(p)) if p.access_hash == 49
    ));
    assert_eq!(input_peer(uid(7), Some(&without)), None);
    assert_eq!(input_peer(uid(8), Some(&with_hash)), None);
    assert_eq!(input_peer(uid(7), None), None);
    let group = ChatId {
        kind: PeerKind::Chat,
        id: 3,
    };
    assert!(matches!(
        input_peer(group, None),
        Some(tl::enums::InputPeer::Chat(_))
    ));
    assert_eq!(user_chat(0), None);
}

#[test]
fn min_constructors_carry_no_hash() {
    let mut u = user(7, "Ann", "");
    u.min = true;
    let dir = Directory::from_lists(&users(vec![u]), &[]);
    assert!(dir.get(uid(7)).unwrap().info.auth().is_none());
    assert_eq!(dir.infos().count(), 0, "a min user cannot address a chat");
}

// --- dialogs ---

fn parts<'a>(
    dialogs: &'a [tl::enums::Dialog],
    messages: &'a [tl::enums::Message],
    users: &'a [tl::enums::User],
    chats: &'a [tl::enums::Chat],
) -> DialogsParts<'a> {
    DialogsParts {
        dialogs,
        messages,
        users,
        chats,
        complete: true,
    }
}

#[test]
fn dialogs_map_kinds_counts_previews_and_exclude_broadcasts() {
    let mut bc = channel(20, "news");
    bc.broadcast = true;
    bc.megagroup = false;
    let mut giga = channel(21, "huge");
    giga.gigagroup = true;
    let mut forum = channel(22, "forum");
    forum.forum = true;
    let chats = vec![
        tl::enums::Chat::Channel(bc),
        tl::enums::Chat::Channel(giga),
        tl::enums::Chat::Channel(forum),
        tl::enums::Chat::Chat(basic_group(3, "group")),
    ];
    let mut bot = user(8, "Bot", "");
    bot.bot = true;
    let users = users(vec![user(7, "Ann", ""), bot]);
    let mut d7 = dialog(peer_user(7), 1);
    d7.pinned = true;
    d7.unread_count = 4;
    d7.unread_mentions_count = -3;
    let dialogs: Vec<tl::enums::Dialog> = [
        d7,
        dialog(peer_user(8), 2),
        dialog(peer_channel(20), 3),
        dialog(peer_channel(21), 4),
        dialog(peer_channel(22), 5),
        dialog(peer_chat(3), 6),
    ]
    .into_iter()
    .map(tl::enums::Dialog::Dialog)
    .collect();
    let messages = vec![tl::enums::Message::Message(message(
        peer_user(7),
        1,
        "last\nline",
    ))];
    let dir = Directory::from_lists(&users, &chats);
    let out = lists::dialogs(&parts(&dialogs, &messages, &users, &chats), &dir, 100);
    assert_eq!(out.excluded, 1);
    assert_eq!(out.next, None);
    let ids: Vec<i64> = out.chats.iter().map(|c| c.id.id).collect();
    assert_eq!(ids, vec![7, 8, 21, 22, 3]);
    let ann = &out.chats[0];
    assert!(ann.pinned);
    assert_eq!((ann.unread, ann.unread_mentions), (4, 0));
    assert_eq!(ann.last, Some(Preview::Text("last line".into())));
    assert_eq!(ann.last_date, 1_700_000_001);
    assert_eq!(out.chats[1].kind, ChatKind::Bot);
    assert_eq!(
        out.chats[2].kind,
        ChatKind::Supergroup {
            forum: false,
            gigagroup: true
        }
    );
    assert!(!out.chats[2].can_send, "gigagroups are read-only");
    assert!(matches!(
        out.chats[3].kind,
        ChatKind::Supergroup { forum: true, .. }
    ));
    assert_eq!(out.chats[4].kind, ChatKind::Group);
    assert!(out.chats.iter().all(crate::model::ChatSummary::is_valid));
    assert_eq!(out.pins.len(), 5);
}

#[test]
fn dialogs_cursor_follows_the_last_raw_dialog() {
    let mut bc = channel(20, "news");
    bc.broadcast = true;
    let chats = vec![tl::enums::Chat::Channel(bc)];
    let users = users(vec![user(7, "Ann", "")]);
    let dialogs: Vec<tl::enums::Dialog> = vec![
        tl::enums::Dialog::Dialog(dialog(peer_user(7), 1)),
        tl::enums::Dialog::Dialog(dialog(peer_channel(20), 9)),
    ];
    let messages = vec![
        tl::enums::Message::Message(message(peer_user(7), 1, "a")),
        tl::enums::Message::Message(message(peer_channel(20), 9, "b")),
    ];
    let dir = Directory::from_lists(&users, &chats);
    let page = lists::dialogs(
        &DialogsParts {
            complete: false,
            ..parts(&dialogs, &messages, &users, &chats)
        },
        &dir,
        2,
    );
    let next = page.next.expect("a full slice continues");
    assert_eq!(next.offset_peer, Some(cid(20)), "even when excluded");
    assert_eq!((next.offset_id, next.offset_date), (9, 1_700_000_009));
    let short = lists::dialogs(
        &DialogsParts {
            complete: false,
            ..parts(&dialogs, &messages, &users, &chats)
        },
        &dir,
        3,
    );
    assert_eq!(short.next, None, "a short slice is the last page");
}

#[test]
fn folders_and_unknown_peers_are_skipped() {
    let users = users(vec![user(7, "Ann", "")]);
    let dialogs = vec![
        tl::enums::Dialog::Dialog(dialog(peer_user(99), 1)),
        tl::enums::Dialog::Dialog(dialog(peer_user(7), 2)),
    ];
    let dir = Directory::from_lists(&users, &[]);
    let out = lists::dialogs(&parts(&dialogs, &[], &users, &[]), &dir, 100);
    assert_eq!(out.chats.len(), 1);
    assert_eq!(out.chats[0].last, None);
}

// --- history ---

#[test]
fn history_is_ascending_deduplicated_and_bounded_by_the_query() {
    let raw: Vec<tl::enums::Message> = [5, 3, 4, 3, 1]
        .into_iter()
        .map(|id| tl::enums::Message::Message(message(peer_user(7), id, "m")))
        .chain([tl::enums::Message::Message(message(
            peer_user(8),
            2,
            "other chat",
        ))])
        .chain([tl::enums::Message::Empty(tl::types::MessageEmpty {
            id: 6,
            peer_id: None,
        })])
        .collect();
    let parts = MessagesParts {
        messages: &raw,
        users: &[],
        chats: &[],
        complete: false,
    };
    let dir = Directory::default();
    let ids = |q| {
        lists::history(&parts, &dir, uid(7), q, 100)
            .messages
            .iter()
            .map(|m| m.id.0)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(HistoryQuery::Tail), vec![1, 3, 4, 5]);
    assert_eq!(ids(HistoryQuery::Before(MsgId(4))), vec![1, 3]);
    assert_eq!(ids(HistoryQuery::After(MsgId(3))), vec![4, 5]);
    assert!(!lists::history(&parts, &dir, uid(7), HistoryQuery::Tail, 100).has_more);
    assert!(lists::history(&parts, &dir, uid(7), HistoryQuery::Tail, 7).has_more);
    let complete = MessagesParts {
        complete: true,
        ..parts
    };
    assert!(!lists::history(&complete, &dir, uid(7), HistoryQuery::Tail, 7).has_more);
}

// --- live updates ---

#[test]
fn live_updates_map_to_domain_updates() {
    use tl::enums::Update as U;
    let dir = Directory::default();
    let new = U::NewMessage(tl::types::UpdateNewMessage {
        message: tl::enums::Message::Message(message(peer_user(7), 1, "hi")),
        pts: 1,
        pts_count: 1,
    });
    assert!(matches!(lists::update(&new, &dir), Some(Update::NewMessage(m)) if m.id == MsgId(1)));
    let mut edited = message(peer_channel(10), 2, "fixed");
    edited.edit_date = Some(5);
    let edit = U::EditChannelMessage(tl::types::UpdateEditChannelMessage {
        message: tl::enums::Message::Message(edited),
        pts: 1,
        pts_count: 1,
    });
    assert!(
        matches!(lists::update(&edit, &dir), Some(Update::Edited(m)) if m.edit_date == Some(5))
    );
    let del = U::DeleteMessages(tl::types::UpdateDeleteMessages {
        messages: vec![1, 2],
        pts: 1,
        pts_count: 1,
    });
    assert!(matches!(
        lists::update(&del, &dir),
        Some(Update::Deleted { chat: None, ids }) if ids == vec![MsgId(1), MsgId(2)]
    ));
    let del_ch = U::DeleteChannelMessages(tl::types::UpdateDeleteChannelMessages {
        channel_id: 10,
        messages: (0..i32::try_from(MAX_DELETED_IDS).unwrap() + 5).collect(),
        pts: 1,
        pts_count: 1,
    });
    assert!(matches!(
        lists::update(&del_ch, &dir),
        Some(Update::Deleted { chat: Some(c), ids }) if c == cid(10) && ids.len() == MAX_DELETED_IDS
    ));
    let read = U::ReadChannelInbox(tl::types::UpdateReadChannelInbox {
        folder_id: None,
        channel_id: 10,
        max_id: 9,
        still_unread_count: 2,
        pts: 1,
    });
    assert!(matches!(
        lists::update(&read, &dir),
        Some(Update::ReadInbox { chat, still_unread: 2 }) if chat == cid(10)
    ));
}

#[test]
fn ignored_and_excluded_updates_map_to_nothing() {
    use tl::enums::Update as U;
    let topic_read = U::ReadHistoryInbox(tl::types::UpdateReadHistoryInbox {
        folder_id: None,
        peer: peer_channel(10),
        top_msg_id: Some(3),
        max_id: 9,
        still_unread_count: 1,
        pts: 1,
        pts_count: 1,
    });
    assert!(lists::update(&topic_read, &Directory::default()).is_none());
    let id_map = U::MessageId(tl::types::UpdateMessageId {
        id: 1,
        random_id: 2,
    });
    assert!(lists::update(&id_map, &Directory::default()).is_none());
    let mut bc = channel(10, "news");
    bc.broadcast = true;
    let dir = Directory::from_lists(&[], &[tl::enums::Chat::Channel(bc)]);
    let del = U::DeleteChannelMessages(tl::types::UpdateDeleteChannelMessages {
        channel_id: 10,
        messages: vec![1],
        pts: 1,
        pts_count: 1,
    });
    assert!(lists::update(&del, &dir).is_none());
}

// --- send responses ---

#[test]
fn sent_ids_come_only_from_our_random_id() {
    use tl::enums::Updates as U;
    let short = U::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
        out: true,
        id: 77,
        pts: 1,
        pts_count: 1,
        date: 0,
        media: None,
        entities: None,
        ttl_period: None,
    });
    assert_eq!(lists::sent_id(&short, 5), Some(MsgId(77)));
    let full = |random_id| {
        U::Updates(tl::types::Updates {
            updates: vec![tl::enums::Update::MessageId(tl::types::UpdateMessageId {
                id: 78,
                random_id,
            })],
            users: vec![],
            chats: vec![],
            date: 0,
            seq: 0,
        })
    };
    assert_eq!(lists::sent_id(&full(5), 5), Some(MsgId(78)));
    assert_eq!(lists::sent_id(&full(6), 5), None);
    assert_eq!(lists::sent_id(&U::TooLong, 5), None);
}
