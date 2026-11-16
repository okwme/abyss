//! A running core and the UI state built from its events.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use abyss_core::api::{Command, ConnState, Event, PasswordBuf, SendState, UserError};
use abyss_core::model::{ChatId, LocalId};
use abyss_core::{BackendChoice, CoreConfig, SendError, StartError};
use gpui::{AppContext, Context, Entity, Focusable, SharedString, Subscription, Task, Window};

use super::Root;
use super::auth::AuthView;
use super::bridge::{CoreLink, SharedCore, spawn_pump};
use super::chats::ChatList;
use super::shutdown::{self, Closing, Next};
use crate::launch::Launch;
use crate::ui::copy;
use crate::ui::password::{PasswordEvent, PasswordInput};
use crate::ui::text_input::{Limits, TextInput, TextInputEvent, text_input};

/// Demo account size for `--fake`.
#[cfg(feature = "fake")]
const DEMO_CHATS: u32 = 60;
#[cfg(feature = "fake")]
const DEMO_MESSAGES: u32 = 40;

/// How long a notice (toast) stays up.
const NOTICE_FOR: Duration = Duration::from_secs(5);

/// Who answers login submissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoginDriver {
    Core,
    /// `--fake`: login is simulated in the UI (see `auth::demo`).
    #[cfg(feature = "fake")]
    Demo,
}

pub(crate) struct Started {
    pub(super) core: Box<dyn CoreLink>,
    pub(super) driver: LoginDriver,
    /// Keeps the fake's update channel open; dropping it ends core's update
    /// stream.
    #[cfg(feature = "fake")]
    pub(super) fake: Option<abyss_core::backend::fake::FakeHandle>,
}

pub(crate) fn start(launch: Launch) -> Result<Started, StartError> {
    match launch {
        #[cfg(feature = "fake")]
        Launch::Fake => {
            use abyss_core::backend::fake::{FakeBackend, FakeScript};
            let (backend, handle) = FakeBackend::new(FakeScript::demo(DEMO_CHATS, DEMO_MESSAGES));
            let core = abyss_core::start(CoreConfig::default(), BackendChoice::Fake(backend))?;
            Ok(Started {
                core: Box::new(core),
                driver: LoginDriver::Demo,
                fake: Some(handle),
            })
        }
        Launch::Telegram(credentials) => {
            let core =
                abyss_core::start(CoreConfig::default(), BackendChoice::Grammers(credentials))?;
            Ok(Started {
                core: Box::new(core),
                driver: LoginDriver::Core,
                #[cfg(feature = "fake")]
                fake: None,
            })
        }
    }
}

pub(crate) struct Session {
    core: SharedCore,
    pub(super) driver: LoginDriver,
    pub(super) auth: AuthView,
    pub(super) phone: Entity<TextInput>,
    pub(super) code: Entity<TextInput>,
    pub(super) password: Entity<PasswordInput>,
    pub(super) composer: Entity<TextInput>,
    pub(super) chats: ChatList,
    pub(super) selected: Option<ChatId>,
    pub(super) conn: Option<ConnState>,
    pub(super) last_send: Option<SendState>,
    pub(super) notice: Option<SharedString>,
    notice_gen: u64,
    pub(super) closing: Closing,
    next_local: u64,
    _pump: Task<()>,
    #[cfg(feature = "fake")]
    _fake: Option<abyss_core::backend::fake::FakeHandle>,
    _subscriptions: [Subscription; 4],
}

impl Session {
    pub(crate) fn new(started: Started, window: &mut Window, cx: &mut Context<'_, Root>) -> Self {
        let shared: SharedCore = Rc::new(RefCell::new(started.core));
        let phone = text_input(Limits::PHONE, copy::PHONE_PLACEHOLDER, cx);
        let code = text_input(Limits::CODE, copy::CODE_PLACEHOLDER, cx);
        let composer = text_input(Limits::COMPOSER, copy::COMPOSER_PLACEHOLDER, cx);
        let password = cx.new(|cx| PasswordInput::new(window, cx));
        let subscriptions = [
            cx.subscribe_in(&phone, window, |root, _, _: &TextInputEvent, window, cx| {
                root.with_session(|s, cx| s.submit_phone(window, cx), cx);
            }),
            cx.subscribe_in(&code, window, |root, _, _: &TextInputEvent, window, cx| {
                root.with_session(|s, cx| s.submit_code(window, cx), cx);
            }),
            cx.subscribe_in(
                &password,
                window,
                |root, _, _: &PasswordEvent, window, cx| {
                    root.with_session(|s, cx| s.submit_password(window, cx), cx);
                },
            ),
            cx.subscribe_in(
                &composer,
                window,
                |root, _, _: &TextInputEvent, window, cx| {
                    root.with_session(|s, cx| s.submit_message(window, cx), cx);
                },
            ),
        ];
        let auth = match started.driver {
            LoginDriver::Core => AuthView::Connecting,
            #[cfg(feature = "fake")]
            LoginDriver::Demo => AuthView::Phone { error: None },
        };
        let pump = spawn_pump(Rc::clone(&shared), window, cx);
        let session = Self {
            core: shared,
            driver: started.driver,
            auth,
            phone,
            code,
            password,
            composer,
            chats: ChatList::default(),
            selected: None,
            conn: None,
            last_send: None,
            notice: None,
            notice_gen: 0,
            closing: Closing::Open,
            next_local: 1,
            _pump: pump,
            #[cfg(feature = "fake")]
            _fake: started.fake,
            _subscriptions: subscriptions,
        };
        session.focus_for_auth(window, cx);
        session
    }

    /// Queues a command. A full queue shows "busy"; the rejected command is
    /// dropped (a password in it is zeroized on drop).
    fn send(&mut self, cmd: Command, window: &mut Window, cx: &mut Context<'_, Root>) {
        let result = match self.core.try_borrow() {
            Ok(core) => core.send(cmd),
            Err(_) => Err(SendError::Full(cmd)),
        };
        match result {
            Ok(()) => {}
            Err(SendError::Full(_)) => {
                self.show_notice(copy::user_error(UserError::Busy), window, cx);
            }
            Err(SendError::Closed(_)) => {
                self.show_notice(copy::CORE_GONE.into(), window, cx);
            }
        }
    }

    pub(super) fn show_notice(
        &mut self,
        text: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.notice = Some(text);
        self.notice_gen = self.notice_gen.wrapping_add(1);
        let generation = self.notice_gen;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(NOTICE_FOR).await;
            this.update(cx, |root, cx| {
                root.with_session(|s, _| s.expire_notice(generation), cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn expire_notice(&mut self, generation: u64) {
        if self.notice_gen == generation {
            self.notice = None;
        }
    }

    pub(super) fn set_auth(
        &mut self,
        next: AuthView,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let leaving_password = matches!(self.auth, AuthView::Password { .. })
            && !matches!(next, AuthView::Password { .. });
        if leaving_password {
            self.password.update(cx, PasswordInput::clear);
        }
        self.auth = next;
        if let AuthView::FloodWait { .. } = self.auth {
            spawn_flood_ticker(window, cx);
        }
        self.focus_for_auth(window, cx);
        cx.notify();
    }

    /// Called every second during a flood wait. Returns whether to keep ticking.
    pub(super) fn flood_tick(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) -> bool {
        let AuthView::FloodWait { until, resume } = &self.auth else {
            return false;
        };
        if Instant::now() < *until {
            cx.notify();
            return true;
        }
        let resume = (**resume).clone();
        self.set_auth(resume, window, cx);
        false
    }

    fn focus_for_auth(&self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let handle = match self.auth {
            AuthView::Phone { .. } => self.phone.focus_handle(cx),
            AuthView::Code { .. } => self.code.focus_handle(cx),
            AuthView::Password { .. } => self.password.focus_handle(cx),
            AuthView::SignedIn if self.selected.is_some() => self.composer.focus_handle(cx),
            AuthView::Connecting | AuthView::FloodWait { .. } | AuthView::SignedIn => return,
        };
        window.focus(&handle, cx);
    }

    pub(super) fn submit_phone(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Phone { .. }) {
            return;
        }
        let phone = self.phone.read(cx).text().to_owned();
        match self.driver {
            LoginDriver::Core => {
                self.auth = self.auth.cleared();
                self.send(Command::SubmitPhone(phone), window, cx);
            }
            #[cfg(feature = "fake")]
            LoginDriver::Demo => self.set_auth(super::auth::demo::phone(&phone), window, cx),
        }
        cx.notify();
    }

    pub(super) fn submit_code(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Code { .. }) {
            return;
        }
        let code = self
            .code
            .update(cx, |input, cx| input.take_text(window, cx));
        match self.driver {
            LoginDriver::Core => {
                self.auth = self.auth.cleared();
                self.send(Command::SubmitCode(code), window, cx);
            }
            #[cfg(feature = "fake")]
            LoginDriver::Demo => {
                let next = super::auth::demo::code(&self.auth, &code, Instant::now());
                self.set_auth(next, window, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn submit_password(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Password { .. }) {
            return;
        }
        let secret = self.password.update(cx, PasswordInput::take_secret);
        match self.driver {
            LoginDriver::Core => {
                self.auth = self.auth.cleared();
                self.send(Command::SubmitPassword(PasswordBuf(secret)), window, cx);
            }
            #[cfg(feature = "fake")]
            LoginDriver::Demo => {
                let next = super::auth::demo::password(&self.auth, secret.len());
                drop(secret);
                self.set_auth(next, window, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn submit_message(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let Some(chat) = self.selected.filter(|_| self.auth == AuthView::SignedIn) else {
            return;
        };
        if self.composer.read(cx).text().trim().is_empty() {
            return;
        }
        let text = self.composer.update(cx, |c, cx| c.take_text(window, cx));
        let local_id = LocalId(self.next_local);
        self.next_local = self.next_local.wrapping_add(1);
        self.send(
            Command::SendText {
                chat,
                text,
                local_id,
            },
            window,
            cx,
        );
    }

    pub(super) fn select_chat(
        &mut self,
        chat: ChatId,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.selected = Some(chat);
        self.focus_for_auth(window, cx);
        cx.notify();
    }

    pub(super) fn apply_event(
        &mut self,
        event: Event,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        match event {
            Event::Auth(state) => {
                if self.driver == LoginDriver::Core {
                    self.set_auth(state.into(), window, cx);
                }
            }
            Event::Connection(state) => self.conn = Some(state),
            Event::DialogsReset(chats) => self.chats.reset(chats),
            Event::DialogsAppended(chats) => self.chats.append(chats),
            Event::ChatBumped(chat) => self.chats.bump(chat),
            Event::SendState { state, .. } => self.last_send = Some(state),
            Event::Error(error) => self.on_error(error, window, cx),
            // Message views land in task 10.
            Event::History { .. }
            | Event::ChatInvalidated(_)
            | Event::Edited(_)
            | Event::Deleted { .. } => {}
        }
    }

    fn on_error(&mut self, error: UserError, window: &mut Window, cx: &mut Context<'_, Root>) {
        tracing::info!(?error, "core reported an error");
        match self.auth.with_error(error, Instant::now()) {
            Some(next) => self.set_auth(next, window, cx),
            None => self.show_notice(copy::user_error(error), window, cx),
        }
    }

    /// Starts shutdown: asks core to stop and waits for its ack in the
    /// background. Idempotent.
    pub(super) fn begin_shutdown(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.closing != Closing::Open {
            return;
        }
        self.closing = Closing::Waiting;
        self.password.update(cx, PasswordInput::clear);
        let ack = self
            .core
            .try_borrow_mut()
            .ok()
            .and_then(|mut core| core.begin_shutdown());
        let timer = cx.background_executor().timer(shutdown::UI_TIMEOUT);
        tracing::info!("shutdown requested");
        cx.spawn_in(window, async move |this, cx| {
            let outcome = shutdown::await_ack(ack, timer).await;
            tracing::info!(?outcome, "shutdown finished");
            this.update(cx, |root, cx| match shutdown::next(outcome) {
                Next::Quit => cx.quit(),
                Next::ShowUnconfirmed => {
                    root.with_session(|s, _| s.closing = Closing::Unconfirmed, cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Backstop for OS-initiated termination: cancels core without waiting.
    pub(super) fn cancel_core(&self) {
        if let Ok(mut core) = self.core.try_borrow_mut() {
            drop(core.begin_shutdown());
        }
    }
}

/// Re-renders the countdown every second until the flood wait ends.
fn spawn_flood_ticker(window: &mut Window, cx: &mut Context<'_, Root>) {
    cx.spawn_in(window, async move |this, cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let more = this.update_in(cx, |root, window, cx| {
                root.with_session(|s, cx| s.flood_tick(window, cx), cx)
            });
            if more.ok().flatten() != Some(true) {
                return;
            }
        }
    })
    .detach();
}
