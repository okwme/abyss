//! Archiving and chat folder editing (folders plan, part C): a small menu
//! opened by right-clicking a chat row or a folder tab. Every choice is one
//! core command; the list changes only when core reports the change
//! (`ChatBumped`, `ChatFolders`), never ahead of it.

use abyss_core::api::Command;
use abyss_core::model::{ChatFolder, ChatId, FolderEdit, PeerFolder};
use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, Focusable, MouseButton, MouseDownEvent, Pixels,
    Point, SharedString, Stateful, Subscription, Window, div, point, prelude::*, px,
};

use super::Root;
use super::session::Session;
use super::view::field;
use crate::chat::copy;
use crate::ui::text_input::{Limits, TextInput, TextInputEvent, text_input};
use crate::ui::theme::{self, Depth, Theme};

/// Width of the menu.
const MENU_WIDTH: f32 = 240.;
/// Height of one menu entry, for keeping the menu inside the window.
const ENTRY_HEIGHT: f32 = 30.;

/// What the open menu is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Menu {
    /// A chat row's menu. `naming`: the "New folder" entry became a title
    /// field.
    Chat {
        chat: ChatId,
        at: Point<Pixels>,
        naming: Naming,
    },
    /// A folder tab's menu. `confirm`: Delete was clicked once.
    Folder {
        id: i32,
        at: Point<Pixels>,
        confirm: Confirm,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Naming {
    No,
    Yes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Confirm {
    No,
    Pending,
}

/// The folders a chat menu can change: regular ones (shared folders are
/// read-only here).
fn editable(folders: &[ChatFolder]) -> impl Iterator<Item = &ChatFolder> {
    folders.iter().filter(|f| !f.shared)
}

/// Where a menu of `entries` rows opened at `at` fits in the window.
fn placed(at: Point<Pixels>, entries: usize, window: &Window) -> Point<Pixels> {
    let size = window.viewport_size();
    let rows = f32::from(u16::try_from(entries).unwrap_or(u16::MAX));
    let height = px(rows * ENTRY_HEIGHT + 16.);
    let x = at.x.min(size.width - px(MENU_WIDTH)).max(px(0.));
    let y = at.y.min(size.height - height).max(px(0.));
    point(x, y)
}

/// The new-folder title field and its Enter subscription.
pub(super) fn title_field(
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> (Entity<TextInput>, Subscription) {
    let field = text_input(Limits::FOLDER_TITLE, copy::FOLDER_NAME_PLACEHOLDER, cx);
    let sub = cx.subscribe_in(&field, window, |root, _, _: &TextInputEvent, window, cx| {
        root.with_session(|s, cx| s.submit_folder_name(window, cx), cx);
    });
    (field, sub)
}

impl Session {
    /// Right-click on a chat row.
    pub(super) fn open_chat_menu(
        &mut self,
        chat: ChatId,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let entries = 2 + editable(self.shell.list.folders()).count();
        self.menu = Some(Menu::Chat {
            chat,
            at: placed(at, entries, window),
            naming: Naming::No,
        });
        cx.notify();
    }

    /// Right-click on a folder tab.
    pub(super) fn open_folder_menu(
        &mut self,
        id: i32,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        let shared = self
            .shell
            .list
            .folders()
            .iter()
            .any(|f| f.id == id && f.shared);
        if shared {
            self.show_notice(copy::SHARED_FOLDER.into(), window, cx);
            return;
        }
        self.menu = Some(Menu::Folder {
            id,
            at: placed(at, 1, window),
            confirm: Confirm::No,
        });
        cx.notify();
    }

    pub(super) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if self.menu.take().is_some() {
            self.folder_name
                .update(cx, |input, cx| input.clear(window, cx));
            cx.notify();
        }
    }

    /// Sends a folder command and closes the menu. A refusal (core busy or
    /// gone) is shown; the outcome arrives as an event.
    fn menu_command(&mut self, cmd: Command, window: &mut Window, cx: &mut Context<'_, Root>) {
        self.close_menu(window, cx);
        self.send(cmd, window, cx);
    }

    pub(super) fn set_peer_folder(
        &mut self,
        chat: ChatId,
        folder: PeerFolder,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.menu_command(Command::SetPeerFolder { chat, folder }, window, cx);
    }

    pub(super) fn edit_folder(
        &mut self,
        edit: FolderEdit,
        window: &mut Window,
        cx: &mut Context<'_, Root>,
    ) {
        self.menu_command(Command::EditChatFolder(edit), window, cx);
    }

    /// "New folder with this chat…": the entry turns into a title field.
    pub(super) fn start_naming(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        if let Some(Menu::Chat { naming, .. }) = &mut self.menu {
            *naming = Naming::Yes;
            window.focus(&self.folder_name.focus_handle(cx), cx);
            cx.notify();
        }
    }

    /// Enter in the title field: creates the folder with the menu's chat.
    pub(super) fn submit_folder_name(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        let Some(Menu::Chat {
            chat,
            naming: Naming::Yes,
            ..
        }) = self.menu
        else {
            return;
        };
        let title = self.folder_name.read(cx).text().to_owned();
        if abyss_core::model::folder_title(&title).is_none() {
            self.show_notice(copy::FOLDER_TITLE_INVALID.into(), window, cx);
            return;
        }
        self.edit_folder(FolderEdit::Create { title, chat }, window, cx);
    }

    /// Delete on a folder tab: the first click asks, the second deletes.
    pub(super) fn delete_folder(&mut self, window: &mut Window, cx: &mut Context<'_, Root>) {
        match self.menu {
            Some(Menu::Folder {
                id,
                confirm: Confirm::Pending,
                ..
            }) => self.edit_folder(FolderEdit::Delete { folder: id }, window, cx),
            Some(Menu::Folder {
                ref mut confirm, ..
            }) => {
                *confirm = Confirm::Pending;
                cx.notify();
            }
            _ => {}
        }
    }
}

/// One clickable menu entry.
fn entry(
    id: (&'static str, usize),
    label: impl IntoElement,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
    on_click: impl Fn(&mut Session, &mut Window, &mut Context<'_, Root>) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(ENTRY_HEIGHT))
        .px_3()
        .flex()
        .items_center()
        .rounded(px(theme::RADIUS))
        .cursor_pointer()
        .hover(|d| d.bg(theme::faded(theme.text, 0.08)))
        .child(label)
        .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| on_click(s, window, cx), cx);
        }))
}

/// The open menu over everything, with a backdrop that closes it.
pub(super) fn menu_overlay(
    session: &Session,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Option<AnyElement> {
    let menu = session.menu?;
    let (at, entries) = match menu {
        Menu::Chat { chat, at, naming } => (at, chat_entries(session, chat, naming, theme, cx)?),
        Menu::Folder { id, at, confirm } => (at, folder_entries(session, id, confirm, theme, cx)?),
    };
    let close =
        |root: &mut Root, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<'_, Root>| {
            root.with_session(|s, cx| s.close_menu(window, cx), cx);
        };
    let backdrop = div()
        .id("menu-backdrop")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .on_mouse_down(MouseButton::Left, cx.listener(close))
        .on_mouse_down(MouseButton::Right, cx.listener(close));
    let panel = div()
        .id("menu")
        .absolute()
        .left(at.x)
        .top(at.y)
        .w(px(MENU_WIDTH))
        .p_1()
        .flex()
        .flex_col()
        .text_sm()
        .bg(theme.surface(Depth::NOW))
        .text_color(theme.text)
        .border_1()
        .border_color(theme::line(theme.text))
        .rounded(px(theme::RADIUS))
        .shadow_md()
        // Clicks inside the menu never reach the backdrop.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .children(entries);
    Some(
        // Inset from the corner: without it, the static position would put
        // the overlay below the full-height screen.
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(backdrop)
            .child(panel)
            .into_any_element(),
    )
}

/// Archive or Unarchive, a toggle per editable folder, and New folder.
fn chat_entries(
    session: &Session,
    chat: ChatId,
    naming: Naming,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Option<Vec<AnyElement>> {
    let summary = session.shell.list.get(chat)?;
    let (label, target) = match summary.folder {
        PeerFolder::Main => (copy::ARCHIVE_CHAT, PeerFolder::Archive),
        PeerFolder::Archive => (copy::UNARCHIVE_CHAT, PeerFolder::Main),
    };
    let mut out = vec![
        entry(("menu-archive", 0), label, theme, cx, move |s, w, cx| {
            s.set_peer_folder(chat, target, w, cx);
        })
        .into_any_element(),
    ];
    for (ix, folder) in editable(session.shell.list.folders()).enumerate() {
        let id = folder.id;
        let inside = folder.contains(summary);
        let label = div()
            .flex()
            .child(div().w(px(18.)).flex_none().children(inside.then_some("✓")))
            .child(folder.title.clone());
        let item = entry(("menu-folder", ix), label, theme, cx, move |s, w, cx| {
            let edit = if inside {
                FolderEdit::RemoveChat { folder: id, chat }
            } else {
                FolderEdit::AddChat { folder: id, chat }
            };
            s.edit_folder(edit, w, cx);
        });
        out.push(item.into_any_element());
    }
    let last = match naming {
        Naming::No => entry(("menu-new", 0), copy::NEW_FOLDER, theme, cx, |s, w, cx| {
            s.start_naming(w, cx);
        })
        .into_any_element(),
        Naming::Yes => div()
            .p_1()
            .child(field(session.folder_name.clone().into_any_element(), theme))
            .into_any_element(),
    };
    out.push(last);
    Some(out)
}

/// Delete, confirmed by a second click.
fn folder_entries(
    session: &Session,
    id: i32,
    confirm: Confirm,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Option<Vec<AnyElement>> {
    let folder = session.shell.list.folders().iter().find(|f| f.id == id)?;
    let label: SharedString = match confirm {
        Confirm::No => copy::delete_folder(&folder.title),
        Confirm::Pending => copy::confirm_delete_folder(&folder.title),
    };
    Some(vec![
        entry(("menu-delete", 0), label, theme, cx, |s, w, cx| {
            s.delete_folder(w, cx);
        })
        .into_any_element(),
    ])
}
