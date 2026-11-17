//! A running core and the UI state built from its events.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use abyss_core::api::{AuthState, Command, ConnState, Event, PasswordBuf, UserError};
use abyss_core::{BackendChoice, CoreConfig, SendError, StartError};
use gpui::{
    AppContext, Context, Entity, FocusHandle, Focusable, SharedString, Subscription, Task, Window,
};

use super::Root;
use super::auth::{AuthView, project};
use super::bridge::{CoreLink, EventSource, SharedCore, spawn_pump};
use super::shutdown::{self, Closing, Next};
use crate::chat::ChatShell;
use crate::launch::Launch;
use crate::ui::copy;
use crate::ui::password::{PasswordEvent, PasswordInput};
use crate::ui::text_input::{Limits, TextInput, TextInputEvent, text_input};

/// How long a notice (toast) stays up.
const NOTICE_FOR: Duration = Duration::from_secs(5);

/// A started core, split into its command and event halves.
pub(crate) struct Started {
    pub(super) commands: Box<dyn CoreLink>,
    pub(super) events: Box<dyn EventSource>,
    /// Keeps the fake's update channel open; dropping it ends core's update
    /// stream.
    #[cfg(feature = "fake")]
    pub(super) fake: Option<abyss_core::backend::fake::FakeHandle>,
}

impl Started {
    fn new(core: abyss_core::CoreHandle) -> Self {
        let (commands, events) = core.split();
        Self {
            commands: Box::new(commands),
            events: Box::new(events),
            #[cfg(feature = "fake")]
            fake: None,
        }
    }
}

pub(crate) fn start(launch: Launch) -> Result<Started, StartError> {
    match launch {
        #[cfg(feature = "fake")]
        Launch::Fake { faults } => {
            use abyss_core::backend::fake::{FakeBackend, FakeScript};
            let mut script = crate::chat::demo::script(crate::chat::format::now());
            // The demo account's login: code `FAKE_CODE`, 2FA password with a hint.
            script.login = FakeScript::demo(0, 0).login;
            let (backend, handle) = FakeBackend::new(script);
            let config = CoreConfig {
                fake_faults: faults,
                ..CoreConfig::default()
            };
            let core = abyss_core::start(config, BackendChoice::Fake(backend))?;
            Ok(Started {
                fake: Some(handle),
                ..Started::new(core)
            })
        }
        Launch::Telegram(credentials) => {
            let core =
                abyss_core::start(CoreConfig::default(), BackendChoice::Grammers(credentials))?;
            Ok(Started::new(core))
        }
    }
}

pub(crate) struct Session {
    pub(super) core: SharedCore,
    pub(super) auth: AuthView,
    /// Whether the Welcome screen has been left in this run. It comes before
    /// the phone step of a signed-out run (plan section 17.3).
    pub(super) welcomed: bool,
    /// Holds focus while the Welcome screen shows, so Enter reaches it.
    pub(super) welcome_focus: FocusHandle,
    /// Core said the password token is gone: the next submission asks for new
    /// parameters first.
    needs_params: bool,
    pub(super) phone: Entity<TextInput>,
    pub(super) code: Entity<TextInput>,
    pub(super) password: Entity<PasswordInput>,
    pub(super) composer: Entity<TextInput>,
    /// Chat list, open chat and pending sends (`crate::chat`, `shell::chats`).
    pub(super) shell: ChatShell,
    pub(super) conn: Option<ConnState>,
    pub(super) notice: Option<SharedString>,
    notice_gen: u64,
    pub(super) closing: Closing,
    _pump: Task<()>,
    #[cfg(feature = "fake")]
    fake: Option<abyss_core::backend::fake::FakeHandle>,
    _subscriptions: [Subscription; 4],
    _chat_wiring: [Subscription; 2],
}

impl Session {
    pub(crate) fn new(started: Started, window: &mut Window, cx: &mut Context<'_, Root>) -> Self {
        let shared: SharedCore = Rc::new(RefCell::new(started.commands));
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
        let pump = spawn_pump(started.events, window, cx);
        let shell = ChatShell::default();
        let chat_wiring = super::chats::wire(shell.rows(), window, cx);
        let session = Self {
            core: shared,
            auth: AuthView::Connecting,
            welcomed: false,
            welcome_focus: cx.focus_handle(),
            needs_params: false,
            phone,
            code,
            password,
            composer,
            shell,
            conn: None,
            notice: None,
            notice_gen: 0,
            closing: Closing::Open,
            _pump: pump,
            #[cfg(feature = "fake")]
            fake: started.fake,
            _subscriptions: subscriptions,
            _chat_wiring: chat_wiring,
        };
        session.focus_for_auth(window, cx);
        session
    }

    /// Queues a command. A full queue shows "busy"; the rejected command is
    /// dropped (a password in it is zeroized on drop).
    pub(super) fn send(&mut self, cmd: Command, window: &mut Window, cx: &mut Context<'_, Root>) {
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

    /// Leaves the Welcome screen: the phone step shows and takes focus.
    pub(super) fn accept_welcome(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.welcomed = true;
        self.focus_for_auth(window, cx);
        cx.notify();
    }

    fn focus_for_auth(&self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let handle = match self.auth {
            AuthView::Phone { .. } if !self.welcomed => self.welcome_focus.clone(),
            AuthView::Phone { .. } => self.phone.focus_handle(cx),
            AuthView::Code { .. } => self.code.focus_handle(cx),
            AuthView::Password { .. } => self.password.focus_handle(cx),
            AuthView::SignedIn if self.shell.selected().is_some() => self.composer.focus_handle(cx),
            AuthView::Connecting | AuthView::FloodWait { .. } | AuthView::SignedIn => return,
        };
        window.focus(&handle, cx);
    }

    pub(super) fn submit_phone(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Phone { .. }) {
            return;
        }
        let phone = self.phone.read(cx).text().to_owned();
        self.auth = self.auth.cleared();
        self.send(Command::SubmitPhone(phone), window, cx);
        cx.notify();
    }

    pub(super) fn submit_code(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Code { .. }) {
            return;
        }
        let code = self
            .code
            .update(cx, |input, cx| input.take_text(window, cx));
        self.auth = self.auth.cleared();
        self.send(Command::SubmitCode(code), window, cx);
        cx.notify();
    }

    /// Sends the password, or, when core has no password token, asks for new
    /// parameters and leaves the typed password in place for the next Enter.
    pub(super) fn submit_password(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if !matches!(self.auth, AuthView::Password { .. }) {
            return;
        }
        self.auth = self.auth.cleared();
        if std::mem::take(&mut self.needs_params) {
            self.send(Command::RetryPasswordParams, window, cx);
            cx.notify();
            return;
        }
        let secret = self.password.update(cx, PasswordInput::take_secret);
        let buf = PasswordBuf::from_slice(&secret);
        drop(secret);
        if let Ok(buf) = buf {
            self.send(Command::SubmitPassword(buf), window, cx);
        } else if let Some(next) = self
            .auth
            .with_error(UserError::InvalidPassword, Instant::now())
        {
            // Longer than any password Telegram accepts.
            self.set_auth(next, window, cx);
        }
        cx.notify();
    }

    pub(super) fn apply_event(
        &mut self,
        event: Event,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        match event {
            Event::Auth(state) => self.on_auth(state, window, cx),
            Event::Connection(state) => self.conn = Some(state),
            Event::Error(error) => self.on_error(error, window, cx),
            // Chat list, message view and sends: `shell::chats`.
            chat => self.apply_chat_event(chat, window, cx),
        }
    }

    fn on_auth(&mut self, state: AuthState, window: &mut Window, cx: &mut Context<'_, Root>) {
        let hint = match &self.auth {
            AuthView::Password { hint, .. } => hint.clone(),
            _ => None,
        };
        #[cfg(feature = "fake")]
        let fake_code = self.fake.is_some() && matches!(state, AuthState::Code { error: None, .. });
        let projection = project(state, hint, Instant::now());
        self.needs_params = projection.needs_params;
        self.set_auth(projection.view, window, cx);
        if let Some(notice) = projection.notice {
            self.show_notice(notice, window, cx);
        }
        #[cfg(feature = "fake")]
        if fake_code {
            self.show_notice(copy::FAKE_CODE_HINT.into(), window, cx);
        }
    }

    fn on_error(&mut self, error: UserError, window: &mut Window, cx: &mut Context<'_, Root>) {
        tracing::info!(?error, "core reported an error");
        self.shell.load_failed();
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
