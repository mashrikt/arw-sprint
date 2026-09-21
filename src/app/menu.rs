use crate::browser::filter::PhotoFilter;
use muda::{
    accelerator::{Accelerator, Code, Modifiers},
    AboutMetadata, CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Open,
    OpenDeleted,
    Quit,
    Fit,
    Fill,
    Actual,
    Fullscreen,
    ZoomLock,
    AutoAdvance,
    Pin,
    ReplacePin,
    UndoRating,
    Filmstrip,
    Filter(PhotoFilter),
    MoveDeletedCurrent,
    MoveDeletedRejected,
    TrashCurrent,
    TrashRejected,
    Help,
}

pub struct AppMenu {
    _menu: Menu,
    items: Vec<(MenuId, Command)>,
    filters: Vec<(PhotoFilter, CheckMenuItem)>,
    zoom_lock: CheckMenuItem,
    auto_advance: CheckMenuItem,
    pin: MenuItem,
    undo: MenuItem,
    filmstrip: CheckMenuItem,
    last_workflow: Cell<Option<[bool; 5]>>,
}

impl AppMenu {
    pub fn new() -> Result<Self, String> {
        let menu = Menu::new();
        let application = Submenu::new("FastCull", true);
        let file = Submenu::new("File", true);
        let edit = Submenu::new("Edit", true);
        let view = Submenu::new("View", true);
        let filter_menu = Submenu::new("Filter", true);
        let window = Submenu::new("Window", true);
        let help = Submenu::new("Help", true);
        application.append_items(&[
            &PredefinedMenuItem::about(None, Some(AboutMetadata {
                name: Some("FastCull".into()), version: Some(env!("CARGO_PKG_VERSION").into()),
                comments: Some("Fast Sony ARW culling for Intel Macs. Embedded JPEG previews; original RAW bytes are never edited.".into()),
                ..Default::default()
            })),
            &PredefinedMenuItem::separator(), &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::hide(None), &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None), &PredefinedMenuItem::separator(),
        ]).map_err(|e| e.to_string())?;
        let mut items = Vec::new();
        let mut add =
            |parent: &Submenu, text: &str, command, key: Option<Code>| -> Result<(), String> {
                let item = MenuItem::new(
                    text,
                    true,
                    key.map(|key| Accelerator::new(Some(Modifiers::SUPER), key)),
                );
                items.push((item.id().clone(), command));
                parent.append(&item).map_err(|e| e.to_string())
            };
        add(
            &application,
            "Quit FastCull",
            Command::Quit,
            Some(Code::KeyQ),
        )?;
        add(&file, "Open Folder…", Command::Open, Some(Code::KeyO))?;
        add(&file, "Open deleted Folder", Command::OpenDeleted, None)?;
        file.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        add(
            &file,
            "Move Current Photo to deleted (Space / X)",
            Command::MoveDeletedCurrent,
            None,
        )?;
        add(
            &file,
            "Move Rejected Photos to deleted…",
            Command::MoveDeletedRejected,
            None,
        )?;
        file.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        add(
            &file,
            "Move Current Rejected Photo to Trash…",
            Command::TrashCurrent,
            Some(Code::Backspace),
        )?;
        add(
            &file,
            "Move Rejected Photos to Trash…",
            Command::TrashRejected,
            None,
        )?;
        add(&view, "Fit", Command::Fit, None)?;
        add(&view, "Fill", Command::Fill, None)?;
        add(&view, "Actual Size (100%)", Command::Actual, None)?;
        add(&view, "Toggle Fullscreen", Command::Fullscreen, None)?;
        add(&help, "Keyboard Shortcuts", Command::Help, None)?;
        let undo = MenuItem::new(
            "Undo Rating / Move",
            false,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyZ)),
        );
        items.push((undo.id().clone(), Command::UndoRating));
        edit.append(&undo).map_err(|e| e.to_string())?;
        let auto_advance = CheckMenuItem::new("Auto-Advance After Rating (A)", true, false, None);
        items.push((auto_advance.id().clone(), Command::AutoAdvance));
        edit.append(&auto_advance).map_err(|e| e.to_string())?;
        let zoom_lock = CheckMenuItem::new("Keep Zoom and Position (L)", true, false, None);
        items.push((zoom_lock.id().clone(), Command::ZoomLock));
        view.append_items(&[&PredefinedMenuItem::separator(), &zoom_lock])
            .map_err(|e| e.to_string())?;
        let pin = MenuItem::new("Pin Current Photo for Comparison (C)", false, None);
        items.push((pin.id().clone(), Command::Pin));
        view.append(&pin).map_err(|e| e.to_string())?;
        let replace_pin =
            MenuItem::new("Replace Reference with Current Photo (Shift+C)", true, None);
        items.push((replace_pin.id().clone(), Command::ReplacePin));
        view.append(&replace_pin).map_err(|e| e.to_string())?;
        let filmstrip = CheckMenuItem::new("Show Filmstrip (Tab)", true, false, None);
        items.push((filmstrip.id().clone(), Command::Filmstrip));
        view.append(&filmstrip).map_err(|e| e.to_string())?;
        let mut filters = Vec::new();
        for (filter, key) in [
            (PhotoFilter::All, Some(Code::Digit0)),
            (PhotoFilter::Stars(1), Some(Code::Digit1)),
            (PhotoFilter::Stars(2), Some(Code::Digit2)),
            (PhotoFilter::Stars(3), Some(Code::Digit3)),
            (PhotoFilter::Stars(4), Some(Code::Digit4)),
            (PhotoFilter::Stars(5), Some(Code::Digit5)),
            (PhotoFilter::Rated, None),
            (PhotoFilter::Unrated, None),
            (PhotoFilter::Rejected, Some(Code::KeyX)),
            (PhotoFilter::NotRejected, None),
        ] {
            let item = CheckMenuItem::new(
                filter.label(),
                true,
                filter == PhotoFilter::All,
                key.map(|key| Accelerator::new(Some(Modifiers::SUPER | Modifiers::ALT), key)),
            );
            items.push((item.id().clone(), Command::Filter(filter)));
            filter_menu.append(&item).map_err(|e| e.to_string())?;
            filters.push((filter, item));
        }
        window
            .append_items(&[
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::maximize(None),
            ])
            .map_err(|e| e.to_string())?;
        menu.append_items(&[
            &application,
            &file,
            &edit,
            &view,
            &filter_menu,
            &window,
            &help,
        ])
        .map_err(|e| e.to_string())?;
        menu.init_for_nsapp();
        window.set_as_windows_menu_for_nsapp();
        Ok(Self {
            _menu: menu,
            items,
            filters,
            zoom_lock,
            auto_advance,
            pin,
            undo,
            filmstrip,
            last_workflow: Cell::new(None),
        })
    }

    pub fn set_filter(&self, selected: PhotoFilter) {
        for (filter, item) in &self.filters {
            item.set_checked(*filter == selected);
        }
    }

    pub fn command(&self, event: &MenuEvent) -> Option<Command> {
        self.items
            .iter()
            .find(|(id, _)| *id == event.id)
            .map(|(_, command)| *command)
    }

    pub fn set_filmstrip(&self, visible: bool) {
        self.filmstrip.set_checked(visible);
    }

    pub fn set_workflow(
        &self,
        zoom_locked: bool,
        auto_advance: bool,
        pinned: bool,
        can_pin: bool,
        can_undo: bool,
    ) {
        let state = [zoom_locked, auto_advance, pinned, can_pin, can_undo];
        if self.last_workflow.replace(Some(state)) == Some(state) {
            return;
        }
        self.zoom_lock.set_checked(zoom_locked);
        self.auto_advance.set_checked(auto_advance);
        self.pin.set_enabled(pinned || can_pin);
        self.pin.set_text(if pinned {
            "Close Comparison (C)"
        } else {
            "Pin Current Photo for Comparison (C)"
        });
        self.undo.set_enabled(can_undo);
    }
}
