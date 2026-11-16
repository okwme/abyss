# abyss Stage 1 plan: login, chat list, text messages

Status: **v3.1** (2026-11-16). This document is self-contained. It has been through three GPT-6 Astra review rounds; round 3's verdict was "ready after fixes", and v3.1 applies them (section 15).

Pinned upstreams:
- **grammers** `e61cf858` (codeberg.org/Lonami/grammers, the same code as v0.12.0). `G/` = `~/.cargo/git/checkouts/grammers-8937e3b5288aa015/e61cf85/`.
- **zed/gpui** `917559da`. `Z/` = `~/.cargo/git/checkouts/zed-a70e2ad075855582/917559d/crates/gpui/`.
- Paths written `api.tl:N` mean `G/grammers-tl-types/tl/api.tl`.

## 0. Goal and non-goals

Stage 1 delivers a signed-in macOS client that:
- lists private chats and groups;
- shows text history with formatting;
- sends text;
- applies live updates.

Everything must run against a fake backend, because we have no `api_id`/`api_hash` yet.

Out of scope for Stage 1:
- broadcast channels;
- forum topics (Stage 2);
- notifications, code signing, App Sandbox (Stage 3);
- any media download or decode, and avatars (Stage 4).

Design principle: **persist little, recover one way.** Only the critical login state survives a restart. Every failure is handled by rebuilding the client and reconciling.

## 1. Crates and threading

```
crates/core  (abyss-core)  no GPUI. Owns the core thread + tokio runtime, backend, client lifecycle,
                         in-memory store, persistence worker, domain model.
crates/app   (abyss)       GPUI only. Views and view-models; talks to core only through CoreHandle.
```

```rust
pub fn start(cfg: CoreConfig, backend: BackendChoice) -> Result<CoreHandle, StartError>;
pub struct CoreHandle {
    cmd: mpsc::Sender<Command>,      // bounded 256, data plane
    events: mpsc::Receiver<Event>,   // bounded 1024 + byte budget (section 6)
    control: ControlHandle,          // CancellationToken + shutdown/logout acks; never shares `cmd`
}
pub enum Command { SubmitPhone(String), SubmitCode(String), SubmitPassword(PasswordBuf), ResendCode,
                   RetryPasswordParams, LoadDialogs, OpenChat(ChatId), CloseChat(ChatId),
                   LoadOlder(ChatId), SendText { chat: ChatId, text: String, local_id: LocalId },
                   RetrySend(LocalId), MarkRead { chat: ChatId, max_id: MsgId }, Logout }
pub enum Event   { Auth(AuthState), Connection(ConnState), DialogsReset(Vec<ChatSummary>),
                   DialogsAppended(Vec<ChatSummary>), ChatBumped(ChatSummary),
                   History { chat: ChatId, view: ViewGen, older: Vec<Message>, has_more: bool },
                   ChatInvalidated(ChatId), NewMessage(Message), Edited(Message),
                   Deleted { chat: ChatId, ids: Vec<MsgId> }, SendState { local_id: LocalId, state: SendState },
                   Error(UserError) }
```

`BackendChoice` is either `Grammers(Credentials)` or `Fake(FakeScript)`. `Fake` exists only with the `fake` cargo feature (section 12).

**Core thread.** `start` spawns an OS thread named `abyss-core`. That thread builds a 2-worker tokio runtime, runs `rt.block_on(supervisor(..))`, then calls `rt.shutdown_timeout(1s)`. That call is legal there because the thread has already left the runtime's async context.

The GPUI main thread never blocks on core. Dropping `CoreHandle` cancels the token and detaches the thread; it never joins.

We don't use `gpui_tokio`, because it would make core depend on GPUI.

**Bridge to GPUI.**
- A single `cx.spawn` loop (`Z/src/app.rs:2557`) awaits `events.recv()`. The tokio receiver is runtime-agnostic because it only uses the waker.
- After each wake it drains up to 256 events with `try_recv` and applies them in one `entity.update`.
- The UI sends commands with `try_send`. A full queue shows a "busy" toast.

**Actor and admission.** One actor task owns the store and the auth state machine.
- **Concurrency cap.** Backend calls take a permit from `Semaphore(16)` *before* the task is spawned.
- **Serialized auth.** Auth commands run one at a time. A conflicting command gets `UserError::Busy`.
- **Single-flight.** History loads are single-flight per chat. `LoadDialogs` runs at most one at a time, with at most one more queued.
- **Coalesced read marks.** `MarkRead` keeps the highest id per chat and flushes after 500 ms.
- **Generations.** Every completion is tagged with a `ClientGen`, which bumps on every rebuild, login and logout. Chat-scoped completions also carry a `ViewGen`, which bumps on close, eviction or invalidation. Stale completions are dropped.
- **Deadlines.** Each call has a deadline: 30 s, or 60 s for auth.
- **Timed-out permits are parked.** When a call hits its deadline, its permit is *parked*, not returned. The request may still sit in grammers' unbounded queues: the pool's request channel (`G/grammers-mtsender/src/sender_pool.rs:154-158`) and the sender's `requests: Vec` (`G/grammers-mtsender/src/sender.rs:67, 140`). Parked permits are recycled only after the client instance has fully **retired** (section 2, step 6 of the rebuild procedure), because retirement drops those queues. So grammers can never hold more than 16 of our requests.

**Panic containment.** Every task is awaited through its `JoinHandle`, and a panic becomes `Event::Error(Internal)`. Known grammers panic sites on server data:
- `Dialog::new` (`G/grammers-client/src/peer/dialog.rs:49-66`)
- `check_password` `current_algo.clone().unwrap()` (`G/grammers-client/src/client/auth.rs:423`)
- `complete_login` `to_ref().await?.unwrap()` (`auth.rs:121`)

Release builds keep `panic = "unwind"`.

## 2. Client lifecycle: the single recovery mechanism

One "client instance" is `{ Session object, SenderPool runner task, Client, UpdateStream task }`, tagged with a `ClientGen`.

**Build.**
1. Create a **fresh** `AbyssSession` object (section 3), seeded from:
   - at process startup only, the critical state loaded from the Keychain;
   - on an in-process rebuild, the **live critical snapshot** from the retired instance (section 3). The Keychain is never re-read during a rebuild, so a `SignedInUnsaved` status and pending durability results carry over. The bounded carried `PeerRef` set is seeded as well.
2. `SenderPool::with_configuration(session, api_id, params)`. `params` sets `retry_policy: NoRetries` (`G/grammers-mtsender/src/configuration.rs:61, 106`) and `updates_channel_capacity: 1024` (`configuration.rs:63-66`; the default is 100, `:156`).
3. Spawn `runner.run()` as a supervised task.
4. `Client::with_configuration(handle, ..)` (`G/grammers-client/src/client/net.rs:54`).
5. If authorized, run the **bootstrap** (below). It creates the stream with `client.stream_updates(pool.updates, UpdatesConfiguration { catch_up: false })` (`G/grammers-client/src/client/updates.rs:69-72`). `catch_up: false` is also the default (`configuration.rs:163-166`), but we set it explicitly.

**Bootstrap: establish the update baseline before taking snapshots.**

How grammers sets the baseline:
- With `catch_up: false`, `UpdatesReceiver::create` starts from an empty message box (`G/grammers-mtsender/src/updates_receiver.rs:92-98`). It schedules a baseline only if the session knows the self user (`:100-101`).
- The **first `next()` call** sends `updates.getState` *before* waiting for any update (`:126-145`). It records the result through `session.set_update_state(UpdateState::All(..))` (`:132`) and `message_box.set_state`.
- If `getState` fails, the error is swallowed (`:142-145`) and no baseline is set.
- Updates at or below the baseline are ignored (`G/grammers-session/src/message_box/mod.rs:430`).

So:
1. Create the stream and spawn the consumer task **first**. Its first `next()` call starts `getState`.
2. `AbyssSession::set_update_state` signals `BaselineEstablished` on its first `UpdateState::All` for this instance. That call site is the only one we reach, because we never call `sync_update_state` (`updates_receiver.rs:244`).
3. Bootstrap waits for that signal, for at most 10 s. A timeout counts as a stall, which triggers a rebuild.
4. While bootstrap runs, the consumer **buffers** decoded updates instead of applying them. The buffer is capped at 4,096 updates; overflow aborts this bootstrap and rebuilds.
5. After the baseline, fetch the snapshots: the first page of dialogs, then the tail of each open chat. Then apply the buffered updates **merged over** the snapshots:
   - New messages are merged by id.
   - An edit wins if its `edit_date` is at least the snapshot's.
   - A deletion goes into a **bootstrap tombstone set** (at most 4,096 ids, cleared when bootstrap ends), so a snapshot fetched later can't bring the message back.
6. Switch the consumer to live mode.

The dialogs page doubles as the peer cache that gap resolution needs (`updates.rs:62-65`). Pinned peers are also seeded from the carried set.

Tests: a new message, an edit and a delete each arriving during bootstrap (before, during and after the snapshot fetch) end in the correct state. A `getState` failure leads to the timeout, then a rebuild.

`stream_updates` takes ownership of the unique `mpsc::Receiver` (`updates.rs:69`), and `UpdatesReceiver` keeps it (`G/grammers-mtsender/src/updates_receiver.rs:28-35`). It can't be extracted again, so a broken stream is always replaced by building a new pool.

**Rebuild triggers.** Any of these starts a rebuild:
- The runner task exits or panics. It re-raises connection-task panics (`sender_pool.rs:302-306`).
- `UpdateStream::next` (`updates.rs:31`) returns an error that's still there after 3 attempts on the *same* stream, with backoff of 0.5 s, 1 s and 2 s.
- **Transport stall:** 3 caller deadlines exceeded within 2 minutes, or all 16 permits parked.
- An authenticated `303` (migration) on a non-file request. See "DC migration" below.
- Wake from sleep (`NSWorkspaceDidWakeNotification`). Rather than track socket health across sleep, we rebuild.
- A bootstrap failure: the baseline wait times out, or the buffer overflows.

**Rebuild procedure (explicit retirement).**
1. Stop admission and bump `ClientGen`.
2. Cancel the instance's own tasks (the update consumer, the bootstrap, and in-flight command tasks bound to this instance) and **join** them. The join is capped at 3 s; any task still running is aborted and its `JoinHandle` awaited again.
3. Send `pool.handle.quit()` (`sender_pool.rs:175`) and await the runner `JoinHandle`, capped at 5 s.
   - A clean `run()` returns only after clearing connections and `join_all` (`sender_pool.rs:324-325`).
   - If the cap expires, call `abort()`, then **await the `JoinHandle` again** until it reports completion or cancellation. `abort()` only schedules the cancellation. Dropping the runner future also drops its `JoinSet`, which aborts the connection tasks without joining them. Those tasks (`run_sender`, `sender_pool.rs:524+`) never touch the session. Only runner methods write DC options: `create_connection` at `:394` and `update_config` at `:472-518`. So once the runner future has completed or been dropped, grammers can't make any more critical writes.
4. Revoke the retired `AbyssSession`'s write token. Any late call into it is dropped (section 3).
5. Capture the live critical snapshot and the carried `PeerRef` set: the open chats (at most 8) plus the peers of entries in the pending-send table (at most 256).
6. Only now drop the `Client` and `UpdateStream` and **recycle parked permits**. Requests left in grammers' queues have already received `InvocationError::Dropped` (`sender_pool.rs:156-159`).
7. Build again (section 2, "Build") with exponential backoff: 1 s up to 60 s plus jitter, reset after 5 minutes of stable operation.
8. Bootstrap, which includes reconcile, then resume admission.

`Event::Connection` reports `Rebuilding{attempt}` and `Online`.

**Reconcile** (bootstrap step 5):
1. Reload the first dialogs page, which emits `DialogsReset`.
2. Invalidate *every* cached message window (`ChatInvalidated`).
3. Refetch the open chat's newest 50 messages.

This is also the startup path. It recovers updates missed while offline (`catch_up: false`).

**Bounded refresh (no gap detection promised).** grammers gives no signal in two cases:
- "difference too long" advances the update state without delivering the missed messages (`G/grammers-session/src/message_box/mod.rs:675-680`);
- a channel difference ends early, as `Banned`, for untracked channels (section 3).

We don't claim to detect either case. Instead, a **light refresh** (the first dialogs page plus the open chat's tail, merged with the bootstrap rules) runs when the window is activated or focused, and every 5 minutes while the window is active. It's single-flight and skipped while a rebuild is running. Silent gaps between refreshes are a residual risk (risk 7). Follow-up: ask upstream to expose a gap or too-long signal.

**Pending sends survive rebuilds.** The account-level pending-send table (section 9) keeps each entry's `random_id` and replays eligible entries after `Online`.

**DC migration (verified).**
- grammers handles `303` only in `bot_sign_in` and `request_login_code` (`auth.rs:195-202, 273-280`) and for file transfers (`client/files.rs:32, 133`).
- Generic `invoke` passes it through (`net.rs:91-96`).
- Moving an authorization to a new home DC needs `auth.exportAuthorization` / `auth.importAuthorization` (`api.tl:2424-2425`). grammers' helper for this, `copy_auth_to_dc`, is crate-private (`net.rs:116`).
- Updates flow only on the connection flagged "home" when it was created (`sender_pool.rs:396-402`).

Telegram issues `USER_MIGRATE` during login, which grammers handles. Post-login home-DC migration for the message, dialog and history calls Stage 1 makes is effectively unreachable. **Decision:** we don't implement export/import in Stage 1. An authenticated non-file `303` surfaces `UserError::AccountMigrated`, rebuilds once, and if it recurs, asks the user to sign in again. A fresh login takes grammers' handled path. This is recorded as risk 6.

**Fault-injection tests** (`FakeBackend`):
- runner panic;
- stream error, both recoverable and persistent;
- stall (calls that never complete);
- too-long gap (state jumps, so reconcile must fetch the missing tail);
- authenticated 303;
- a pending send surviving a rebuild without changing its `random_id`;
- a runner that ignores `quit`, which forces abort; permits must be recycled only after its `JoinHandle` completes;
- a late `set_dc_option` arriving on a retired session, which must be dropped;
- a rebuild while in `SignedInUnsaved`: the status is kept and the Keychain is not read again.

## 3. Session and persistence

**What gets persisted.** Only critical state goes to the Keychain:

```rust
struct CriticalState { home_dc: i32, dc_options: Vec<DcOption> /* incl. auth keys */, self_user: Option<SelfUser> }
struct SelfUser { id: i64, access_hash: i64, bot: bool }
```

- There's no peer cache, no update state, no periodic snapshot and no ticker.
- Peers and update state live in memory and die with the client instance. The one exception is the bounded carried `PeerRef` set (section 2, rebuild step 5), which is passed to the next instance.
- Format: postcard with a `version: u16` prefix. An unknown version is discarded, which forces a new login.
- Item: Keychain generic password, service `dev.abyss.session`, never iCloud-synced. The crate is `security-framework` (`passwords::{set,get,delete}_generic_password`), the same backend `keyring` uses on macOS, minus the cross-platform layer.

**`AbyssSession: grammers_session::Session`.** `SessionData` and `MemorySession` can't export their data (`G/grammers-session/src/storages/memory.rs:23`), so we write our own.

- **Critical fields** live in an account-level `CriticalCell`, owned by the actor, that outlives client instances:

  ```rust
  struct CriticalCell { state: CriticalState, rev: u64, last_blob: Zeroizing<Vec<u8>>, gen: AccountGen,
                        durability: Durability /* Durable{rev} | Unsaved{rev, err} | Pending{rev} */ }
  ```

  - **`AccountGen`** is the persistence generation. It starts at login (or at process startup with a loaded item) and ends at logout. It is separate from `ClientGen`: rebuilds don't change it, and `rev` keeps increasing across rebuilds within one `AccountGen`.
  - **Write tokens.** Each `AbyssSession` instance holds a write token for the cell. The token is revoked at retirement (section 2, rebuild step 4), so a late mutation from a retired instance is dropped. The new instance gets a fresh token and starts from the same cell. That is the "live critical snapshot".
  - **Revisions.** Every critical mutation, *under the cell lock*, applies the change and serializes the state. **Only if the bytes differ from `last_blob`** does it increment `rev`, store `last_blob`, set `Pending{rev}` and send `PersistMsg::Write { gen, rev, blob }`. An identical mutation leaves `rev` unchanged. This covers the runner re-setting the same DC option on every reconnect (`create_connection` copies the stored option, sets the same auth key and calls `set_dc_option`; `sender_pool.rs:386-394`).
  - **Already-loaded state.** At startup, the state loaded from the Keychain becomes `rev = 0`, `last_blob = loaded bytes`, `Durable{0}`. A Barrier on `rev = 0` resolves `Ok` immediately.
  - Critical mutations are `set_home_dc_id`, `set_dc_option` (called by the runner when it creates a connection, `sender_pool.rs:394`, and on config refresh, `:518`), and `cache_peer` of the self user. `complete_login` caches the self user with `is_self: Some(true)` (`auth.rs:113-131`), and that makes the login critical.
  - These methods never block on the Keychain. They enqueue and return `Ok`.
- **Self-user sentinel:** `peer(PeerId::self_user())` (`G/grammers-session/src/peer.rs:135, 165`) returns the self user. `SqliteSession` does the same (`storages/sqlite.rs:339, 394`). The updates receiver uses it to choose channel diff limits (`updates_receiver.rs:60-70`).
- **In-memory peer map:**
  - Every peer of a loaded dialog is pinned (at most `MAX_DIALOGS = 1000`), along with the self user.
  - Other peers are kept only up to a bound of 2,000 unpinned peers, first in first out.
  - Writes merge with `PeerInfo::extend_info` (`peer.rs:357-373`, as `memory.rs:86-93` and `sqlite.rs:380-383` do). An incoming `None` hash, from Telegram's "min" constructors, never clears a known hash.
- **Untracked channels:** if a channel's access hash is missing, grammers ends that channel's difference as `Banned` (`updates_receiver.rs:50-80`, `message_box/mod.rs:875-893`) without telling the caller. Pinning makes this rare for chats in the list, and rebuild plus reconcile is the documented recovery.
- **Update state:** in memory only. `UpdateStream::sync_update_state` (`updates.rs:51-57`) is never called.

**Persistence worker.** One task owns the `SecretStore`, and it's the only Keychain caller.

```rust
enum PersistMsg { Write { gen: Gen, rev: u64, blob: Zeroizing<Vec<u8>> },
                  Barrier { gen: Gen, rev: u64, reply: oneshot::Sender<Result<(), PersistError>> },
                  Terminate { gen: Gen }, Delete { gen: Gen, reply: oneshot::Sender<Result<(), PersistError>> } }
```

- **Ordering.** Messages are processed strictly in order, one at a time. Each Keychain call runs in `spawn_blocking` and is awaited before the next message.
- **Writes.** `Write` is dropped if `gen` has been terminated or `rev <= last_written_rev`. Otherwise it's written, and its outcome is recorded against `rev`.
- **`Barrier{rev}`** is always sent with the cell's *current* `rev`, so it's satisfiable even when nothing new was written. It replies with the outcome of the write for that `rev`: either the recorded result, or, if the write is still queued, the result once it completes. It returns `Ok` only if that write succeeded and nothing for this generation has failed since. If `rev` hasn't advanced past the last processed write, it returns that write's outcome immediately.
- **Retiring a generation.** When an `AccountGen` ends at logout, the worker records it as retired. Every message tagged with a retired generation is a no-op, except the `Delete` that retired it.
- **`Terminate{gen}`** makes every later `Write` for that generation a no-op.
- **`Delete`** runs after everything queued before it, and its `Result` is returned to the caller.

**Login durability.** After `sign_in` / `check_password` succeeds, the actor sends `Barrier{rev}` and waits. On success it emits `SignedIn`. On failure it emits `SignedInUnsaved`, and the UI says: "Signed in, but the session couldn't be saved. You'll need to sign in again next launch."

**`SecretStore` implementations:** `KeychainStore`, `MemoryStore`, and a `FaultyStore` that injects delays and failures for tests.

**Tests:**
- A delayed write followed by logout leaves no item behind.
- Out-of-order revisions: an older revision never overwrites a newer one.
- An identical `set_dc_option`, as on reconnect, doesn't advance `rev`, and a Barrier taken right after it resolves with the previous outcome instead of hanging.
- A Barrier at startup with no writes since load resolves `Ok`.
- A failed write gives a `Barrier` error, which gives `SignedInUnsaved`.
- A failed delete makes the logout report a local failure.
- After 10k peer insertions, the self user and pinned peers are still present.
- An extend_info merge never loses a hash.
- No files are created under `~/Library` by any test flow.

## 4. Login

```rust
pub enum AuthState { MissingCredentials, Connecting,
  Phone { error: Option<UserError> },
  Code { phone_hint: String /* +44 •••• ••12 */, error: Option<UserError> },
  Password { hint: Option<String>, error: Option<UserError> },
  PasswordTokenUnavailable { error: UserError },          // must "Retry" before submitting
  FloodWait { until: Timestamp, resume: Box<AuthState> },
  SignUpRequired, SignedIn { display_name: String }, SignedInUnsaved { display_name: String },
  LoggingOut, LoggedOut { remote: RemoteLogout, local: LocalDeletion } }
```

**Flow:**
- `Connecting` checks `is_authorized()` (`auth.rs:105-111`), which leads to `Phone` or to `SignedIn`.
- `Phone` calls `request_login_code(phone, api_hash)` (`auth.rs:242`), which handles `303`, and moves to `Code`.
- `Code` calls `sign_in(&token, code)` (`auth.rs:339`):
  - `PasswordRequired(t)` goes to `Password` (`auth.rs:355-360`).
  - `InvalidCode` stays in `Code`, showing "Code invalid or expired" and a **Send new code** button (`ResendCode`). grammers folds `PHONE_CODE_EXPIRED` into `InvalidCode` (`auth.rs:362`).
  - `SignUpRequired` tells the user to create the account in an official app (`auth.rs:23-40`).
- `Password` calls `check_password(token, pw)` (`auth.rs:416`):
  - `InvalidPassword(t)` stays in `Password` with the new token.
  - Every other outcome **consumes the token**: `Other`, timeout, panic, or a stale `ClientGen`. The state becomes `PasswordTokenUnavailable`.
  - `RetryPasswordParams` refetches with a raw `invoke(account::GetPassword)` (`api.tl:2471`) and `PasswordToken::new(..)` (public, `auth.rs:70`), then returns to `Password`. Submitting is disabled until then.

**Pending flag.** An `AuthInFlight` guard sets the pending flag. Its `Drop` clears the flag on *every* termination path: success, error, timeout, panic or cancellation. So a stuck "submitting…" state is impossible.

**Ambiguous completion.** After a timeout or I/O error during `sign_in` / `check_password`, the actor calls `is_authorized()` before allowing a retry.
- If authorized, it fetches the self user with raw `users.getUsers([inputUserSelf])` (`api.tl:2579, 23`). It caches it as `is_self: Some(true)`, which is critical and persisted, then runs `Barrier` and moves to `SignedIn`.
- If not authorized, the state returns to `Code` or `PasswordTokenUnavailable`.

**Retries.** The pool uses `NoRetries`. Note that `G/grammers-client/src/client/retry_policy.rs` is dead code: it isn't a module in `client/mod.rs:11-21`. The pool default is `AutoSleep` (`configuration.rs:155`), which would silently sleep through auth flood waits. Our adapter retry classes:
- `Auth`: never retries.
- `Read`: flood waits of 5 s or less are slept, at most twice; an I/O error is retried once.
- `Send`: see section 9.

**Error mapping:**
- `RpcError{name:"FLOOD_WAIT", value}` (`G/grammers-mtsender/src/errors.rs:69-93`) becomes `FloodWait`, shown with a countdown.
- `PHONE_NUMBER_INVALID` and `PHONE_NUMBER_BANNED` map to typed errors.
- `AUTH_KEY_UNREGISTERED` / `SESSION_REVOKED` on any call run the local part of logout (section 5) and go to `Phone`.

**Tests:** flood wait on the code request, expired code then resend, `Other` on `check_password` then token-unavailable then refetch, timeout then the `is_authorized` probe in both branches, the pending flag clearing after a panic, and the login `Barrier` failing.

## 5. Logout and shutdown

**Logout sequence:**
1. Stop app admission and enter `LoggingOut`.
2. `Terminate{gen}` to the persistence worker.
3. With the runner **still alive**, call `client.sign_out()` (`auth.rs:489-491`, `auth.logOut`, `api.tl:2422`) with a 10 s deadline. The runner has to be running at this point: quitting it first makes the call fail with `Dropped` (`sender_pool.rs:154-159`).
4. `quit()`, then join the runner (5 s cap, then abort). Drop the client and the stream.
5. `Delete{gen}`, which runs after any queued write, and await its `Result`.
6. Emit `LoggedOut { remote, local }`.
   - `remote` is `Revoked`, `Failed(reason)` or `TimedOut`.
   - `local` is `Deleted` or `Failed(err)`.
   - The UI reports the two separately. If `local` failed, it shows **Logout incomplete** with a retry, and never "logged out". If `remote` failed, it tells the user to end the session from Active Sessions on another device.
7. The next login uses a fresh `AbyssSession` and a new `gen`.

**Shutdown.** GPUI waits at most 200 ms for `on_app_quit` futures (`SHUTDOWN_TIMEOUT`, `Z/src/app.rs:78-80`), so shutdown starts earlier:
- Window close is intercepted by `window.on_window_should_close` (`Z/src/window.rs:7352`). It returns `false` while core is running.
- Cmd+Q is bound to our own `Quit` action, not GPUI's default.
- Both call `core.begin_shutdown()`, show "Closing…" and await the ack in `cx.spawn`.

Core shutdown runs on the control plane, so a full command queue can't block it:
1. Stop admission. Reject queued commands with `ShuttingDown`.
2. Cancel the token, which ends the actor tasks and the update consumer.
3. **Retire the client instance** with the same steps as a rebuild (section 2, rebuild steps 2–4), using caps of 1 s for tasks and 2 s for the runner. The runner gets an abort, then an await, then the session's write token is revoked. After this point no critical writer is left: only runner methods write DC options (`sender_pool.rs:394, 472-518`).
4. Only then, **seal** the persistence worker by sending `Barrier{current rev}`, and wait for it with a 3 s deadline counted from the start of shutdown. If retirement or the barrier doesn't finish within the deadline, the status is `Unconfirmed`.
5. If a logout is in progress, its remaining steps continue within that same deadline.
6. Ack with `SaveStatus::{Confirmed, Unconfirmed}` and `LogoutStatus` if relevant. Then return from `block_on`. The thread then calls `rt.shutdown_timeout(1s)`.

**Guarantees:**
- We never claim a save we didn't observe. `Unconfirmed` makes the UI say "Couldn't confirm the session was saved; you may need to sign in again", with a Quit button.
- An interrupted logout reports **Logout incomplete — the session key may remain in the Keychain**.
- Critical state is written when it changes, so a normal shutdown has nothing extra to save.
- A `spawn_blocking` Keychain call that outlives the deadline isn't aborted. The process exits around it, and the Keychain update is either applied or not.
- The UI enforces its own 6 s timeout, then calls `cx.quit()`.
- `cx.on_app_quit` (`Z/src/app.rs:2950`) only cancels the token, as a backstop for OS-initiated termination.

**Tests:**
- Shutdown with a full command queue.
- A slow write that finishes within the deadline gives `Confirmed`.
- A write that runs past the deadline gives `Unconfirmed`.
- Shutdown during logout, before and after the remote call.
- Shutdown during a rebuild.
- Logout when `auth.logOut` times out still deletes the item and reports `remote=TimedOut, local=Deleted`.

## 6. Domain model and formatting budget

```rust
pub struct ChatId { kind: PeerKind, id: i64 }                 // access hashes stay inside core
pub struct ThreadKey { chat: ChatId, topic: Option<i32> }     // topic is always None in Stage 1; Stage 2 fills it
pub enum ChatKind { Private, Bot, Group, Supergroup { forum: bool, gigagroup: bool } }
pub struct ChatSummary { id: ChatId, title: String, kind: ChatKind, pinned: bool, unread: u32,
                         unread_mentions: u32, last: Option<Preview>, last_date: i64, can_send: bool }
pub struct Message { id: MsgId, thread: ThreadKey, sender: Option<Sender>, date: i64, edited: bool,
                     outgoing: bool, reply_to: Option<MsgId>, body: Body }
pub enum Body { Text(RichText), Media { kind: MediaKind, caption: RichText }, Service(String), Unsupported }
pub struct RichText { text: String, segments: Vec<Segment>, links: Vec<String> } // segments partition text exactly
pub struct Segment { len: u32, style: StyleFlags, link: Option<u8> }
pub struct StyleFlags(u8); // BOLD|ITALIC|UNDERLINE|STRIKE|CODE|SPOILER
pub enum MediaKind { Photo, Video, Voice, Audio, Document { name: String, size: u64 }, Sticker, Gif,
                     Poll, Location, Contact, Other }
```

**Mapping split.** `GrammersBackend` calls `extract(&grammers::Message) -> RawParts`, a thin accessor wrapper over `G/grammers-client/src/message/message.rs:145-628`: `text`, `fmt_entities`, `media`, `sender`, `date`, `edit_date`, `outgoing`, `reply_to_message_id`, `action`. Then a pure `map(RawParts) -> Message` runs. `grammers::Message` needs a live `Client` to construct, so the tests cover `map` with hand-built `tl::types` values.

**Formatting budget.** These limits are checked *before* normalization. If any is exceeded, the message renders as **plain text**: one segment, no links.
- Text up to 16 KiB. Longer text is clamped down to a char boundary, and the formatting is dropped.
- At most 256 entities.
- At most 64 link entities, and at most 8 KiB of URL bytes in total. That total counts **both** the hidden targets of `TextUrl` entities and the visible text covered by `Url` entities, because the visible text is the URL that gets shown, tooltipped and opened.

**Normalization.** `StyledText::with_runs` panics unless the runs exactly cover the text on UTF-8 boundaries (`Z/src/elements/text.rs:502-512`). We normalize in core, so no panic path on the UI thread can be reached from server data:
1. Build a UTF-16-to-byte offset table with one pass over `char_indices`. Map each entity's start and end with checked arithmetic. An entity whose start or end doesn't land exactly on a UTF-16 char boundary (for example, inside a surrogate pair), falls outside the text, or overflows is **dropped**, never clamped. Empty and inverted spans are dropped too.
2. Take all start and end points plus `0` and `len`, then sort and deduplicate. That's at most 514 points.
3. For each gap, OR together the style flags of every span covering it. The innermost (shortest) link wins. That's at most 513 × 256 checks.
4. Merge adjacent segments that have the same style and link.

**Proptest:** for arbitrary text and entities, `sum(len) == text.len()`, every boundary is a char boundary, entities with an offset inside a surrogate pair are absent from the output, the budget fallback triggers exactly at the limits (counting visible `Url` bytes), and a headless GPUI test calling `with_runs` on the output never panics.

**Sanitizing.** Titles, sender names and document names have bidi controls (U+202A–U+202E, U+2066–U+2069) and other C0/C1 control characters stripped.

**Byte budgets.** The store's retained bytes are capped at 64 MiB in total, with LRU chat eviction above that. Event payloads queued for the UI are capped at 16 MiB through a byte-counting semaphore, released when the UI consumes the batch. That budget is separate from the 1024-event count bound.

## 7. Chat list

- **Source:** `iter_dialogs()` (`G/grammers-client/src/client/dialogs.rs:158`), 100 per page and more on scroll, up to `MAX_DIALOGS = 1000`.
- **Kinds:**
  - `Peer::User` and `Peer::Group` are shown. grammers puts megagroups and gigagroups in `Group`, and only `broadcast` channels in `Peer::Channel` (`G/grammers-client/src/peer/mod.rs:82-92`).
  - **`Peer::Channel` dialogs are excluded**, and only counted in a debug log.
  - Raw `Dialog::Folder` / `Dialog::Community` entries are skipped.
  - Gigagroups (`ChannelKind::Gigagroup`, `G/grammers-session/src/peer.rs:116-122`) are shown with `can_send = false`.
- **Order:** server order. A new message moves its chat to the top of the unpinned section. Pinned chats keep server order (`raw.pinned`).
- **Unread counts:**
  - Initial values come from raw `tl::types::Dialog.unread_count` / `unread_mentions_count`.
  - An incoming message increments the count unless the chat is open, the window has focus and the list is following its tail.
  - **Mark read:** grammers' `mark_as_read` hardcodes `max_id: 0` (`dialogs.rs:226-240`). We call the raw `messages.readHistory(peer, max_id)` / `channels.readHistory(channel, max_id)` (`api.tl:2622, 2933`) with the **highest message id actually displayed**, coalesced. `InputChannel` comes from the `PeerRef` conversions (`peer.rs:781-797`).
  - Reads on other devices arrive as `Update::Raw` `updateReadHistoryInbox` / `updateReadChannelInbox` (`api.tl:349, 356`), which set `unread = still_unread_count`.
- **Row:** fixed height, with an initials avatar (color from a hashed id, no image), the title, a 100-character preview (media shown as `[Photo]` etc.), the time and an unread badge.
- **Rendering:** `uniform_list` (`Z/examples/uniform_list.rs`).

## 8. Message view

- **List:**
  - `ListState::new(n, ListAlignment::Bottom, px(800.))` with `set_follow_mode(FollowMode::Tail)` (`Z/src/elements/list.rs:99-105, 314, 605`).
  - **Older history:** a `set_scroll_handler` (`list.rs:542`) sends `LoadOlder` when the first visible index is below 10, nothing is loading and `has_more` is set. The call is `iter_messages(peer).offset_id(oldest).limit(50)` (`G/grammers-client/src/client/messages.rs:284-318, 1081`).
  - **Prepending:** `splice(0..0, k)` (`list.rs:490`), which shifts the scroll anchor itself (`list.rs:524-537`). There is no manual scroll restore.
- **Bounds:**
  - At most 2,000 messages per chat window. At the cap, older loading stops and the list shows "history limit for this view". New messages past the cap drop the oldest.
  - Windows are kept only for the 8 most recently opened chats.
  - `ChatInvalidated` clears a window and refetches its tail if it's open.
- **Rows:**
  - The sender name appears only in groups, colored by hashed id.
  - The body is rendered from `Segment`s mapped 1:1 to `TextRun`s (`StyledText::with_runs`, `text.rs:502`) and wrapped in `InteractiveText` (`text.rs:1013-1060`). Code and pre text use a monospace font. Spoilers render dimmed and reveal on click.
  - **Links:** the tooltip shows the **real** target URL. Clicking opens `window.prompt` (`Z/src/window.rs:7149`) with the full URL and Open / Copy / Cancel. Open calls `cx.open_url` (`Z/src/app.rs:2050`) only for `http`/`https`. Every other scheme is copy-only.
  - Each row also shows local time (`jiff`, which grammers already uses), day separators and an "edited" mark. Outgoing messages are right-aligned. A reply shows one line quoting the target if it's cached.
- **Media:** an inert chip showing kind, sanitized name and size, plus the caption. No download API exists in `Backend`, and `FakeBackend` asserts that none is reached.
- **Live updates** (`G/grammers-client/src/update/update.rs:23-48`):
  - `NewMessage` appends to the open window and bumps the chat list.
  - `MessageEdited` replaces the message.
  - `MessageDeleted` uses `channel_id()` when it's set (`update/message_deletion.rs:21-39`). Otherwise it looks up the store's `msg_id → ChatId` index; ids are unique per account in private and basic-group chats.
  - Other updates are counted at debug level and ignored.

## 9. Sending

Messages carry no `random_id` (`api.tl:128`). The only correlation is `updateMessageID{id, random_id}` (`api.tl:330`). `Client::send_message` draws a new id on every call (`messages.rs:593-640`), so the adapter sends raw `messages.sendMessage` (`api.tl:2627`) instead.

- **One id per send.** `random_id` is a nonzero `i64` from `getrandom`, drawn **once per logical send** and stored with its `LocalId` in the pending-send table, which survives rebuilds. We never draw a fresh `random_id` for an unresolved send.
- **Request fields:** `no_webpage: true`. `peer` is converted from `&PeerRef` (`G/grammers-session/src/peer.rs:797`).
- **Correlation.** Record `random_id → id` from:
  - the RPC response: `updateShortSentMessage` (`api.tl:519`) gives the id directly; for `updates` / `updatesCombined` (`api.tl:517-518`), take the `updateMessageID` entries;
  - `updateMessageID` arriving on the update stream as `Update::Raw`.
- **Ownership.** Outcomes belong to the **account-level pending-send table**, keyed by `LocalId`, holding at most 256 entries. If the table is full, the composer refuses the send.
  - Entries hold `{ chat: ChatId, peer: PeerRef, random_id, text, state, attempts }`.
  - They are **not** tied to `ViewGen` or `ClientGen`. Closing the chat, evicting its window or rebuilding the client never cancels or drops a send. `ViewGen` only decides whether a chat window currently *shows* the echo.
  - Entries leave the table on `Sent`, on a user discard, or when a `SentUnconfirmed` echo is cleared by reconcile.
- **States:**

  ```rust
  enum SendState { Sending { attempts: u8 }, Sent { id: MsgId }, SentUnconfirmed,
                   Unknown { attempts: u8 }, Failed { reason: UserError } }
  ```

  - **`Sent{id}`** comes from the response or a stream `updateMessageID`. The echo takes the server id, and an incoming `NewMessage` with that id replaces it instead of appending.
  - **`Failed`** comes from a definite rejection: an RPC error other than flood or 303. Retry is manual only, and reuses the **same** `random_id`.
  - **Automatic retries.** A timeout, an I/O error, or `Dropped` from a rebuild is an unknown outcome. The send stays `Sending{attempts+1}` and is resent with the same `random_id` while `attempts < 3` (the cap counts across rebuilds). **`Unknown{attempts}`** is the explicit state after the attempts run out: we don't know whether the server accepted it. The user can Retry (same `random_id`, attempts reset) or Discard.
  - **`SentUnconfirmed`** means the server accepted the send but we have no id mapping: `RANDOM_ID_DUPLICATE`, or a response without `updateMessageID`. It's distinct from `Unknown`, which means we don't know the outcome at all. The echo shows a "sent, unconfirmed" marker and is **never** matched to history by text. It's dropped when that chat is next reconciled, and the server's copy then appears from history.
  - **Replay after `Online`.** Only `Sending` entries and `Unknown` entries under the cap are replayed, both with the same `random_id`. `Sent`, `SentUnconfirmed` and `Failed` are never replayed automatically.
- **Tests:**
  - Two identical texts sent back to back keep distinct `random_id`s and resolve independently.
  - With the response's mapping lost, an unknown outcome is resent with the same id and ends `SentUnconfirmed` without a duplicate echo.
  - A rejected send is retried with the same id.
  - A rebuild in the middle of a send keeps the id.
  - Closing the chat before the acknowledgement arrives still records `Sent{id}`, and reopening shows the server message once.
  - Exhausting the attempts across two rebuilds gives `Unknown{3}`, which isn't replayed again until the user retries.
  - The echo and the `NewMessage` merge in either order.

## 10. Text input

GPUI has no text-field widget at this commit. `Z/examples/input.rs` (912 lines) is a complete single-line reference, with `EntityInputHandler` IME support (`input.rs:288-475`) and UTF-16 mapping (`input.rs:222+`). We vendor it as `app/src/ui/text_input.rs`, keeping the Apache-2.0 header and NOTICE.

- **Normal fields** (phone, code): single-line with a max length. Phone accepts digits and `+` only.
- **Composer:**
  - Multi-line with soft wrap via `TextLayout` / `WrappedLineLayout` (`Z/src/elements/text.rs:388, 899-922`).
  - Enter sends; Shift+Enter inserts a newline.
  - It grows to 8 lines, then scrolls.
  - Length is capped at 4,096 UTF-16 units, with a counter.
- **Password field** (`PasswordInput`): a separate element, not the vendored editor. That editor builds plaintext temporaries (`input.rs:362`, `to_owned() + new_text`).
  - **Buffer:** `PasswordBuf`, a preallocated `Box<[u8; 256]>` plus `len`. It never reallocates. Inserting shifts bytes in place with `copy_within`, and only whole UTF-8 chars are inserted. An insert that would overflow is rejected. The buffer is zeroized on submit, on cancel and on `Drop`.
  - It's never a `SharedString` or a `String`. `check_password` takes `impl AsRef<[u8]>` (`auth.rs:416-420`), so the buffer is passed as a slice. Copies grammers makes internally during SRP are a residual risk (risk 9).
  - **Display:** a bullet string with one `•` per char. `•` is a single UTF-16 unit, which makes display char `i` equal to secret char `i`.
  - **Leak prevention:** `text_for_range` and `selected_text_range` return bullets only, so IME, text services and accessibility never see plaintext. Copy and cut are disabled. Paste is allowed, for password managers.
  - **Marked text:** supported, for dead keys. The marked chars live in `PasswordBuf` and display as bullets.
  - **Secure input:** a `SecureInputGuard` (RAII) calls `EnableSecureEventInput` on focus and `DisableSecureEventInput` in `Drop`, through a small Carbon FFI module.
  - **Tests:** non-ASCII text (`é`, `ß`, an emoji, a ZWJ sequence). A dead-key sequence (Option-e, then e) goes through `replace_and_mark_text_in_range` then `replace_text_in_range` and yields `é`. IME composition and cancellation. Capacity overflow. `text_for_range` never returns a non-bullet. Zeroization on drop (checked through a test-only view of the buffer).

## 11. Logging

- `tracing` + `tracing-subscriber` with `EnvFilter`. The default is `warn,abyss=info,abyss_core=info`. Output goes to stderr only, with no log files.
- **All `grammers*` events are dropped** by a per-layer `filter_fn` on the target prefix, with no allowlist. The reason: grammers' send-failure path clones and logs the whole `Updates` at WARN (`G/grammers-client/src/client/messages.rs:664-672`).
  - In debug builds, `ABYSS_DEBUG_PROTOCOL=1` lifts the filter.
  - Follow-up: an upstream patch moving payload dumps to TRACE.
- **Our own sanitized events** cover RPC error *names* and codes, durations, ids, `ClientGen`/`ConnState` changes, rebuild causes, and persistence outcomes.
- A `Redacted<T>` wrapper prints `<redacted>`. Phone numbers, codes, passwords, message text, titles and usernames are never logged.
- **Tests:**
  - A synthetic WARN event with target `grammers_client::client::messages` carrying a marker never reaches the output.
  - A full fake session at `info` contains neither the fake phone number nor any message text.

## 12. Build, features, testing, CI

- **Features:**
  - `abyss-core`: `fake = []`.
  - `abyss`: `fake = ["abyss-core/fake"]` and `runtime-shaders = ["gpui_platform/runtime_shaders"]` (default).
  - Nothing toggles by profile.
- **Aliases** in `.cargo/config.toml`:
  - `cargo dev` = `run -p abyss --features fake -- --fake`
  - `cargo t` = `test --workspace --features fake`
- **Production build:** `cargo build --release -p abyss --no-default-features`. It has precompiled shaders (which needs the Metal toolchain) and no fake backend. CI checks that `fake` is absent with `cargo tree -e features`.
- **Credentials:** `ABYSS_API_ID` / `ABYSS_API_HASH` are read at runtime only. If they're missing, the app shows a full-window error screen naming them. `--fake` skips credentials.
- **GPUI tests:** the app's dev-dependency is `gpui = { workspace = true, features = ["test-support"] }`, which provides `TestAppContext` (`Z/src/app.rs:69-75`).
- **Lints:**
  - `abyss-core` non-test code denies `clippy::unwrap_used`, `expect_used`, `panic` and `indexing_slicing`.
  - `clippy.toml` sets `too-many-lines-threshold = 70`.
  - CI runs `-D warnings`.
- **Contract suite:** `contract::run(impl Backend)` runs against `FakeBackend` in CI, and against `GrammersBackend` under `#[ignore]` once credentials exist.
- **CI:**
  - Runner `macos-15`. `rust-toolchain.toml` pins `1.98.1`, matching Zed at this commit.
  - Steps: `fmt --check`, `clippy --workspace --all-targets --features fake -- -D warnings`, `test --workspace --features fake`, then the production-build job.

## 13. Ordered tasks and acceptance criteria

1. **Repo hygiene.** Add the toolchain pin, lints, aliases, CI, LICENSE (Apache-2.0) and NOTICE, and `git init` once the repo location is chosen. *Done when:* CI is green and both feature configurations build.
2. **Domain model, normalization, store** (pure code). *Done when:* the formatting proptests, the headless `with_runs` test, and the bound, LRU and byte-budget tests pass.
3. **Core skeleton:** core thread, actor (admission, generations, parked permits), control plane, `FakeBackend` with fault injection. *Done when:* the admission, stale-completion and panic-containment tests pass, and `cargo tree -p abyss-core` shows no gpui.
4. **Client lifecycle:** build, rebuild and reconcile. *Done when:* every fault-injection test in section 2 passes, and the parked permits are released by rebuild.
5. **Persistence:** `AbyssSession`, worker, `SecretStore`s. *Done when:* every section 3 test passes.
6. **Login, logout, shutdown** state machines. *Done when:* every section 4 and section 5 test passes.
7. **App bridge:** auth router, missing-credentials screen (superseded by onboarding, section 17 / task 6b), `Quit` action, the should-close hook. *Done when:* `cargo dev` goes from login to the chat list, and closing shows "Closing…" then exits, or shows the unconfirmed-save message when `FaultyStore` stalls.
8. **Text input:** normal, composer and password. *Done when:* the section 10 tests pass, and manual Pinyin/Japanese IME testing works in the composer.
9. **Chat list.** *Done when:* 1,000 fake dialogs scroll without dropped frames on Apple Silicon, live reorder works, read state from another device (fake) applies, and the displayed max id reaches read-history.
10. **Message view and sending.** *Done when:* a 2,000-message fake chat (emoji, RTL, 4,096-character text, every entity and media kind, over-budget messages) renders; prepending shows no visible jump; links open only after confirmation; edits and deletes apply live; the section 9 tests pass.
11. **`GrammersBackend`:** mapping, raw read/send/getPassword/getUsers, retry classes, 303 handling, log suppression. *Done when:* it builds in the production configuration, and the contract suite and log tests pass.
12. **Live verification** (blocked on credentials). First Telegram's test DC, then a real account. *Done when:*
    - login with and without 2FA works;
    - the session survives `kill -9`;
    - send and receive work;
    - the app recovers from sleep and wake;
    - a remote session revocation wipes the local session;
    - after logout, `security find-generic-password -s dev.abyss.session` fails.

**Task 3 status (2026-11-16): done.** Implemented the core thread, the actor (admission, parked permits, `ClientGen`/`ViewGen` staleness, single-flight history and dialogs, coalesced read marks, sends with retries), `FakeBackend` with fault injection, and the cache worker (section 16), with the cache wired into OpenChat, scrolling and live updates. Verified: fmt, clippy in both feature configs, 77 core tests, and no gpui in `cargo tree -p abyss-core`.

Deviations:
- `BackendChoice::Fake` takes a `FakeBackend`, not a `FakeScript`, so tests keep the `FakeHandle`.
- `Event::NewMessage` was removed. Live messages that land in a window are reported as `History{dir: Newer, has_more: false}`, which carries the `removed` ids when the window trims.
- With a cache, the server tail is requested after the cache tail is read, so the overlap check always sees what the cache held. When the tail does not overlap the cache (a gap), the chat's cache is replaced by the server page.

Open for task 4:
- Nothing sends `Internal::Retire` yet. `UpdatesEnded` only emits `Connecting` plus `Error(Network)`.
- A Failed send stays in the 256-entry table until a retry succeeds, and there is no discard command.
- Cancelling shutdown does not interrupt an `emit` that is blocked on a full event budget. It is unblocked once the UI drops its receiver.
- No auth or logout handling: those commands return `Unsupported`.

**Task 5 status (2026-11-16): done, not wired.** Implemented `crates/core/src/session/` (`AbyssSession`, `SessionSnapshot`, `PersistHandle`, the persistence worker) and `crates/core/src/secret/` (`SecretStore`, `KeychainStore`, `MemoryStore`, `FaultyStore`, cache-key provisioning). Nothing in the actor, `core.rs` or the backend uses it yet. Verified: fmt, clippy in both feature configs, 108 core tests (22 session, 7 secret) plus 2 ignored real-Keychain tests, which were run once locally and pass.

What exists:
- `AbyssSession: grammers_session::Session`. Critical mutations (`set_home_dc_id`, `set_dc_option`, `cache_peer` of the self user) encode the state under the cell lock into a zeroizing buffer preallocated to 8 KiB and compare it with `last_blob`. Only a change bumps `rev` and queues `Write{account, rev, blob}`, with a non-blocking `try_send`. Peers live in a bounded in-memory map: up to `MAX_PINNED_PEERS` (1,264) pinned, 2,000 unpinned FIFO, merged with `extend_info`. The self user lives only in the critical state, so it is never evicted, and both `PeerId::self_user()` and its real id resolve to it. Update state is in memory. `baseline_established()` is a `watch` that flips on the first `UpdateState::All`.
- Blob: postcard, a `u16` version (1), then home DC, up to 16 DC options (BTreeMap order, so encoding is deterministic) and an optional `SelfUser`. Auth keys are held in `Zeroizing<[u8; 256]>` and borrowed during encode and decode. An unknown version, trailing bytes, an oversized blob, duplicate DC ids, a bad key length or an out-of-range self id all decode as logged out.
- The worker is a dedicated OS thread (`abyss-persist`), the only `SecretStore` caller after `spawn`. It processes messages strictly in order. A `Write` is dropped when its generation is terminated or older than the current one, or when `rev <= written`. `Barrier{rev}` returns the outcome of the newest write covering `rev`, at once when nothing changed. It returns `NotQueued` if that write never reached the store, and `Terminated` for a retired generation. `Terminate` and `Delete` retire every generation up to the given one. `Delete` returns the store `Result` and is idempotent.
- `KeychainStore`: generic password items, services `dev.abyss.session` and `dev.abyss.cachekey`, account `abyss`. Every item is written with `kSecAttrSynchronizable = false` (also used in queries) and `kSecAttrAccessible = WhenUnlockedThisDeviceOnly`, plus a label. Writes try add, then update in place on `errSecDuplicateItem`, never delete-then-add. Reads are copied into `Zeroizing`. The doc comment in `secret/keychain.rs` justifies each attribute.
- Cache key: `load_or_create_cache_key` (getrandom, returned only after the store accepted it), `create_cache_key` (replace), `delete_cache_key`. A stored key that is not 32 bytes gives `CacheKeyError::Malformed`, which is distinct from `Store(..)`.

Deviations:
- **Login keychain, not the data protection keychain.** The accessibility class is enforced only by the data protection keychain, and that keychain returns `errSecMissingEntitlement` (-34018) for unsigned or ad-hoc binaries (verified by the ignored test `keychain_data_protection_attributes`). Stage 1 therefore uses `KeychainLocation::File` (the login keychain). There, the items are never synced (the file keychain cannot sync), but `WhenUnlockedThisDeviceOnly` is set and ignored; the login keychain's own lock applies instead. Stage 3 (signed, with `keychain-access-groups`) switches `KeychainStore::new()` to `KeychainLocation::DataProtection`. The attributes are already written, and the ignored test then asserts `akpu` and non-synchronizable. Add this to risk 3 when signing lands.
- **Write tokens are internal.** `AbyssSession::from_snapshot` takes the token and so revokes the previous instance. `revoke()` is available for rebuild step 4. `snapshot()` does not revoke.
- **No `Durability` field in the cell.** The worker is the single source of truth, and `barrier()` returns it. The cell records only a `Write` that could not be queued (64-slot queue, never blocked on), which makes the next barrier return `NotQueued` until a later write is queued.
- **The carried set is the pinned set.** `snapshot()` carries every pinned peer (bounded by `MAX_PINNED_PEERS`), not only the open chats and pending sends. A superset, so bootstrap keeps channel hashes before dialogs reload. `AbyssSession::info_from_ref` converts extra carried `PeerRef`s, which are then pinned with `pin_peers`.
- **The Keychain is read through the worker, also at startup** (`load_session`, `load_or_create_cache_key`), so the store has exactly one owner from `spawn` on.
- **Unknown or corrupt items are not deleted at load.** They count as logged out. The next login overwrites the item, and a logout deletes it.
- A `cache_key_deleter()` called on an async runtime thread returns `InAsyncContext` instead of blocking. It is meant for the cache worker thread.

**Wiring for task 6** (all from `abyss_core::session` and `abyss_core::secret`):
1. **Startup** (core thread, inside the runtime): `let persist = PersistHandle::spawn(KeychainStore::new())?` (`FaultyStore::new(MemoryStore::new())` under `--fake`). Then `let LoadedSession { snapshot, status } = persist.load_session().await?`. A `StoreError` here means the Keychain is unreadable: treat it as logged out but surface a warning. `snapshot.is_signed_in()` decides between `Connecting` and `Phone`. Keep `snapshot` in the actor as the account's live critical state (`AccountGen` = `snapshot.generation()`).
2. **Cache key:** `persist.load_or_create_cache_key().await`. On `Malformed`, call `cache::destroy_unopened(dir)`, then `persist.create_cache_key().await`. On `Store(_)`, run this session without a cache and don't destroy anything. Then `CacheHandle::spawn(dir, &key)`.
3. **Build** (section 2): `let session = Arc::new(AbyssSession::from_snapshot(snapshot.clone())?)`. Pass it to `SenderPool::with_configuration`. During bootstrap, wait on `session.baseline_established()` (`wait_for(|b| *b)` with the 10 s cap). After each dialogs page, call `session.replace_pinned_peers(infos)` on `DialogsReset` and `pin_peers(infos)` on later pages.
4. **Rebuild:** after the runner join (step 3), call `old.revoke()` (step 4), then `let snapshot = old.snapshot()` (step 5; add carried pending-send peers with `AbyssSession::info_from_ref` and `pin_peers` on the new instance), then `AbyssSession::from_snapshot(snapshot)`. Never call `load_session` again.
5. **Login durability:** after `sign_in`/`check_password` (or the ambiguous-completion `users.getUsers` + `cache_peer(is_self: Some(true))`), `session.barrier().await`: `Ok` gives `SignedIn`, `Err(_)` gives `SignedInUnsaved`.
6. **Logout:** take `account = snapshot.generation()?`. Then `persist.terminate(account).await` (step 2). Run `sign_out` with the runner alive, then quit and join it. Then `persist.delete_session(account).await` gives `local` (`Ok` → `Deleted`, `Err(e)` → `Failed(e)`; a retry may call it again). Then `cache.erase(key, persist.cache_key_deleter()).await`. Finally replace the actor's snapshot with `persist.fresh_session()` (a new generation) for the next login.
7. **Shutdown:** after retirement (`revoke()`), `snapshot.barrier()` under the 3 s deadline: `Ok` gives `SaveStatus::Confirmed`; an error or the timeout gives `Unconfirmed`.
8. `AUTH_KEY_UNREGISTERED`/`SESSION_REVOKED` run step 6 without the remote call.

Open:
- `PersistError` and `StoreError` carry OS status text only. Map them to `UserError` in task 6, and never log secrets or the blob.
- Security.framework's internal `CFData` copies of secrets are released, not wiped (documented residual).
- The section 3 test "no files under `~/Library`" holds trivially (every session test uses `MemoryStore`). An explicit filesystem check belongs with the task 6 integration tests.

**Task 8 status (2026-11-16): done, pending manual IME testing.** `crates/app/src/ui/text_input.rs` vendors `Z/examples/input.rs` with an Apache-2.0 header and modification notice (NOTICE already lists it). The editing state lives in a GPUI-free `EditBuffer` (`text_input/edit.rs`): character filters (phone: ASCII digits and `+`; code: digits), UTF-16 length caps that never split a grapheme cluster, a line policy (single-line turns breaks into spaces, the composer normalizes CRLF), and marked-text handling. `text_input/element.rs` shapes through `shape_text` with soft wrap; the composer measures its height with `request_measured_layout`, grows to 8 rows, then scrolls to keep the caret visible (wheel scrolling too), and shows a `n / 4096` counter from 3,596 UTF-16 units. Enter emits `Submit`, Shift+Enter inserts a newline (single-line fields ignore it), Up/Down move by visual row. Key bindings are scoped to the `TextInput` key context. `PasswordInput` (`ui/password.rs`) is a separate element over `SecretBuf`, a `Box<[u8; 256]>` edited in place with `copy_within`, rejecting whole inserts that overflow, zeroing the stale tail on shrink, and wiping on submit (`take` hands core an exact-capacity `Zeroizing<Vec<u8>>`), Escape, leaving the password screen, shutdown and drop. Positions are chars, so the masked-layout byte offset is `3 × char` and the platform UTF-16 offset is the char index; `text_for_range` returns bullets only; copy and cut are not handled; paste is (the clipboard temporary is zeroized). `SecureInputGuard` wraps Carbon `EnableSecureEventInput`/`DisableSecureEventInput` and is held only while the field is focused *and* the window is active. Tests: 34 in `ui/` (mapping with é/ß/emoji/ZWJ, caps, filters, line policy, dead key and IME composition/cancel, no reallocation, stale-tail zeroing, wipe on take/clear/drop via a test-only drop hook, bullets-only platform APIs, secure-input pairing across focus/blur/deactivation/drop, composer key handling and height cap through `TestAppContext`).

Deviations:
- One bullet per **char**, not per grapheme, as the plan says; a ZWJ emoji shows several bullets and Backspace removes one char at a time.
- Secure Event Input is also released while the window is inactive (the plan only said focus), because holding it in the background blocks keyboard monitoring system-wide.
- Paste into the password field removes control characters; IME candidate geometry (`bounds_for_range`) is approximate on wrapped rows.

Needs manual testing: Pinyin and Japanese IME in the composer (candidate window position, Enter committing a composition rather than submitting), dead keys in the password field with Secure Event Input on, and VoiceOver not reading the password.

**Task 7 status (2026-11-16): done against the fake, with deviations.** `crates/app/src/shell/`:
- **Bridge** (`bridge.rs`): one `spawn_in` loop awaits the next event, drains up to 256 with `try_recv`, and applies them in one `Root` update. The UI talks to core through a small `CoreLink` trait implemented by `CoreHandle` (and by a same-thread double in tests, because GPUI's test scheduler rejects wakeups from core's threads as non-determinism). A full queue shows a "busy" toast.
- **Auth router** (`view.rs::auth_screen`): a single `match` on the auth state with one arm per state, each delegating to its own screen function (`connecting_screen`, `phone_screen`, `code_screen`, `password_screen`, `flood_wait_screen`, `chat_shell`). New states such as `NeedCredentials { error }`, `Welcome` or the credentials steps (section 17, task 6b) add a variant, an arm and a screen function without restructuring. Core's `AuthState` has only `SignedIn`, so the router matches a UI-side `AuthView` that mirrors section 4 and maps core states in with `From`.
- **Copy**: all user-facing strings for the startup, auth and shutdown screens live in `crates/app/src/ui/copy.rs` (chat-shell placeholders stay inline until tasks 9–10).
- **Missing credentials**: without `ABYSS_API_ID`/`ABYSS_API_HASH` and without `--fake`, a minimal full-window screen names the unset variables (onboarding replaces it in task 6b). An invalid id, `--fake` on a build without the feature, and `StartError::NotImplemented` (credentials set but no grammers backend yet) get their own screens. No panic; the process stays up.
- **Shutdown**: our own `Quit` action is bound to Cmd+Q and Cmd+W and is the app menu's Quit; the window's should-close hook returns `false` while core runs. Both call `begin_shutdown`, show "Closing…", and await the ack against a 6 s UI timer: `Confirmed` or timeout quits, `Unconfirmed` (or a lost ack) shows the unconfirmed-save message with a Quit button. `on_app_quit` only cancels core.
- **Logging**: `tracing` + `tracing-subscriber` to stderr, default `warn,abyss=info,abyss_core=info` (`RUST_LOG` overrides), and a per-layer filter drops every `grammers*` target regardless of `RUST_LOG` (lifted only by `ABYSS_DEBUG_PROTOCOL=1` in debug builds). The startup `eprintln` is gone.
- Tests: 21 in `shell/`, `launch.rs` and `logging.rs` (credential resolution, grammers suppression, the 256 drain cap, fake login → chat list → `SendText`, busy toast, close → Closing → Unconfirmed → close allowed, the UI timeout, error screen closes freely, chat list bounds and ordering, phone masking, shutdown outcome mapping). Smoke-run: `abyss --fake` and `abyss` without credentials both stay up.

Deviations:
- **Login under `--fake` is simulated in the UI** (`auth::demo`): the fake backend has no auth and core answers login commands with `Unsupported`. Any phone with 5+ digits, any 5+ digit code (`00000` shows a 10 s flood-wait countdown), any non-empty password. Without `--fake`, submissions go to core as `SubmitPhone`/`SubmitCode`/`SubmitPassword`.
- The `FaultyStore` stall can't be triggered from the app (core exposes no fault hook through `start`), so the unconfirmed-save path is covered by the GPUI test with a core double, not by `cargo dev`.
- Cmd+W also quits (single-window app); after the window closes on the error or unconfirmed screen, the app quits.

Core API gaps (for the core tasks):
- `CoreHandle` has no way to split the event receiver from the command sender, so the UI shares it in an `Rc<RefCell<_>>` and polls a fresh `recv()` future per wake. A `split()` or `poll_recv` would remove that.
- `AuthState` needs the section 4 states (and `NeedCredentials`/`Welcome` for task 6b); the UI's `AuthView` should then collapse into it.
- No way to inject `FakeBackend` faults (or a `FaultyStore`) through `--fake` for the manual shutdown check.
- `PasswordBuf` is still `Zeroizing<Vec<u8>>`; the UI hands over an exact-capacity copy.

Needs manual testing: `cargo dev` login → chat list; Cmd+Q, Cmd+W and the close button showing "Closing…" then exiting (keystrokes could not be automated here without Accessibility permission).

**Task 4 status (2026-11-16): done.** Implemented the client lifecycle in `crates/core/src/actor/{lifecycle,boot,merge}.rs`:
- **Backend as an instance factory.** `Backend::build(Seed) -> Instance { client, updates, runner, control }`, with per-instance `Client` calls, an `UpdateStream` that yields `StreamItem::Baseline` once and then updates, a runner future that core spawns and supervises, and `InstanceControl::{quit, retire}`. `Seed` carries `SessionSeed::Stored` (process startup only) or `SessionSeed::Live(SessionSnapshot)` from the retired instance, plus bounded `CarriedPeers` (window chats and pending-send chats, at most 264). `SessionSnapshot` is an opaque placeholder in `backend/seed.rs` for task 5.
- **Rebuild triggers**, each causing exactly one rebuild: runner exit or panic; update stream fatal error, panic, or a transient error still failing after retries at 0.5, 1 and 2 s on the same stream; stall (3 deadline misses within 2 minutes, or all 16 permits parked); wake (`CoreHandle::notify_wake`, control plane); baseline not established within 10 s; bootstrap buffer or tombstone overflow (4,096 each); authenticated migration (`BackendError::Migrated` → `UserError::AccountMigrated`; rebuilds once, a second one halts in `Offline`). Triggers coalesce while a rebuild is running.
- **Retirement order:** bump `ClientGen` and stop admission → cancel the instance's tasks (calls observe a per-instance token and return `Dropped`) and join them, 3 s cap then abort and join → `quit` and await the runner, 5 s cap then `abort` and await again → `InstanceControl::retire` (revoke, capture the snapshot) → apply surviving send outcomes → recycle parked permits → build after backoff (1 s doubling to 60 s, up to 20% jitter, reset after 5 minutes online). Build and retirement run as one spawned step, so the actor keeps serving commands and the cache meanwhile.
- **Bootstrap:** the consumer starts first; snapshots wait for `Baseline`; updates are buffered meanwhile and deletions recorded as tombstones. At the baseline every window is invalidated (`ChatInvalidated`), the open chat's cached tail is shown again, then the first dialogs page and the open chat's server tail are fetched as priority jobs (UI jobs wait until `Online`). Snapshot pages are filtered by tombstones; buffered updates are replayed over them (edits win only if `edit_date` is at least the window's; a replayed message already reflected in the dialogs snapshot does not bump the chat again). Then `Online` and send replay.
- **Light refresh** on `Command::Window(Active)` (at most every 15 s) and every 5 minutes while active: dialogs first page plus the open tail, with updates buffered the same way. The tail is merged into the window in place (deletions, newer edits, appends); a message missing inside the window counts as a gap and replaces the window instead.
- **Sends:** `Sending` and under-cap `Unknown` entries replay after `Online` with the same `random_id`; the 3-attempt cap counts across rebuilds. `Command::DiscardSend` removes `Failed`/`Unknown` entries (`Busy` while in flight, `NotFound` if absent).
- **Shutdown** interrupts an `emit` blocked on a full event queue, then retires the live instance with 1 s / 2 s caps.
- **Events:** `ConnState::{Connecting, Online, Rebuilding{attempt}, Offline}`; `ChatInvalidated` at every bootstrap baseline.
- `FakeBackend` faults: failed build, missing baseline, runner exit/panic, runner slow or deaf to `quit`, stream transient/fatal error and panic, per-call `Delay`, and counters for builds, seeds, runner completions, retirements and send attempts.

Verified: fmt, clippy in both feature configs, 99 core tests (15 lifecycle tests with paused time), no gpui in `cargo tree -p abyss-core`.

Deviations:
- `ClientGen` bumps at the start of a rebuild (plan step 1), not after retirement; only the parked-permit recycling waits for retirement. Completions joined during retirement are therefore stale, except send outcomes, which outlive instances.
- `Message::edited: bool` became `edit_date: Option<i64>` (`edited()` remains as a method), because the merge needs to order edits. The cache schema is now version 2; version 1 message rows are dropped on open.
- With a cache, the server tail's overlap check now runs in the cache worker (`CacheCmd::ReconcileTail`) against what the cache holds, because a rebuild invalidates the window the actor used to compare with.
- The bootstrap's dialogs snapshot still resets the chat list (`DialogsReset`), so pages beyond the first must be reloaded; the refresh does the same. Live updates during a light refresh are delayed until it ends (at most the 30 s call deadline).
- Stream errors are classified by the backend (`StreamError::{Transient, Fatal}`): only transient ones are retried on the same stream.
- `ConnState::Offline` is emitted for each failed build; `Rebuilding{attempt}` only when a rebuild starts. The window state defaults to `Active` until the app reports otherwise.
- `DiscardSend` emits no event on success.

Open for task 6 (and 5/11):
- Task 5 fills `SessionSnapshot` from the `CriticalCell`; `InstanceControl::retire` must revoke the write token. Task 11's grammers `UpdateStream` must yield `Baseline` from `AbyssSession::set_update_state`, and `quit` maps to `pool.handle.quit()`.
- A second `AccountMigrated` leaves core `Offline` (halted) with no instance; task 6 must route it to sign-in, and a fresh login should clear the halt.
- Shutdown does not yet seal persistence or report `SaveStatus`; a retirement step still running at shutdown is waited for 3 s, then aborted (its runner is then left to `shutdown_timeout`).
- A retirement step that panics keeps the previous snapshot and recycles parked permits.
- `SentUnconfirmed` and `updateMessageID` correlation are not modelled yet (task 10/11).

**Task 11 status (2026-11-16): done offline; live verification is task 12.** `GrammersBackend` lives in `crates/core/src/backend/grammers/`:
- **Snapshot unification.** `backend/seed.rs` re-exports `crate::session::SessionSnapshot`; the placeholder is gone. `SessionSnapshot::detached()` (test/`fake` only) gives the fake a snapshot without a persistence worker. `FakeBackend::build` now builds a real `AbyssSession` from the seed and makes one critical write per build (`set_home_dc_id`), so `rev` advances across rebuilds; `FakeControl::retire` revokes, then snapshots. `GrammersControl::retire` does the same (a poisoned cell is logged, the snapshot is still returned).
- **Instance.** `GrammersBackend::new(api_id, PersistHandle)`. `build`: `SessionSeed::Stored` → `PersistHandle::load_session` (an unreadable store falls back to `fresh_session`, logged without details); `Live` → `AbyssSession::from_snapshot`. `SenderPool::with_configuration` with `NoRetries` and `updates_channel_capacity` 1024; `Client` with `auto_cache_peers`; `stream_updates` with `catch_up = false`. Runner = `pool.runner.run()`; `quit` = `pool.handle.quit()`.
- **Update stream** (`stream.rs`): the baseline comes from `AbyssSession::baseline_established()`. The pull future owns the stream and hands it back, so racing the baseline watch never cancels a pull mid-way; `Baseline` is reported once, before any update pulled in the same wake. Errors are classified `Transient`/`Fatal` (`errors::stream_error`); panics are contained by the actor's join handles.
- **Calls** (`calls.rs`, `invoke.rs`): raw TL over an `Invoke` trait (implemented for `grammers_client::Client`, mocked in tests). Retry classes: `Read` sleeps floods of at most 5 s, twice at most, and retries I/O once; `Never` (sends, and auth later) never retries. `303` → `Migrated`. `messages.sendMessage` with the caller's `random_id` and `no_webpage`; the id comes from `UpdateShortSentMessage` or the `updateMessageID` matching the `random_id`, else `SentUnconfirmed`. `messages.readHistory` / `channels.readHistory` with `max_id`. Peers resolve only through the session's cached access hashes (unknown → `Rejected`, no request); every dialogs/history response re-caches the hashes it carries, and the first dialogs page re-pins the page plus the carried peers.
- **Mapping** (`map/`, pure functions): ids are range-checked before grammers' `PeerId` constructors (which debug-panic or overflow out of range); names/titles sanitized; text through `RichText::from_entities` (plain when oversized or with too many entities; URLs bounded); media to inert `MediaKind` placeholders (no download); service messages to sanitized strings naming the actor; broadcast channels excluded (by `post` flag or directory) and counted; forum replies fill `ThreadKey.topic`; deletes bounded to 10,000 ids; dialogs cursor from the last raw dialog.
- **Logging:** only RPC error names matching `[A-Z0-9_]{1,64}` and codes are logged; never payloads, text, names or phones. A test captures `tracing` output at TRACE across failing and succeeding calls and asserts no text, name or phone appears.
- **Contract suite** (`backend/contract.rs`): `contract::run(&impl Backend, target, Sends)` checks build from stored state, baseline first (10 s), no second baseline, dialog paging without overlap, history Tail/Before/After anchors, one `random_id` → one message, read marks, `quit` stopping the runner (5 s) and `retire` returning a usable snapshot. Runs on `FakeBackend` in CI; on `GrammersBackend` under `#[ignore]` (`ABYSS_API_ID`, optional `ABYSS_CONTRACT_CHAT=user:<id>` to enable the send check).

Verified: fmt, clippy in both feature configs, `cargo test --workspace --features fake` (173 core tests, 3 ignored; 55 app), no gpui in `cargo tree -p abyss-core`. Grammers module: 45 tests (mapping incl. UTF-16, oversized text, bidi, media, service, broadcast, topics; the RPC error table; retry classes with paused time; byte-identical resends; log redaction; the baseline race).

Deviations:
- Raw TL requests and our own mapping instead of grammers' high-level iterators and message wrappers, so retries, `random_id` and sanitizing stay in our hands.
- New `BackendError::SentUnconfirmed` (accepted without an id mapping, or `RANDOM_ID_DUPLICATE`); core marks the send `SentUnconfirmed` and drops it from the pending table at once. Core maps it to `UserError::Network` where a user error is needed.
- `ThreadKey.topic` is filled for forum messages (Stage 1 still never keys windows on it).
- `Preview::of` moved from `actor/apply.rs` to `model.rs`.
- The fake now deduplicates `(chat, random_id)` (second send → `SentUnconfirmed`) and makes one critical write per build.
- `updateMessageID` arriving on the update stream (rather than in the send response) is not correlated; the send response is authoritative.
- `GrammersBackend` is not wired into `core.rs`/`api.rs` (task 6 owns that, together with auth).

Remaining for task 12 (live):
- Run the ignored contract test against a real account (and the test DC, if usable).
- Confirm `getHistory` After semantics (`add_offset = -limit`, `min_id`) and the dialogs cursor at page boundaries with excluded broadcasts.
- Confirm the baseline arrives within 10 s on a fresh and on a stored session, and that `quit` stops the runner promptly.
- Watch for grammers-internal panics on odd peers in the update path (contained by core, but each one costs a rebuild).

Auth TODOs for task 6 (listed in `grammers/mod.rs`): `is_authorized`, `request_login_code` (needs `api_hash`), `sign_in`, `check_password` (re-fetch via `account.getPassword` on `SRP_ID_INVALID`), `users.getUsers` self after sign-in, `sign_out`. All auth calls use `RetryClass::Never` with typed error mapping; `Unauthorized` from any call must trigger a local logout; no bootstrap while signed out.

**Task 9 status (2026-11-16): done, with one deferral.** The chat list is `crates/app/src/chat/list.rs` (model) and `crates/app/src/shell/chat_view.rs` (a `uniform_list` sidebar: avatar initials, kind tag, time, sanitized 100-char preview, unread badge with `@` for mentions). `DialogsReset` replaces the list, `DialogsAppended` upserts, `ChatBumped` moves a chat to the top of its section (pinned or not) only when its last message or date changed, so read-state bumps do not reorder. Scrolling within 20 rows of the end asks for the next page once per list length, never past `MAX_DIALOGS`. Selecting a chat sends `OpenChat` and focuses the composer.

Dialogs-refresh truncation is fixed in core (`store::Dialogs::merge_first_page`, `actor/apply.rs::on_dialogs`), not the app, so core stays the single source of truth. A refreshed first page becomes the head of the list. Previously loaded chats missing from it are kept when they are not newer than the page's oldest unpinned chat: those sort below the page, so the server would return them on later pages anyway. Kept chats are unpinned (a pinned chat absent from the page was unpinned) and stay sorted newest first; the list stays bounded by `MAX_DIALOGS`. A first page that is also the last (`next == None`) replaces the list, since it is the whole truth. The pagination cursor is kept when older pages survived the merge, so scrolling continues from where it was instead of refetching. `DialogsAppended` upserts in core too, so a later page that overlaps the merged head updates in place instead of duplicating. This supersedes the task 4 note that a refresh resets the list to one page. Tests: 4 store tests plus `refresh_merges_the_first_page_into_loaded_pages` in the actor.

Deviations:
- Forum topic expansion is deferred. Topics are a Stage 2 non-goal (section 0), and neither the model nor the backend carries topic data. Forum chats are tagged "forum" and the chat header says topics arrive later; messages show as one stream.
- A merged list can keep a chat the user left between refreshes until it is bumped out or the app restarts (the server does not report removals in a page).

Manual checks remaining: scroll smoothness with 1,000 dialogs (`--fake` seeds 1,000), pinned ordering against a real account, pagination against grammers (task 11).

**Task 10 status (2026-11-16): done against the fake.** `crates/app/src/chat/`:
- `messages.rs`: `MessageWindow` mirrors core's window per `ViewGen` (a stale view is ignored, a newer one resets, nothing but a tail seeds an empty window) and returns `RowOp`s (coalesced splices, remeasures) that keep a GPUI `ListState` (bottom-aligned, `FollowMode::Tail`) in step, so a prepend is one `splice(0..0, n)` and the scroll anchor holds. Scrolling within 10 rows of the top sends `LoadOlder`; within 10 of the bottom while detached sends `LoadNewer`; each once until any `History` or `Error` answers (core drops requests while a load runs, so a stuck flag would stall scrolling). Bounded by `MAX_MESSAGES_PER_CHAT`; a "Jump to latest" button reopens the tail when detached.
- `rich.rs`: `RichText` spans become `TextRun`s for `StyledText::with_runs`, each run as long as its own piece, so the partition is exact by construction; a malformed one falls back to plain text rather than panicking. Bold, italic, underline, strike, code/pre (monospace with background), mention (colored), spoiler (hidden until clicked, per message, bounded set). Links render as text; a hidden `TextUrl` target is appended as ` (target)`, sanitized and cut at 256 bytes. Clicking a link copies the exact target to the clipboard and shows "Link copied"; nothing opens a browser. Tests: per-feature cases, a 2,000-case generated partition fuzz (surrogate-splitting and out-of-range offsets), and the headless `with_runs` render through `InteractiveText` in `TestAppContext`.
- Rows show sender names in groups, day separators, time, an "edited" marker, service messages, reply quotes, and inert media chips (kind, file name, size); nothing is downloaded.
- `sends.rs`: composer submit sends `SendText` with a UI-allocated `LocalId` and shows an echo row (at most 256) showing Sending, Sent, Sent unconfirmed, Not sent (Failed) or Not sure it was sent (Unknown). Failed and Unknown get Retry (`RetrySend`) and Discard (`DiscardSend`). A Sent echo leaves when its server id appears in the window; SentUnconfirmed and older Sent echoes leave when a new tail arrives. A full command queue shows Busy and keeps the composer text.
- Mark read: after each frame, the newest message row inside the viewport is found; `MarkRead` is sent when it is above the last sent mark and the window is active.
- Window activation sends `Command::Window(Active/Inactive)`; GPUI's `on_system_wake` calls `CoreHandle::notify_wake()` (new defaulted `CoreLink::notify_wake`).
- `demo.rs` (`--fake`): 1,000 dialogs of every kind and a 2,000-message "Formatting showcase" chat (entities, emoji, RTL, 4,100-char text, every media kind, service, unsupported, edits).

Deviations:
- Echoes are UI-side and listed after the window's messages, not merged by date; the fake does not echo own sends, so with `--fake` a Sent echo stays until the chat reloads.
- Links copy only; there is no "open in browser" prompt in Stage 1.
- Topic threads are not shown (see task 9).

Manual checks remaining: prepend anchoring and live-tail follow under real scrolling, mark-read timing, system wake after sleep, IME in the composer, and the rich-text rendering against the showcase chat.

## 14. Risks and unknowns

1. **GPUI** is pre-1.0. `input.rs` is an example, not a maintained widget, and the composer and `PasswordInput` are the largest UI unknowns.
2. **Prepend jump.** It's unverified whether `splice` anchoring avoids a visible jump. If it doesn't, we fix it in GPUI usage, not with a scroll-restore hack.
3. **Keychain prompts.** The dev binary is ad-hoc signed and changes on every rebuild, so each rebuild triggers Keychain ACL prompts until Stage 3 signing. The fake backend avoids them.
4. **grammers panic sites** are contained, but each one loses that operation. We'll file upstream issues.
5. **Reconcile cost.** Every rebuild reloads dialogs and the open chat tail, which costs extra requests and is a possible flood-wait source under flapping networks. Backoff and the 5-minute stability reset bound it.
6. **Authenticated DC migration** is deferred and unimplemented. We consider it unreachable for Stage 1 calls; if it happens, the user is asked to sign in again. Implementing it needs export/import authorization (`api.tl:2424-2425`) and a rebuild with the new home DC.
7. **Silent gaps** are a residual risk. A "difference too long" (`message_box/mod.rs:675-680`) or an untracked channel difference ending as `Banned` (`updates_receiver.rs:50-80`) gives no signal. They're bounded only by the light refresh (on window activation and every 5 minutes) and by rebuilds. Follow-up: ask upstream for a signal.
8. **Unverified:** what `iter_dialogs` returns for the archive folder, and whether grammers supports Telegram's test DC.
9. **Password copies outside `PasswordBuf`** are deferred residual risks:
   - grammers' SRP computation;
   - GPUI and AppKit framework buffers along the input path;
   - the system clipboard, if the user pastes the password.
10. **Anti-spam checks** on logins from a new third-party `api_id`.
11. **The raw send adapter** must track `messages.sendMessage` schema changes on every grammers bump (`api.tl:2627`).
12. **Accessibility** is deferred to Stage 3 (`Z/examples/a11y.rs`).

## 15. Review log

### Round 1 (verdict: needs rework)

| # | Finding | Resolution (current design) |
|---|---|---|
| 1 | Logout could recreate the session through unordered writes | Ordered persistence worker with revision and generation checks, delete last (sections 3, 5) |
| 2 | Unsafe shutdown: runtime context, main-thread blocking, 200 ms quit deadline | Core thread owns the runtime, control plane, early shutdown start, honest status (sections 1, 5) |
| 3 | Unbounded spawning; auth conflicts; stale responses | Admission before spawn, serialized auth, single-flight, coalescing, generations, deadlines (section 1) |
| 4 | Pool-level retry policy auto-sleeps on auth; expired code; consumed password token | `NoRetries` pool, adapter retry classes, Send-new-code, token refetch (section 4) |
| 5 | No supervision; too-long difference; DC migration | The single rebuild mechanism and reconcile; 303 policy (section 2) |
| 6 | FIFO eviction broke the self-user contract and channel recovery | Pinned peers, self-user sentinel, in-memory only (section 3) |
| 7 | grammers logs payloads at WARN | All `grammers*` events dropped (section 11) |
| 8 | Password plaintext leaks through mapping and text APIs | `PasswordInput` with a fixed buffer, bullets-only APIs, secure input (section 10) |
| 9 | `with_runs` panics on non-partition runs | Core normalization into disjoint segments; proptest (section 6) |
| 10 | `mark_as_read` sends `max_id: 0` | Raw read-history with the highest displayed id (section 7) |
| 11 | Send retries duplicate messages | One `random_id` per logical send (section 9) |
| 12 | Manual anchor restore conflicts with `splice` | Rely on `splice` (section 8) |
| 13 | Features don't toggle by profile; test-support | Explicit features, aliases, production-build job (section 12) |
| 14 | Broadcast channels expand scope | Excluded via `Peer::Channel` (section 7) |

### Round 2 (verdict: needs rework)

| # | Finding | Resolution in v3 |
|---|---|---|
| 1 | Logout cancelled the runner before `auth.logOut` (`sender_pool.rs:154` → `Dropped`) | Runner kept alive for `auth.logOut`, then stopped and joined, then delete; remote and local results reported separately (section 5) |
| 2 | Semaphore didn't bound grammers work after timeouts (`sender_pool.rs:154`, `sender.rs:140`) | Timed-out permits are parked until rebuild; teardown drops grammers' queues; stall triggers rebuild (sections 1, 2) |
| 3 | Persistence: stale seq, failure semantics, Flush durability, login metadata treated as lazy (`auth.rs:113`) | Only critical state is persisted; revision captured under the lock; stale revisions dropped; `Barrier` returns the storage `Result`; self-user login is critical; delete failure surfaced (sections 3, 4, 5) |
| 4 | Contradictory shutdown guarantees; `spawn_blocking` can't be aborted; runner stopped after seal although it persists DC options (`sender_pool.rs:393`) | Runner quiesced first, then a bounded wait for the in-flight critical write; `Confirmed`/`Unconfirmed` reported, never claimed (section 5) |
| 5 | `stream_updates` consumes the unique receiver (`updates.rs:69`, `updates_receiver.rs:28`); `catch_up` defaults to false; state not synced (`updates.rs:51`) | A broken stream means a full client rebuild; `catch_up: false` set explicitly; update state in memory only, recovered by reconcile (section 2) |
| 6 | Authenticated migration needs export/import authorization (`net.rs:116`); home flag captured at connection creation (`sender_pool.rs:397`) | Verified: grammers handles 303 only during login and for files; Stage 1 surfaces an error, rebuilds, and asks for re-login if it recurs; documented as risk 6 (section 2) |
| 7 | A missing channel hash ends the difference as `Banned` silently (`updates_receiver.rs:50`, `message_box/mod.rs:875`); partial peer info must merge (`sqlite.rs:380`) | Pinned in-memory peer map for loaded dialogs; `extend_info` merge; silent end documented, recovery by rebuild/reconcile (section 3) |
| 8 | `check_password` token lost on timeout, panic or failed refetch | `PasswordTokenUnavailable` state, Drop-guarded pending flag, refetch before resubmit, `is_authorized` probe after ambiguous completion (section 4) |
| 9 | Messages lack `random_id`; correlation only via `updateMessageID` (`api.tl:128, 330`) | Correlate from responses including `updatesCombined` and from stream `updateMessageID`; explicit `SentUnconfirmed`; no history matching; never a fresh id (section 9) |
| 10 | No entity or URL processing budget | Hard caps checked before normalization, with plain-text fallback; separate event byte budget (section 6) |
| 11 | Growable zeroizing string leaves old allocations; `input.rs:362` `to_owned` temporaries | Fixed 256-byte `PasswordBuf` with in-place edits, separate `PasswordInput` element, dead-key and IME tests (section 10) |
| 12 | The prepend fallback restored the wrong index | Fallback deleted; rely on `splice` (section 8) |
| 13 | Plan not self-contained | v3 is a complete standalone document |

### Round 3 (verdict: ready after fixes), applied in v3.1

| # | Finding | Resolution in v3.1 |
|---|---|---|
| 1 (P1) | Reconcile ran before the stream's baseline (`updates_receiver.rs:126`; updates at or below the baseline dropped, `message_box/mod.rs:430`) | Verified that the first `next()` sends `getState` before waiting (`updates_receiver.rs:126-145`). The consumer starts first, `set_update_state` signals the baseline (`:132`), live updates are buffered during bootstrap, and snapshots are merged under them (edits by `edit_date`, deletions through a bounded tombstone set); tests (section 2) |
| 2 (P2) | Silent "difference too long" (`message_box/mod.rs:675`) | No detection promised; light refresh on window activation and every 5 minutes; residual risk 7; upstream follow-up (sections 2, 14) |
| 3 (P1) | Rebuild reloaded from the Keychain | Rebuild seeds from the live `CriticalCell` (keeping `SignedInUnsaved` and durability status) plus a bounded carried `PeerRef` set; only process startup reads the Keychain; `AccountGen` is separate from `ClientGen`, `rev` is continuous, and write tokens are revoked at retirement (sections 2, 3) |
| 4 (P2) | `abort()` only schedules cancellation, and dropping the `JoinSet` doesn't join | Explicit retirement: stop admission, cancel and join instance tasks, quit the runner, abort if needed, await completion, revoke the token, then recycle permits. Seal only after the critical writers stop (only runner methods write DC options, `sender_pool.rs:394, 472-518`). `Unconfirmed` past the deadline; old generations retired (sections 2, 3, 5) |
| 5 (P2) | A Barrier could hang when `rev` advanced without a Write (identical DC option re-set on reconnect, `sender_pool.rs:386-394`) | `rev` advances only when the serialized bytes change; a Barrier uses the current `rev` and resolves with the last outcome; the loaded state is `rev 0 / Durable`; tests (section 3) |
| 6 (P2) | Send outcomes tied to `ViewGen`; no exhaustion state | Account-level pending-send table owns outcomes; `Unknown{attempts}` is distinct from `SentUnconfirmed`; replay rules defined; tests for close-before-ack and exhaustion across rebuilds (section 9) |
| 7 | Visible URL bytes not counted in the URL budget | Budget counts `TextUrl` targets plus visible `Url` text (fixed in code by the coordinator; plan wording aligned). Invalid UTF-16 boundaries are dropped, not clamped (section 6) |
| Deferred | Full authenticated migration; framework, clipboard and SRP password copies | Recorded as residual risks 6 and 9 (section 14) |

- **Round 4 (code review of task 4, 2026-11-16).** These are open, and are fixed in "Task 4b" after the 6/9/10/11 wave merges, because that wave touches the same files.
  - **High:** reopening the bootstrap tail chat during bootstrap orphans its snapshot, so `Online` never comes (`boot.rs`, `apply.rs`). Track the snapshot by `(chat, request id)`, complete it even when stale, and queue a replacement through bootstrap.
  - Refresh never removes a deleted newest message, or anything at all when the server tail is empty, because deletion detection is capped at the returned max id. Fix: reconcile the pre-request window against the authoritative tail, including the newest boundary.
  - Dialog replay suppression keys on timestamps, so two messages in the same second lose a preview or unread count. Fix: carry the snapshot's top message id.
  - `contiguous` grows by one entry per visited chat. Fix: bound it with the windows.
  - A send that gets `Migrated` or `FloodWait` becomes `Failed` and is stranded after the rebuild. Fix: classify both separately and keep the send replayable with the same `random_id`.
  - The send-replay test only covers an unaccepted attempt. Fix: add an "accepted, response lost" fault plus fake server dedup by `random_id`, and check the 3-attempt cap across several rebuilds.
  - Shutdown during a retirement skips revoke. This one went to task 6.

## 16. Encrypted message cache (added after v3.1, user decision 2026-11-16)

History must survive restarts, so Stage 1 adds an encrypted on-disk cache. This supersedes the "nothing on disk except the Keychain item" property in sections 3 and 9. The new property: **nothing plaintext on disk; everything on disk is undecryptable without a device-only Keychain key.**

- **Engine:** SQLite + SQLCipher via `rusqlite` `bundled-sqlcipher`. On macOS this links CommonCrypto/Security.framework, not OpenSSL (`libsqlite3-sys` build.rs: `SQLCIPHER_CRYPTO_CC` when no `OPENSSL_DIR` is set). `Cache::open` asserts `PRAGMA cipher_provider = commoncrypto`, so a build that silently picked up OpenSSL fails closed.
- **Key:** 256-bit random (`getrandom`), handed to SQLCipher through `sqlite3_key_v2` as a raw key (`x'..'`, no KDF) from a fixed-size zeroizing buffer, so no SQL string ever contains it. `cipher_memory_security` is enabled before keying. Keychain reads hand over a `Zeroizing<[u8; 32]>`. Stored as its own Keychain item (`dev.abyss.cachekey`, device-only, non-synchronizable) through the `SecretStore`/persistence worker, separate from the session item.
- **Pragmas:** `cipher_memory_security = ON`, `temp_store = MEMORY` (no plaintext temp files), `secure_delete = ON`, WAL (the WAL is encrypted too).
- **Location:** a 0700 directory in Application Support (later the sandbox container), excluded from backups with the same sticky xattr `tmutil addexclusion` writes.
- **Contents:** only our validated domain model (`postcard`). Bodies over 64 KiB are filtered in SQL (`length(body)`), so they never reach Rust memory. Every decoded row must match its SQL key (message id, chat) and pass `is_valid()` (sanitized names, bounded previews and service text); the same check gates writes. Decoding re-validates `RichText` invariants (`#[serde(try_from)]`), so a corrupted or tampered row can never reach `StyledText::with_runs`. Bad rows are deleted and skipped.
- **Bounds:** 512 MiB of message bodies in total, evicting the oldest messages of the least recently opened chats first, using an indexed `cached_chats(last_access)` table instead of scanning messages; byte accounting is committed only with its transaction; failing to get under budget is an explicit `OverBudget` error; 50,000 messages per chat; chat list capped at `MAX_DIALOGS`.
- **Disposable:** a wrong key or unknown schema version means destroy and recreate, never a fatal error.
- **Logout = crypto-erase** via `Cache::erase(self, dir, key, delete_key)`: close the connection (SQLCipher wipes its key copy), drop our key, delete the Keychain item, and only then unlink the files. If the key deletion fails, the files are kept and the error is returned. The cache worker must stop admitting and drain commands before calling it (it consumes `Cache`). `destroy_unopened` is only for caches that failed to open.
- **Freshness:** the server stays authoritative. Opening a chat shows the cached tail immediately, then each server history page is applied with `reconcile_range(lo, hi, page)`, which deletes cached messages the server no longer has in that range. Live updates (new, edit, delete) write through. Silent gaps (risk 7) also apply to the cache; the 5-minute and on-activation refreshes reconcile the open chat.
- **Memory windows slide:** the 2,000-message in-memory window no longer stops at the cap. Scrolling past it drops the far end and reloads it from the cache (then the server) on return.
- **Threading:** `Cache` is blocking. It's owned by a dedicated cache worker thread, which receives ordered commands from the actor, mirroring the persistence worker. The async runtime never blocks on disk.
- **Status:** the storage layer is implemented (`crates/core/src/cache/`, 20 tests, including regressions for the cache review: roundtrip, wrong key, no plaintext in DB/WAL, 0700 + backup exclusion, reconcile, unscoped delete, corrupt rows, schema version, per-chat cap, LRU budget, byte accounting). Still to do: the cache worker, Keychain key handling (task 5), wiring into the actor and the sliding window (tasks 3–4, 10).

## 17. Onboarding: connecting abyss to Telegram (added 2026-11-16)

Replaces the bare "missing credentials" error screen (section 12) with a guided first-run flow. Lands as **task 6b**, after task 6, because it needs the persistence worker (task 5) and the login state machine (task 6).

### 17.1 Credential model

Telegram requires an `api_id`/`api_hash` per client application. Decision: **bring your own credentials (BYO)** by default.
- A shared project `api_id` gives no secrecy: it's extractable from any binary. It also ties every user to one registration that Telegram can rate-limit or revoke, and risk 10 (anti-spam checks on new third-party ids) would hit all users at once.
- BYO costs one visit to my.telegram.org. The onboarding exists to make that visit painless.
- A packaged release may later embed a project id at build time (`option_env!("ABYSS_EMBEDDED_API_ID")` / `..._HASH`). This is off by default and not part of Stage 1.

**Resolution order:**
1. `ABYSS_API_ID` + `ABYSS_API_HASH` env vars. This is a dev override and is never persisted.
2. The Keychain item `dev.abyss.apicreds`, owned by the persistence worker with the same attributes as the session item (section 3, task 5 notes).
3. The embedded build-time pair, if compiled in.
4. Otherwise go to onboarding.

### 17.2 Core changes

- `AuthState::MissingCredentials` becomes `AuthState::NeedCredentials { error: Option<CredentialError> }`.
- Core starts its persistence worker before it has credentials. With no credentials it emits `NeedCredentials`, and no backend is built.
- `Command::SubmitCredentials { api_id: i32, api_hash: ApiHash }` builds the client and moves to `Connecting` then `Phone`.
  - Validation: `api_id > 0`. `ApiHash` is exactly 32 hex chars, normalized to lowercase, with whitespace trimmed on paste.
  - Persist **only after the first successful `request_login_code`**, which proves the pair is valid. Until then the pair lives in memory only.
- `API_ID_INVALID` and `API_ID_PUBLISHED_FLOOD` from `request_login_code` map to `CredentialError`. The state goes back to `NeedCredentials` with that error, without touching the session. The client is retired through the normal rebuild boundary.
- `Command::ForgetStoredCredentials` (renamed from `ResetCredentials`) is allowed only while logged out and only deletes the stored pair. It never touches an env override. While signed in, changing credentials means logging out first; the UI says so.
- `ApiHash` has a redacting `Debug` and is never logged. This is the same rule as phone numbers.

### 17.2a Credential provenance and auth-attempt rules (review round 4)

**Provenance.** Resolution is atomic per source. Core reports `CredentialSource::{Env, Stored, Embedded, Entered}` alongside `AuthState` and the UI shows which source is in control.
- **Env pair:**
  - A partial env pair (only one variable set) is an explicit error naming the missing variable. It never falls through to the Keychain.
  - An invalid env pair yields `NeedCredentials { error, source: Env }`, and the screen explains how to unset the variables. Entering a new pair is disabled while the env override is active.
- **Keychain:**
  - A Keychain read *failure* (locked, denied) is distinct from *absent*. It shows "Keychain unavailable" with Retry, and never shows the onboarding as if no pair were stored.
  - Credential write and delete failures surface as `UserError` with Retry. A failed write after a successful `sendCode` does not block login; the pair is retried on the next successful auth step and stays in memory until then.

**Auth attempts.** Every `SubmitCredentials`, `RequestCode`, `SubmitCode` and `SubmitPassword` runs under an `AuthAttempt` id.
- **Back and Forget** are actor transitions, not UI-only navigation. Each one:
  1. bumps the attempt id;
  2. retires the client built for the superseded credentials through the normal retirement boundary;
  3. clears the login and password tokens.
- **Stale completions** carry an old attempt id and are dropped, including credential persistence. A late `sendCode` success can never persist a superseded pair.
- **Ambiguous completion:** if a superseded `sign_in` or `check_password` may have completed, core runs the section 4 `is_authorized` probe before it accepts new credentials. If the probe says authorized, the user is signed in with the pair in use, and that pair is persisted.

**Sent-code handling.** The pinned grammers `request_login_code` discards the sent-code type, timeout and next type (`auth.rs:290`). Use a raw `auth.sendCode` adapter that keeps the full `auth.SentCode`.
- **Supported:** delivery in the app, SMS, call and flash call shown as text, plus server-directed resend with the server timeout.
- **Unsupported:** the email-setup flow (`sentCodeTypeSetUpEmailRequired`) and `auth.sentCodeSuccess`. Both show "Finish this step in an official Telegram app, then try again" and never fake the flow.

**`API_ID_PUBLISHED_FLOOD`** means the id is unusable (for example, it was published). It is not a timed wait. The message says to create or use a different application, with no countdown.

### 17.3 Screens

Every screen has Back except Welcome. Copy lives in one module (`crates/app/src/onboarding/copy.rs`) so docs and UI stay in sync.

1. **Welcome.** What abyss is (a small, auditable client) and an honest threat model:
   - It reduces the *client-side* attack surface.
   - Cloud chats are still readable by Telegram's servers.
   - There are no secret chats, calls, stickers, browser or Mini Apps in v1.

   One button: **Connect to Telegram**.
2. **Get API credentials.** Numbered steps, with **Open my.telegram.org** opening the system browser.
   - The URL is the hardcoded constant `https://my.telegram.org/apps`, opened via `cx.open_url`. This is the only URL onboarding opens directly. Message links keep the separate confirm-before-open policy in section 8.
   - The steps:
     0. Prerequisite: you need an existing Telegram account signed in on an official app (phone or desktop), and you must keep that session available. Login codes for my.telegram.org and for abyss arrive there.
     1. Log in with your phone number. The login code arrives *in your Telegram app*, not by SMS.
     2. Open "API development tools".
     3. Fill App title `abyss`, a Short name of 5–32 letters/digits, and Platform "Desktop"; leave URL empty.
     4. Copy `api_id` and `api_hash`.
   - Troubleshooting:
     - If the form says "ERROR", retry later, with VPN and ad blockers off and a letters-only short name.
     - There is one app per phone number. If you already registered one, copy its existing `api_id`/`api_hash`; don't try to create a second.
     - Never share the `api_hash`.
     - New `api_id`s can draw extra anti-spam scrutiny.
3. **Enter credentials.** The `api_id` field accepts digits only, at most 10. The `api_hash` field accepts hex only, exactly 32 characters; it has a paste-normalizing filter and is masked with a reveal toggle. Inline validation runs, then **Continue** sends `SubmitCredentials`. A `CredentialError` comes back here with a specific message.
4. **Phone** (section 4). International format with a leading `+`. Explain that the code is sent to your other Telegram sessions first.
5. **Code.** Show where the code went, from the full `auth.SentCode` (section 17.2a), including the resend countdown and the next delivery type. Add a fixed warning: "Telegram and abyss will never ask for this code anywhere else." Expect a "new login" notice on your other devices.
6. **Password** (2FA, section 4), with the hint.
7. **Done.** Tips:
   - The session shows as "abyss" under Settings › Devices in official apps, where it can be revoked remotely.
   - If `account.getPassword` reports `has_password = false`, recommend enabling 2FA in an official app. abyss doesn't set it.

Back from screens 4–6 to the credential screens is allowed only before the session is authorized. After that, the only way back is logout.

### 17.4 Docs

`docs/connecting.md` mirrors 17.3 with screenshots. The README "Getting started" links to it. Its text is generated from or checked against `copy.rs` by a unit test that greps the doc for each step's key phrase.

### 17.5 Tests

- **Core:**
  - The credential validators: boundaries, case, whitespace, non-hex.
  - Credentials are not persisted before a successful code request and are persisted after.
  - An env override is never persisted.
  - `API_ID_INVALID` returns to `NeedCredentials` with an error, and the session stays untouched.
  - `ForgetStoredCredentials` is refused while signed in and never affects an env override.
  - A partial env pair is an error. A Keychain read failure is not treated as absent.
  - A stale `sendCode` success after Back persists nothing. A superseded `sign_in` that succeeded is detected by the probe.
  - The raw `sendCode` adapter keeps the delivery type and timeout. Email setup and `sentCodeSuccess` route to the official-app message.
  - `API_ID_PUBLISHED_FLOOD` shows no countdown.
  - `ApiHash` `Debug` redacts.
- **Fake backend:** scripted `API_ID_INVALID` and `API_ID_PUBLISHED_FLOOD` on the first code request.
- **App:**
  - The onboarding router follows each `AuthState`.
  - Back navigation follows the rules in 17.3.
  - `cargo dev -- --onboarding` starts the fake from `NeedCredentials` so the whole flow can be clicked through.
- **Manual (task 12):** create a real app on my.telegram.org following only the in-app text, and record any wording fixes.
