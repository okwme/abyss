//! A running core and the UI state built from its events.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use abyss_core::api::{
    AuthState, Command, ConnState, CredentialSource, Event, PasswordBuf, UserError,
};
use abyss_core::{BackendChoice, CoreConfig, SendError, StartError};
use gpui::{AppContext, Context, Entity, Focusable, SharedString, Subscription, Task, Window};

use super::Root;
use super::auth::{AuthView, project};
use super::bridge::{CoreLink, EventSource, SharedCore, spawn_pump};
use super::shutdown::{self, Closing, Next};
use crate::chat::ChatShell;
use crate::launch::Launch;
use crate::ui::copy;
use crate::ui::password::{PasswordEvent, PasswordInput};
use crate::ui::text_input::{Limits, Mask, TextInput, TextInputEvent, text_input};

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
        Launch::Fake { faults, start } => {
            use abyss_core::backend::fake::{FakeBackend, FakeScript};
            let mut script = crate::chat::demo::script(crate::chat::format::now());
            if start == crate::launch::FakeStart::Onboarding {
                // No credentials: core starts from `NeedCredentials`.
                script.env = abyss_core::credentials::EnvResolution::Absent;
            }
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
        Launch::Telegram(env) => {
            let core = abyss_core::start(CoreConfig::default(), BackendChoice::Grammers(env))?;
            Ok(Started::new(core))
        }
    }
}

pub(crate) struct Session {
    pub(super) core: SharedCore,
    pub(super) auth: AuthView,
    /// Core said the password token is gone: the next submission asks for new
    /// parameters first.
    needs_params: bool,
    pub(super) phone: Entity<TextInput>,
    pub(super) code: Entity<TextInput>,
    pub(super) password: Entity<PasswordInput>,
    /// Credential entry (plan section 17.3).
    pub(super) api_id: Entity<TextInput>,
    pub(super) api_hash: Entity<TextInput>,
    /// Inline validation of the credential entry.
    pub(super) entry_error: Option<SharedString>,
    /// Which credential source is in control (core's `CredentialSource`).
    pub(super) source: Option<CredentialSource>,
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
    _subscriptions: [Subscription; 6],
    _chat_wiring: [Subscription; 2],
}

impl Session {
    pub(crate) fn new(started: Started, window: &mut Window, cx: &mut Context<'_, Root>) -> Self {
        let shared: SharedCore = Rc::new(RefCell::new(started.commands));
        let phone = text_input(Limits::PHONE, copy::PHONE_PLACEHOLDER, cx);
        let code = text_input(Limits::CODE, copy::CODE_PLACEHOLDER, cx);
        let composer = text_input(Limits::COMPOSER, copy::COMPOSER_PLACEHOLDER, cx);
        let password = cx.new(|cx| PasswordInput::new(window, cx));
        let api_id = text_input(Limits::API_ID, copy::onboarding::API_ID_PLACEHOLDER, cx);
        let api_hash = text_input(Limits::API_HASH, copy::onboarding::API_HASH_PLACEHOLDER, cx);
        api_hash.update(cx, |input, cx| input.set_mask(Mask::Hidden, cx));
        let [id_sub, hash_sub] = [&api_id, &api_hash].map(|input| {
            cx.subscribe_in(input, window, |root, _, _: &TextInputEvent, window, cx| {
                root.with_session(|s, cx| s.submit_credentials(window, cx), cx);
            })
        });
        let subscriptions = [
            id_sub,
            hash_sub,
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
            needs_params: false,
            phone,
            code,
            password,
            api_id,
            api_hash,
            entry_error: None,
            source: None,
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
        let ticking = next.ticking_until().is_some() && self.auth.ticking_until().is_none();
        self.auth = next;
        if ticking {
            spawn_ticker(window, cx);
        }
        self.focus_for_auth(window, cx);
        cx.notify();
    }

    /// Called every second while a countdown runs (a flood wait, a code
    /// resend). Returns whether to keep ticking.
    pub(super) fn tick(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) -> bool {
        let Some(until) = self.auth.ticking_until() else {
            return false;
        };
        if Instant::now() < until {
            cx.notify();
            return true;
        }
        if let AuthView::FloodWait { resume, .. } = &self.auth {
            let resume = (**resume).clone();
            self.set_auth(resume, window, cx);
        }
        cx.notify();
        false
    }

    fn focus_for_auth(&self, window: &mut Window, cx: &mut Context<'_, Root>) {
        use super::auth::Onboarding;
        let handle = match self.auth {
            AuthView::Onboarding(Onboarding::Entry { .. }) => self.api_id.focus_handle(cx),
            AuthView::Phone { .. } => self.phone.focus_handle(cx),
            AuthView::Code { .. } => self.code.focus_handle(cx),
            AuthView::Password { .. } => self.password.focus_handle(cx),
            AuthView::SignedIn if self.shell.selected().is_some() => self.composer.focus_handle(cx),
            _ => return,
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
            Event::CredentialSource(source) => self.source = source,
            Event::Connection(state) => self.conn = Some(state),
            Event::Error(error) => self.on_error(error, window, cx),
            // Chat list, message view and sends: `shell::chats`.
            chat => self.apply_chat_event(chat, window, cx),
        }
    }

    fn on_auth(&mut self, state: AuthState, window: &mut Window, cx: &mut Context<'_, Root>) {
        #[cfg(feature = "fake")]
        let fake_code = self.fake.is_some() && matches!(state, AuthState::Code { error: None, .. });
        let projection = project(state, &self.auth, Instant::now());
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
                Next::ShowLogoutUnfinished => {
                    root.with_session(|s, _| s.closing = Closing::LogoutUnfinished, cx);
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

/// Re-renders a countdown every second until it ends.
fn spawn_ticker(window: &mut Window, cx: &mut Context<'_, Root>) {
    cx.spawn_in(window, async move |this, cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let more = this.update_in(cx, |root, window, cx| {
                root.with_session(|s, cx| s.tick(window, cx), cx)
            });
            if more.ok().flatten() != Some(true) {
                return;
            }
        }
    })
    .detach();
}
