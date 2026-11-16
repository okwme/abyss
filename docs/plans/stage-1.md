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
7. **App bridge:** auth router, missing-credentials screen, `Quit` action, the should-close hook. *Done when:* `cargo dev` goes from login to the chat list, and closing shows "Closing…" then exits, or shows the unconfirmed-save message when `FaultyStore` stalls.
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
