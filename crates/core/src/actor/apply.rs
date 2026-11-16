//! Applying completions and live updates to the store, cache and UI.

use super::Actor;
use super::jobs::{CallError, CallResult, DialogsStep, Done, Outcome, PAGE};
use crate::api::{ConnState, Event, HistoryDir, UserError};
use crate::backend::{Backend, BackendError, DialogsPage, HistoryPage, HistoryQuery, Update};
use crate::cache::CacheCmd;
use crate::model::{Body, ChatId, ChatSummary, Message, MsgId, Preview};
use crate::sanitize::{MAX_PREVIEW_BYTES, clean};
use crate::store::{Bump, Change, Unread, ViewGen};

pub(super) fn user_error(e: CallError) -> UserError {
    match e {
        CallError::Backend(BackendError::Rejected | BackendError::Unauthorized) => {
            UserError::Rejected
        }
        CallError::Backend(BackendError::FloodWait { seconds }) => UserError::FloodWait { seconds },
        CallError::Backend(BackendError::Transient) | CallError::Timeout => UserError::Network,
    }
}

fn preview_of(m: &Message) -> Preview {
    match &m.body {
        Body::Text(rt) => Preview::Text(clean(rt.text(), MAX_PREVIEW_BYTES)),
        Body::Media { kind, .. } => Preview::Media(kind.clone()),
        Body::Service(_) | Body::Unsupported => Preview::Service,
    }
}

const MIN_ID: MsgId = MsgId(i32::MIN);
const MAX_ID: MsgId = MsgId(i32::MAX);

/// A history response, unpacked.
struct HistoryReply {
    chat: ChatId,
    view: ViewGen,
    dir: HistoryDir,
    query: HistoryQuery,
    page: HistoryPage,
}

impl<B: Backend> Actor<B> {
    pub(super) async fn on_done(&mut self, done: Done) {
        let current = done.cgen == self.cgen;
        if let Some(permit) = done.parked {
            // A stale generation's client has retired, so its permit is free again.
            if current {
                self.admission.park(permit);
            }
        }
        match done.outcome {
            Outcome::Send { local_id, res } => self.on_send(local_id, res).await,
            _ if !current => {}
            Outcome::Dialogs { step, res } => self.on_dialogs(step, res).await,
            Outcome::History {
                chat,
                view,
                dir,
                query,
                res,
            } => {
                self.release_history(chat, view);
                match res {
                    Ok(page) => {
                        self.on_history(HistoryReply {
                            chat,
                            view,
                            dir,
                            query,
                            page,
                        })
                        .await;
                    }
                    Err(e) => self.error(user_error(e)).await,
                }
            }
            // Read marks are best effort; the next read covers a lost one.
            Outcome::Read => {}
            Outcome::CacheChats(chats) => self.on_cache_chats(chats).await,
            Outcome::CacheHistory {
                chat,
                view,
                dir,
                anchor,
                messages,
            } => {
                self.on_cache_history(chat, view, dir, anchor, messages)
                    .await;
            }
            Outcome::UpdatesEnded(e) => {
                // Task 4 turns this into a client rebuild.
                self.emit(Event::Connection(ConnState::Connecting)).await;
                self.error(user_error(CallError::Backend(e))).await;
            }
        }
    }

    /// Ends the single-flight history load for `chat` if it belongs to `view`.
    pub(super) fn release_history(&mut self, chat: ChatId, view: ViewGen) {
        if self.history_busy.get(&chat) == Some(&view) {
            self.history_busy.remove(&chat);
        }
    }

    async fn cache_write(&self, cmd: CacheCmd) {
        if let Some(cache) = &self.cache {
            cache.write(cmd).await;
        }
    }

    async fn on_dialogs(&mut self, step: DialogsStep, res: CallResult<DialogsPage>) {
        self.dialogs_busy = false;
        match res {
            Ok(page) => {
                let event = match step {
                    DialogsStep::First => {
                        self.dialogs.reset(page.chats);
                        Event::DialogsReset(self.dialogs.as_slice().to_vec())
                    }
                    DialogsStep::Next => Event::DialogsAppended(self.dialogs.append(page.chats)),
                };
                // `PutChats` replaces the cached list, so it always gets the whole list.
                if self.cache.is_some() {
                    let all = self.dialogs.as_slice().to_vec();
                    self.cache_write(CacheCmd::PutChats(all)).await;
                }
                self.dialogs_loaded = true;
                self.dialogs_next = page.next;
                self.emit(event).await;
            }
            Err(e) => self.error(user_error(e)).await,
        }
        if std::mem::take(&mut self.dialogs_queued) {
            self.load_dialogs().await;
        }
    }

    async fn on_cache_chats(&mut self, chats: Option<Vec<ChatSummary>>) {
        let Some(chats) = chats.filter(|c| !c.is_empty()) else {
            return;
        };
        if self.dialogs_loaded {
            return; // the server answered first
        }
        self.dialogs.reset(chats);
        self.emit(Event::DialogsReset(self.dialogs.as_slice().to_vec()))
            .await;
    }

    /// Emits the messages `change` inserted (ascending) and what it removed.
    /// `has_more` mirrors the window's own flag for `dir`, so the UI and the
    /// store can never disagree about whether more history exists.
    async fn emit_history(&self, chat: ChatId, view: ViewGen, dir: HistoryDir, change: Change) {
        let Some(w) = self.windows.get(chat) else {
            return;
        };
        let has_more = match dir {
            HistoryDir::Tail | HistoryDir::Older => w.has_older(),
            HistoryDir::Newer => w.has_newer(),
        };
        let mut ids = change.inserted;
        ids.sort_unstable();
        let messages = w
            .messages()
            .iter()
            .filter(|m| ids.binary_search(&m.id).is_ok())
            .cloned()
            .collect();
        self.emit(Event::History {
            chat,
            view,
            dir,
            messages,
            removed: change.removed,
            has_more,
        })
        .await;
    }

    async fn on_cache_history(
        &mut self,
        chat: ChatId,
        view: ViewGen,
        dir: HistoryDir,
        anchor: Option<MsgId>,
        messages: Option<Vec<Message>>,
    ) {
        let messages = messages.unwrap_or_default();
        if dir == HistoryDir::Tail {
            return self.on_cache_tail(chat, view, messages).await;
        }
        self.release_history(chat, view);
        let Some(anchor) = anchor else { return };
        if messages.is_empty() {
            // The cache ran out; continue from the server.
            if self.windows.get(chat).is_some_and(|w| w.view_gen() == view) {
                self.history_busy.insert(chat, view);
                self.fetch_more(chat, view, dir, anchor).await;
            }
            return;
        }
        // Older: the cache never proves the start of history, so the server
        // decides. Newer: a short page means the cache reached its newest row.
        let has_more =
            dir == HistoryDir::Older || u32::try_from(messages.len()).is_ok_and(|n| n >= PAGE);
        let page = HistoryPage { messages, has_more };
        let res = match dir {
            HistoryDir::Older => self.windows.prepend(chat, view, page),
            _ => self.windows.append_newer(chat, view, page),
        };
        if let Ok(change) = res {
            self.emit_history(chat, view, dir, change).await;
        }
    }

    /// Shows cached history, then asks the server for the tail. The server request
    /// is chained after the cache read so `on_tail` always sees what the cache held.
    async fn on_cache_tail(&mut self, chat: ChatId, view: ViewGen, messages: Vec<Message>) {
        if self.windows.get(chat).is_none_or(|w| w.view_gen() != view) {
            return self.release_history(chat, view);
        }
        let page = HistoryPage {
            messages,
            has_more: true,
        };
        if !page.messages.is_empty()
            && let Ok(change) = self.windows.prepend(chat, view, page)
            && !change.inserted.is_empty()
        {
            self.emit_history(chat, view, HistoryDir::Tail, change)
                .await;
        }
        self.request_tail(chat, view).await;
    }

    async fn on_history(&mut self, p: HistoryReply) {
        if self
            .windows
            .get(p.chat)
            .is_none_or(|w| w.view_gen() != p.view)
        {
            return; // stale: closed, evicted or replaced
        }
        match p.dir {
            HistoryDir::Tail => self.on_tail(p).await,
            HistoryDir::Older => {
                let hi = match p.query {
                    HistoryQuery::Before(id) => MsgId(id.0.saturating_sub(1)),
                    _ => MAX_ID,
                };
                let lo = if p.page.has_more {
                    p.page.messages.first().map_or(hi, |m| m.id)
                } else {
                    MIN_ID
                };
                self.reconcile(p.chat, lo, hi, &p.page.messages).await;
                if let Ok(change) = self.windows.prepend(p.chat, p.view, p.page) {
                    self.emit_history(p.chat, p.view, p.dir, change).await;
                }
            }
            HistoryDir::Newer => {
                let lo = match p.query {
                    HistoryQuery::After(id) => MsgId(id.0.saturating_add(1)),
                    _ => MIN_ID,
                };
                let hi = if p.page.has_more {
                    p.page.messages.last().map_or(lo, |m| m.id)
                } else {
                    MAX_ID
                };
                self.reconcile(p.chat, lo, hi, &p.page.messages).await;
                if let Ok(change) = self.windows.append_newer(p.chat, p.view, p.page) {
                    self.emit_history(p.chat, p.view, p.dir, change).await;
                }
            }
        }
    }

    /// Writes a server page over `[lo, hi]` of a contiguous cache.
    async fn reconcile(&self, chat: ChatId, lo: MsgId, hi: MsgId, page: &[Message]) {
        if self.contiguous.contains(&chat) && lo <= hi {
            self.cache_write(CacheCmd::Reconcile {
                chat,
                lo,
                hi,
                messages: page.to_vec(),
            })
            .await;
        }
    }

    /// The server tail replaces the window (which may hold cached history).
    ///
    /// If the page overlaps the cached history, the cache stays and only the
    /// covered range is reconciled. Otherwise there may be a gap, so the chat's
    /// cache is replaced by the page. Either way the cache is then contiguous with
    /// the live tail for the rest of the session.
    async fn on_tail(&mut self, p: HistoryReply) {
        let HistoryReply { chat, mut page, .. } = p;
        let Some(w) = self.windows.get(chat) else {
            return;
        };
        let page_max = page.messages.last().map(|m| m.id);
        // Live messages that arrived after the request was served.
        let is_late = |m: &&Message| page_max.is_none_or(|max| m.id > max);
        let late: Vec<Message> = w.messages().iter().filter(is_late).cloned().collect();
        let cached_newest = w
            .messages()
            .iter()
            .rev()
            .find(|m| !is_late(m))
            .map(|m| m.id);
        let overlaps = !page.has_more
            || cached_newest.is_none()
            || page
                .messages
                .first()
                .zip(cached_newest)
                .is_some_and(|(first, newest)| first.id <= newest);
        page.messages.extend(late);
        if self.cache.is_some() {
            let lo = match page.messages.first() {
                Some(first) if overlaps && page.has_more => first.id,
                _ => MIN_ID,
            };
            self.contiguous.insert(chat);
            self.reconcile(chat, lo, MAX_ID, &page.messages).await;
        }
        let Some(view) = self.windows.invalidate(chat) else {
            return;
        };
        if let Ok(change) = self.windows.prepend(chat, view, page) {
            self.emit_history(chat, view, HistoryDir::Tail, change)
                .await;
        }
    }

    pub(super) async fn on_update(&mut self, update: Update) {
        match update {
            Update::NewMessage(m) => self.on_new_message(m).await,
            Update::Edited(m) => {
                let chat = m.thread.chat;
                if self.windows.replace(m.clone()) {
                    if self.contiguous.contains(&chat) {
                        self.cache_write(CacheCmd::PutMessages(chat, vec![m.clone()]))
                            .await;
                    }
                    self.emit(Event::Edited(m)).await;
                }
            }
            Update::Deleted {
                chat: Some(chat),
                ids,
            } => {
                let removed = self.windows.delete(chat, &ids);
                self.cache_write(CacheCmd::Delete(chat, ids)).await;
                if !removed.is_empty() {
                    self.emit(Event::Deleted { chat, ids: removed }).await;
                }
            }
            Update::Deleted { chat: None, ids } => {
                let removed = self.windows.delete_unscoped(&ids);
                self.cache_write(CacheCmd::DeleteUnscoped(ids)).await;
                for (chat, ids) in removed {
                    self.emit(Event::Deleted { chat, ids }).await;
                }
            }
            Update::ReadInbox { chat, still_unread } => {
                if let Some(summary) = self.dialogs.set_unread(chat, still_unread).cloned() {
                    self.emit(Event::ChatBumped(summary)).await;
                }
            }
        }
    }

    async fn on_new_message(&mut self, m: Message) {
        let chat = m.thread.chat;
        let window = self
            .windows
            .get(chat)
            .map(|w| (w.view_gen(), !w.has_newer()));
        let attached = self.open == Some(chat) && window.is_some_and(|(_, attached)| attached);
        let unread = if m.outgoing || attached {
            Unread::Unchanged
        } else {
            Unread::Increment
        };
        let bump = self.dialogs.bump(chat, preview_of(&m), m.date, unread);
        if let Bump::Updated(summary) = bump {
            self.emit(Event::ChatBumped(summary)).await;
        }
        let Some((view, _)) = window else { return };
        let id = m.id;
        let change = self.windows.upsert(m);
        if !change.inserted.contains(&id) {
            return;
        }
        if self.contiguous.contains(&chat)
            && let Some(stored) = self.windows.get(chat).and_then(|w| w.get(id)).cloned()
        {
            self.cache_write(CacheCmd::PutMessages(chat, vec![stored]))
                .await;
        }
        self.emit_history(chat, view, HistoryDir::Newer, change)
            .await;
    }
}
