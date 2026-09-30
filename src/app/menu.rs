use super::shortcuts::{Action, Bindings};
use crate::browser::filter::PhotoFilter;
use muda::{
    AboutMetadata, CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use std::cell::{Cell, RefCell};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Open,
    RefreshFolder,
    OpenDeleted,
    Quit,
    Fit,
    Fill,
    Actual,
    Fullscreen,
    ZoomLock,
    Brighten,
    Darken,
    ResetBrightness,
    AutoAdvance,
    Pin,
    ReplacePin,
    UndoRating,
    Filmstrip,
    PhotoDetails,
    Filter(PhotoFilter),
    MoveDeletedCurrent,
    MoveDeletedRejected,
    TrashCurrent,
    TrashRejected,
    Help,
    CustomizeShortcuts,
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
    photo_details: CheckMenuItem,
    last_workflow: Cell<Option<[bool; 5]>>,
    shortcut_items: Vec<(MenuItem, Action, &'static str)>,
    shortcut_checks: Vec<(CheckMenuItem, Action, &'static str)>,
    pin_shortcuts: RefCell<String>,
}

impl AppMenu {
    pub fn new(bindings: &Bindings) -> Result<Self, String> {
        let menu = Menu::new();
        let application = Submenu::new("ARW Sprint", true);
        let file = Submenu::new("File", true);
        let edit = Submenu::new("Edit", true);
        let view = Submenu::new("View", true);
        let filter_menu = Submenu::new("Filter", true);
        let window = Submenu::new("Window", true);
        let help = Submenu::new("Help", true);
        application.append_items(&[
            &PredefinedMenuItem::about(None, Some(AboutMetadata {
                name: Some("ARW Sprint".into()), version: Some(env!("CARGO_PKG_VERSION").into()),
                comments: Some("Fast Sony ARW culling for Intel Macs. Embedded JPEG previews; original RAW bytes are never edited.".into()),
                ..Default::default()
            })),
            &PredefinedMenuItem::separator(), &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::hide(None), &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None), &PredefinedMenuItem::separator(),
        ]).map_err(|e| e.to_string())?;
        let mut items = Vec::new();
        let mut shortcut_items = Vec::new();
        let mut shortcut_checks = Vec::new();
        // App shortcuts are routed once through the configurable winit map.
        // Fixed Cocoa accelerators would intercept removed/remapped bindings.
        let mut add =
            |parent: &Submenu, text: &'static str, command, action| -> Result<(), String> {
                let item = MenuItem::new(shortcut_label(text, &bindings.keys(action)), true, None);
                items.push((item.id().clone(), command));
                parent.append(&item).map_err(|e| e.to_string())?;
                shortcut_items.push((item, action, text));
                Ok(())
            };
        add(
            &application,
            "Customize Shortcuts…",
            Command::CustomizeShortcuts,
            Action::CustomizeShortcuts,
        )?;
        application
            .append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        add(&application, "Quit ARW Sprint", Command::Quit, Action::Quit)?;
        add(&file, "Open Folder…", Command::Open, Action::Open)?;
        add(
            &file,
            "Refresh Folder",
            Command::RefreshFolder,
            Action::RefreshFolder,
        )?;
        add(
            &file,
            "Open _Rejected Folder",
            Command::OpenDeleted,
            Action::OpenRejected,
        )?;
        file.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        add(
            &file,
            "Move Current Photo to _Rejected",
            Command::MoveDeletedCurrent,
            Action::MoveRejected,
        )?;
        add(
            &file,
            "Move Rejected Photos to _Rejected…",
            Command::MoveDeletedRejected,
            Action::MoveRejectedBatch,
        )?;
        file.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        add(
            &file,
            "Move Current Rejected Photo to Trash…",
            Command::TrashCurrent,
            Action::TrashCurrent,
        )?;
        add(
            &file,
            "Move Rejected Photos to Trash…",
            Command::TrashRejected,
            Action::TrashRejected,
        )?;
        add(&view, "Fit", Command::Fit, Action::Fit)?;
        add(&view, "Fill", Command::Fill, Action::Fill)?;
        add(&view, "Actual Size (100%)", Command::Actual, Action::Actual)?;
        add(
            &view,
            "Toggle Fullscreen",
            Command::Fullscreen,
            Action::Fullscreen,
        )?;
        view.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        add(
            &view,
            "Increase Viewing Brightness",
            Command::Brighten,
            Action::Brighten,
        )?;
        add(
            &view,
            "Decrease Viewing Brightness",
            Command::Darken,
            Action::Darken,
        )?;
        add(
            &view,
            "Reset Viewing Brightness",
            Command::ResetBrightness,
            Action::ResetBrightness,
        )?;
        add(&help, "Keyboard Shortcuts", Command::Help, Action::Help)?;
        add(
            &help,
            "Customize Shortcuts…",
            Command::CustomizeShortcuts,
            Action::CustomizeShortcuts,
        )?;
        let undo = MenuItem::new(
            shortcut_label("Undo Rating / Move", &bindings.keys(Action::Undo)),
            false,
            None,
        );
        items.push((undo.id().clone(), Command::UndoRating));
        edit.append(&undo).map_err(|e| e.to_string())?;
        shortcut_items.push((undo.clone(), Action::Undo, "Undo Rating / Move"));
        let auto_advance = CheckMenuItem::new(
            shortcut_label(
                "Auto-Advance After Rating",
                &bindings.keys(Action::AutoAdvance),
            ),
            true,
            false,
            None,
        );
        items.push((auto_advance.id().clone(), Command::AutoAdvance));
        edit.append(&auto_advance).map_err(|e| e.to_string())?;
        shortcut_checks.push((
            auto_advance.clone(),
            Action::AutoAdvance,
            "Auto-Advance After Rating",
        ));
        let zoom_lock = CheckMenuItem::new(
            shortcut_label("Keep Zoom and Position", &bindings.keys(Action::ZoomLock)),
            true,
            false,
            None,
        );
        items.push((zoom_lock.id().clone(), Command::ZoomLock));
        view.append_items(&[&PredefinedMenuItem::separator(), &zoom_lock])
            .map_err(|e| e.to_string())?;
        shortcut_checks.push((
            zoom_lock.clone(),
            Action::ZoomLock,
            "Keep Zoom and Position",
        ));
        let pin = MenuItem::new(
            shortcut_label(
                "Pin Current Photo for Comparison",
                &bindings.keys(Action::Pin),
            ),
            false,
            None,
        );
        items.push((pin.id().clone(), Command::Pin));
        view.append(&pin).map_err(|e| e.to_string())?;
        let replace_pin = MenuItem::new(
            shortcut_label(
                "Replace Reference with Current Photo",
                &bindings.keys(Action::ReplacePin),
            ),
            true,
            None,
        );
        items.push((replace_pin.id().clone(), Command::ReplacePin));
        view.append(&replace_pin).map_err(|e| e.to_string())?;
        shortcut_items.push((
            replace_pin,
            Action::ReplacePin,
            "Replace Reference with Current Photo",
        ));
        let filmstrip = CheckMenuItem::new(
            shortcut_label("Show Filmstrip", &bindings.keys(Action::Filmstrip)),
            true,
            false,
            None,
        );
        items.push((filmstrip.id().clone(), Command::Filmstrip));
        view.append(&filmstrip).map_err(|e| e.to_string())?;
        shortcut_checks.push((filmstrip.clone(), Action::Filmstrip, "Show Filmstrip"));
        let photo_details = CheckMenuItem::new(
            shortcut_label("Photo Details (EXIF)", &bindings.keys(Action::PhotoDetails)),
            true,
            false,
            None,
        );
        items.push((photo_details.id().clone(), Command::PhotoDetails));
        view.append(&photo_details).map_err(|e| e.to_string())?;
        shortcut_checks.push((
            photo_details.clone(),
            Action::PhotoDetails,
            "Photo Details (EXIF)",
        ));
        let mut filters = Vec::new();
        for (filter, action) in [
            (PhotoFilter::All, Action::FilterAll),
            (PhotoFilter::Stars(1), Action::Filter1),
            (PhotoFilter::Stars(2), Action::Filter2),
            (PhotoFilter::Stars(3), Action::Filter3),
            (PhotoFilter::Stars(4), Action::Filter4),
            (PhotoFilter::Stars(5), Action::Filter5),
            (PhotoFilter::Rated, Action::FilterRated),
            (PhotoFilter::Unrated, Action::FilterUnrated),
            (PhotoFilter::Rejected, Action::FilterRejected),
            (PhotoFilter::NotRejected, Action::FilterNotRejected),
        ] {
            let item = CheckMenuItem::new(
                shortcut_label(filter.label(), &bindings.keys(action)),
                true,
                filter == PhotoFilter::All,
                None,
            );
            items.push((item.id().clone(), Command::Filter(filter)));
            filter_menu.append(&item).map_err(|e| e.to_string())?;
            shortcut_checks.push((item.clone(), action, filter.label()));
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
            photo_details,
            last_workflow: Cell::new(None),
            shortcut_items,
            shortcut_checks,
            pin_shortcuts: RefCell::new(bindings.keys(Action::Pin)),
        })
    }

    pub fn set_shortcuts(&self, bindings: &Bindings) {
        for (item, action, label) in &self.shortcut_items {
            item.set_text(shortcut_label(label, &bindings.keys(*action)));
        }
        for (item, action, label) in &self.shortcut_checks {
            item.set_text(shortcut_label(label, &bindings.keys(*action)));
        }
        *self.pin_shortcuts.borrow_mut() = bindings.keys(Action::Pin);
        self.update_pin_label(self.last_workflow.get().is_some_and(|state| state[2]));
    }

    fn update_pin_label(&self, pinned: bool) {
        self.pin.set_text(shortcut_label(
            if pinned {
                "Close Comparison"
            } else {
                "Pin Current Photo for Comparison"
            },
            &self.pin_shortcuts.borrow(),
        ));
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

    pub fn set_photo_details_visible(&self, visible: bool) {
        self.photo_details.set_checked(visible);
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
        self.update_pin_label(pinned);
        self.undo.set_enabled(can_undo);
    }
}

fn shortcut_label(label: &str, keys: &str) -> String {
    if keys.is_empty() {
        label.to_owned()
    } else {
        format!("{label} ({keys})")
    }
}
