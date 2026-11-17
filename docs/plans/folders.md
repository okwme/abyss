# abyss plan: archive and chat folders

Status: **draft v1** (2026-11-17), proposed by an outside contributor in three PRs. It has not been through the GPT-6 Astra review loop; the PRs ask for it.

Telegram has two unrelated things called folders:
- **Peer folders**: every chat is in exactly one, either the main list (folder 0) or the archive (folder 1). Moving a chat is `folders.editPeerFolders` (`api.tl:3172`); moves arrive as `updateFolderPeers` (`api.tl:393`).
- **Chat folders** (`dialogFilter`, `api.tl:1394`): user-defined filtered views of the whole list ("Work", "Unread"), synced across devices. They are listed by `messages.getDialogFilters` (`api.tl:2734`) and edited with `messages.updateDialogFilter` (`api.tl:2736`). Changes arrive as `updateDialogFilter`, `updateDialogFilterOrder` and `updateDialogFilters` (`api.tl:402-404`).

Today abyss shows one flat list. Stage 1 requests dialogs without a `folder_id` (section 7), which returns every dialog of both peer folders, each carrying its own `folder_id` (risk 8). Archived chats are therefore mixed into the main list, which makes a long-lived account's list hard to use.

The work is split so each part is reviewable and useful on its own:

| Part | PR | Scope | Writes to the account |
|---|---|---|---|
| A | 1 | Archive as a separate view | No |
| B | 2 | Chat folders, read-only | No |
| C | 3 | Archiving and folder editing | Yes |

## A. The archive (PR 1)

**Model.** `ChatSummary` gains `folder: PeerFolder` (`Main | Archive`). The flag lives on the summary because the list stays **one** list: core keeps a single server-ordered `Dialogs` holding both peer folders, and the UI shows one view of it at a time. Nothing about pagination, merging, provisional rows or the 1,000-dialog cap changes.

**Mapping.** `dialog.folder_id == 1` maps to `Archive`. Any other id, or none, maps to `Main`. The dialogs request is unchanged (`folder_id` unset).

**Live moves.** `updateFolderPeers` maps to `Update::Folders(Vec<(ChatId, PeerFolder)>)`, capped at 1,000 entries and with out-of-range peers dropped. Core's `Dialogs::set_folder` changes a listed chat in place: no reorder and no `rev` bump. Each change is sent as `ChatBumped`, which the UI already applies in place when no newer message comes with it. The cached list is rewritten (`PutChats`), so a restart shows the same split. Unlisted chats are ignored; the next dialogs page brings them with their folder. Telegram unarchives a chat on a new message unless it is muted, and sends `updateFolderPeers` when it does, so no client-side rule is needed.

**Cache.** postcard rows are positional, so schema version 4 drops the cached chat list (version 3 rows cannot be decoded). Messages are kept. This is the same pattern as 2→3.

**UI.**
- `ChatList` has a `Shown` view (`Main | Archive`). `rows()` yields `SidebarRow::Archive(ArchiveEntry)` and then the main chats, or only the archived chats.
- In the main view, the archive's entry leads the list once an archived chat is loaded. It shows how many archived chats are loaded and how many are unread, and opens the archive. In the archive view, the heading reads "‹ Archive" and leads back.
- Pagination is driven by the shown view's length. The server traverses both folders together, so the next page may hold more of either. A view with no row wants the next page at once, because nothing renders to ask for it.
- Selection, drafts and the open chat do not depend on the view.

**Limits.** The 1,000-dialog cap counts both views. An account with more than 1,000 dialogs may see fewer archived chats than it has. This is the same bound as today, and the entry's count says "loaded".

**Tests.**
- Mapping: folder ids and `updateFolderPeers`, including out-of-range peers.
- Store: `set_folder` changes in place and is idempotent.
- Actor: a live move emits one `ChatBumped` and ignores repeats and unlisted chats.
- Cache: the 3→4 drop and the `folder` round trip.
- App: the views, the archive entry and pagination by view.
- A snapshot pair (`archive-day/night.png`), and the demo archives every sixth chat.

**Live check (task 12).** Confirm that an unset `folder_id` returns archived dialogs with `folder_id = 1`, as Telethon documents. If it instead returns a `dialogFolder` entry in their place, part A needs a second traversal with `folder_id = 1`.

## B. Chat folders, read-only (PR 2)

**Backend.** `Client::chat_folders() -> Vec<ChatFolder>` calls `messages.getDialogFilters` with retry class `Read`.
- Each `dialogFilter` maps to `ChatFolder { id, title, rules, pinned, include, exclude }`.
- `dialogFilterDefault` (the "All chats" position) is skipped.
- `dialogFilterChatlist` (a shared folder) maps with `include` only and `shared = true`.
- Titles go through `display_name`. Entities and animated emoji are dropped.
- `rules` holds the flags: contacts, non-contacts, groups, broadcasts, bots, exclude muted, exclude read, exclude archived.
- Peers are `InputPeer`s. Each is mapped to a `ChatId` with the same range checks as `chat_id`. `inputPeerSelf` maps to the signed-in user's id, and anything else that does not map is dropped.
- Bounds: at most 64 folders, at most 512 peers per folder across `pinned + include + exclude` (Telegram's own limits are 30 and 200), and the response is rejected whole above them (`Oversized`).

**Model additions for the rules.**
- `ChatSummary` gains `muted: bool`, computed from `dialog.notify_settings.mute_until` against the clock when the page is mapped.
- `ChatKind::Private` becomes `Private { contact }`, from `user.contact`. It mirrors `Supergroup { forum, gigagroup }` and keeps `ChatSummary` within `struct_excessive_bools`.
- `FolderRules` is a `u8` flag set, like `StyleFlags`.
- Cache schema version 5.

**Core.**
- Folders are loaded after each successful first dialogs page (bootstrap and refresh) and sent as `Event::ChatFolders(Vec<ChatFolder>)`.
- `updateDialogFilter`, `updateDialogFilterOrder` and `updateDialogFilters` all map to `Update::ChatFoldersChanged`. Core refetches, coalesced through `FoldersLoad { Idle, Running, Again }`: at most one load in flight and one wanted after it.
- A failed load is only logged. The next first page asks again.
- Folders are not cached on disk. They come back with the first page.

**Matching** follows Telegram's rule and runs in the UI over the loaded list:
- A chat in `pinned` or `include` is always shown.
- A chat in `exclude` is never shown.
- Any other chat is shown when its kind matches a flag: `contacts` and `non_contacts` for users, `bots`, `groups` for basic groups and supergroups. Broadcasts never reach the list.
- A chat that matches is then dropped if `exclude_muted` and it is muted, if `exclude_read` and it has no unread messages, or if `exclude_archived` and it is archived.
- The folder's `pinned` chats come first, in folder order, then the rest in list order.

**UI.** A strip of folder tabs under the heading: "All", then each folder in server order. "All" is today's view, archive entry included. A folder view shows only matching chats from both peer folders, with no archive entry. The `Shown` enum gains `Folder(id)`. A folder that is removed while shown falls back to "All".

**Limits.** Matching sees only loaded dialogs (at most 1,000). A folder's include list may name chats beyond the cap, which simply don't show. That is the same limit as the main list.

## C. Archiving and folder editing (PR 3)

These are the first account-changing calls besides sending and read marks, so they stay narrow.

**Commands.**
- `SetFolder { chat, folder }` calls `folders.editPeerFolders`. On success core applies the move itself, without waiting for the echoed `updateFolderPeers`, which then changes nothing.
- `EditChatFolder(FolderEdit)`, where `FolderEdit` is `AddChat { folder, chat }`, `RemoveChat { folder, chat }`, `Create { title, chat }` or `Delete { folder }`. It calls `messages.updateDialogFilter`.

**Round-tripping what abyss does not model.** The backend keeps the raw `dialogFilter` values from the last `getDialogFilters`. An edit changes only the peer lists or the title of that raw value and sends it back, so emoticons, colors, title entities and peers abyss could not map (or that lie beyond the dialog cap) are never dropped. An edit to a folder whose raw value is missing (not loaded yet) is refused with `NotFound`. Shared (chatlist) folders are read-only and refused, since editing them needs the `chatlists.*` calls. New folders get the smallest free id ≥ 2, and the title is limited to 12 characters, as Telegram requires.

**Retry class.** Edits are idempotent (they set state), so they use the `Read` retry class. An edit that fails shows a notice and leaves the UI unchanged until the server's update arrives.

**UI.**
- Right-click a chat row for a small menu: Archive or Unarchive, then one toggle per editable folder, then "New folder with this chat…".
- The new-folder name uses the existing `TextInput`.
- Deleting a folder is a right-click on its tab, with a confirmation.

**Out of scope.** Folder rules, reordering folders, shared folders and invite links.
