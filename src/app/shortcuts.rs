//! Validated shortcut routing, with no I/O or allocation during key dispatch.
//!
//! Use `KeyEventExtModifierSupplement::key_without_modifiers` with current
//! modifiers so Option does not change the key identity. Shifted punctuation
//! is also accepted for callers providing logical keys.

use std::collections::HashSet;
use winit::keyboard::{Key, ModifiersState, NamedKey};

const MAX_CONFIG_BYTES: usize = 65_536;
const MAX_LINE_BYTES: usize = 512;
const MAX_ALIASES: usize = 8;

macro_rules! actions {
    ($( $variant:ident, $id:literal, $label:literal, [$($key:literal),*], $repeat:literal; )*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(usize)]
        pub(super) enum Action { $( $variant, )* }

        impl Action {
            pub(super) const ALL: &'static [Self] = &[$( Self::$variant, )*];
            pub(super) fn id(self) -> &'static str {
                match self { $( Self::$variant => $id, )* }
            }
            pub(super) fn label(self) -> &'static str {
                match self { $( Self::$variant => $label, )* }
            }
            pub(super) fn defaults(self) -> &'static [&'static str] {
                match self { $( Self::$variant => &[$($key),*], )* }
            }
            pub(super) fn repeatable(self) -> bool {
                match self { $( Self::$variant => $repeat, )* }
            }
        }
    }
}

actions! {
    ViewRaw, "view_raw", "Toggle RAW for current photo", ["R"], false;
    BrowseRaw, "browse_raw", "Toggle RAW browsing (one neighbor ahead)", ["Shift+R"], false;
    Next, "next", "Next photo", ["Right"], true;
    Previous, "previous", "Previous photo", ["Left", "Shift+Space"], true;
    MoveRejected, "move_rejected", "Move current photo to _Rejected", ["Space", "X", "Shift+X"], false;
    ClearRating, "clear_rating", "Clear rating", ["U", "0"], false;
    Rate1, "rate_1", "Assign 1 star", ["1"], false;
    Rate2, "rate_2", "Assign 2 stars", ["2"], false;
    Rate3, "rate_3", "Assign 3 stars", ["3"], false;
    Rate4, "rate_4", "Assign 4 stars", ["4"], false;
    Rate5, "rate_5", "Assign 5 stars", ["5"], false;
    Undo, "undo", "Undo rating or move", ["Cmd+Z"], false;
    AutoAdvance, "auto_advance", "Toggle auto-advance after rating", ["A"], false;
    FilterAll, "filter_all", "Show all photos", ["Cmd+Alt+0"], false;
    Filter1, "filter_1", "Show 1-star photos", ["Cmd+Alt+1"], false;
    Filter2, "filter_2", "Show 2-star photos", ["Cmd+Alt+2"], false;
    Filter3, "filter_3", "Show 3-star photos", ["Cmd+Alt+3"], false;
    Filter4, "filter_4", "Show 4-star photos", ["Cmd+Alt+4"], false;
    Filter5, "filter_5", "Show 5-star photos", ["Cmd+Alt+5"], false;
    FilterRejected, "filter_rejected", "Show saved XMP rejects", ["Cmd+Alt+X"], false;
    FilterRated, "filter_rated", "Show rated photos", [], false;
    FilterUnrated, "filter_unrated", "Show unrated photos", [], false;
    FilterNotRejected, "filter_not_rejected", "Show photos not marked rejected", [], false;
    Fullscreen, "fullscreen", "Toggle fullscreen", ["F"], false;
    FitActual, "fit_actual", "Toggle fit / 100%", ["Z"], false;
    ZoomLock, "zoom_lock", "Keep zoom and position between photos", ["L"], false;
    Peek, "peek", "Hold for temporary 100% peek", ["P"], false;
    ZoomIn, "zoom_in", "Zoom in", ["Equal", "Plus", "S", "Shift+S"], true;
    ZoomOut, "zoom_out", "Zoom out", ["Minus", "Underscore", "D", "Shift+D"], true;
    Brighten, "brighten", "Increase viewing brightness", ["BracketRight"], true;
    Darken, "darken", "Decrease viewing brightness", ["BracketLeft"], true;
    ResetBrightness, "reset_brightness", "Reset viewing brightness", ["Backslash"], false;
    Pin, "pin", "Pin current photo / close comparison", ["C"], false;
    ReplacePin, "replace_pin", "Replace comparison reference", ["Shift+C"], false;
    Escape, "escape", "Leave fullscreen / fit image", ["Escape"], false;
    Open, "open", "Open a folder", ["Cmd+O"], false;
    RefreshFolder, "refresh_folder", "Refresh current folder", ["Cmd+R"], false;
    OpenRejected, "open_rejected", "Open _Rejected folder", [], false;
    Quit, "quit", "Save pending ratings and quit", ["Cmd+Q"], false;
    TrashCurrent, "trash_current", "Confirm Trash for current saved reject", ["Cmd+Backspace"], false;
    TrashRejected, "trash_rejected", "Confirm Trash for all saved rejects", [], false;
    MoveRejectedBatch, "move_rejected_batch", "Move saved rejects to _Rejected", [], false;
    Filmstrip, "filmstrip", "Show / hide filmstrip", ["Tab"], false;
    PhotoDetails, "photo_details", "Show / hide photo details (EXIF)", ["I"], false;
    Fit, "fit", "Fit image to window", [], false;
    Fill, "fill", "Fill image viewport", [], false;
    Actual, "actual", "Actual size (100%)", [], false;
    Help, "help", "Show keyboard shortcuts", [], false;
    CustomizeShortcuts, "customize_shortcuts", "Customize keyboard shortcuts", ["Cmd+Comma"], false;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum KeyId {
    Character(char),
    Space,
    Left,
    Right,
    Up,
    Down,
    Tab,
    Escape,
    Enter,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Function(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Chord {
    key: KeyId,
    modifiers: ModifiersState,
}

impl Chord {
    fn new(key: KeyId, mut modifiers: ModifiersState) -> Self {
        // Detect aliases during validation as well as at runtime.
        let key = match key {
            KeyId::Character('+') => {
                modifiers |= ModifiersState::SHIFT;
                KeyId::Character('=')
            }
            KeyId::Character('_') => {
                modifiers |= ModifiersState::SHIFT;
                KeyId::Character('-')
            }
            KeyId::Character(c) => KeyId::Character(c.to_ascii_lowercase()),
            other => other,
        };
        Self { key, modifiers }
    }

    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() || text.len() > 80 || !text.is_ascii() {
            return Err(
                "Use a named key or one ASCII letter/digit, with optional modifiers".into(),
            );
        }
        let (prefix, key_text) = if text == "+" {
            ("", "+")
        } else if let Some(prefix) = text.strip_suffix("++") {
            (prefix, "+")
        } else {
            text.rsplit_once('+').unwrap_or(("", text))
        };
        let mut modifiers = ModifiersState::empty();
        if !prefix.is_empty() {
            for part in prefix.split('+') {
                let flag = match part.trim().to_ascii_lowercase().as_str() {
                    "cmd" | "command" | "super" => ModifiersState::SUPER,
                    "ctrl" | "control" => ModifiersState::CONTROL,
                    "alt" | "option" => ModifiersState::ALT,
                    "shift" => ModifiersState::SHIFT,
                    _ => return Err(format!("Unknown modifier: {}", part.trim())),
                };
                if modifiers.contains(flag) {
                    return Err(format!("Repeated modifier: {}", part.trim()));
                }
                modifiers |= flag;
            }
        } else if text.starts_with('+') && text != "+" {
            return Err("A shortcut cannot start with an empty modifier".into());
        }
        let key_text = key_text.trim();
        let lower = key_text.to_ascii_lowercase();
        let key = match lower.as_str() {
            "space" => KeyId::Space,
            "left" | "arrowleft" => KeyId::Left,
            "right" | "arrowright" => KeyId::Right,
            "up" | "arrowup" => KeyId::Up,
            "down" | "arrowdown" => KeyId::Down,
            "tab" => KeyId::Tab,
            "escape" | "esc" => KeyId::Escape,
            "enter" | "return" => KeyId::Enter,
            "backspace" => KeyId::Backspace,
            "delete" | "forwarddelete" => KeyId::Delete,
            "home" => KeyId::Home,
            "end" => KeyId::End,
            "pageup" => KeyId::PageUp,
            "pagedown" => KeyId::PageDown,
            "backslash" | "\\" => KeyId::Character('\\'),
            "plus" | "+" => KeyId::Character('+'),
            "minus" | "-" => KeyId::Character('-'),
            "equal" | "=" => KeyId::Character('='),
            "underscore" | "_" => KeyId::Character('_'),
            "bracketleft" | "[" => KeyId::Character('['),
            "bracketright" | "]" => KeyId::Character(']'),
            "comma" => KeyId::Character(','),
            "period" | "." => KeyId::Character('.'),
            "slash" | "/" => KeyId::Character('/'),
            "semicolon" | ";" => KeyId::Character(';'),
            "quote" | "'" => KeyId::Character('\''),
            "backquote" | "`" => KeyId::Character('`'),
            "f1" => KeyId::Function(1),
            "f2" => KeyId::Function(2),
            "f3" => KeyId::Function(3),
            "f4" => KeyId::Function(4),
            "f5" => KeyId::Function(5),
            "f6" => KeyId::Function(6),
            "f7" => KeyId::Function(7),
            "f8" => KeyId::Function(8),
            "f9" => KeyId::Function(9),
            "f10" => KeyId::Function(10),
            "f11" => KeyId::Function(11),
            "f12" => KeyId::Function(12),
            _ if lower.len() == 1 && lower.as_bytes()[0].is_ascii_alphanumeric() => {
                KeyId::Character(char::from(lower.as_bytes()[0]))
            }
            _ => return Err(format!("Unknown key: {key_text}")),
        };
        Ok(Self::new(key, modifiers))
    }

    fn from_event(key: &Key, modifiers: ModifiersState) -> Option<Self> {
        let key = match key.as_ref() {
            Key::Character(" ") | Key::Named(NamedKey::Space) => KeyId::Space,
            Key::Character(text) if text.len() == 1 && text.is_ascii() => {
                KeyId::Character(char::from(text.as_bytes()[0]))
            }
            Key::Named(NamedKey::ArrowLeft) => KeyId::Left,
            Key::Named(NamedKey::ArrowRight) => KeyId::Right,
            Key::Named(NamedKey::ArrowUp) => KeyId::Up,
            Key::Named(NamedKey::ArrowDown) => KeyId::Down,
            Key::Named(NamedKey::Tab) => KeyId::Tab,
            Key::Named(NamedKey::Escape) => KeyId::Escape,
            Key::Named(NamedKey::Enter) => KeyId::Enter,
            Key::Named(NamedKey::Backspace) => KeyId::Backspace,
            Key::Named(NamedKey::Delete) => KeyId::Delete,
            Key::Named(NamedKey::Home) => KeyId::Home,
            Key::Named(NamedKey::End) => KeyId::End,
            Key::Named(NamedKey::PageUp) => KeyId::PageUp,
            Key::Named(NamedKey::PageDown) => KeyId::PageDown,
            Key::Named(NamedKey::F1) => KeyId::Function(1),
            Key::Named(NamedKey::F2) => KeyId::Function(2),
            Key::Named(NamedKey::F3) => KeyId::Function(3),
            Key::Named(NamedKey::F4) => KeyId::Function(4),
            Key::Named(NamedKey::F5) => KeyId::Function(5),
            Key::Named(NamedKey::F6) => KeyId::Function(6),
            Key::Named(NamedKey::F7) => KeyId::Function(7),
            Key::Named(NamedKey::F8) => KeyId::Function(8),
            Key::Named(NamedKey::F9) => KeyId::Function(9),
            Key::Named(NamedKey::F10) => KeyId::Function(10),
            Key::Named(NamedKey::F11) => KeyId::Function(11),
            Key::Named(NamedKey::F12) => KeyId::Function(12),
            _ => return None,
        };
        Some(Self::new(key, modifiers))
    }

    fn reserved(self) -> bool {
        matches!(
            (self.key, self.modifiers),
            (KeyId::Character('h' | 'm'), m) if m == ModifiersState::SUPER
        ) || (self.key == KeyId::Character('h')
            && self.modifiers == (ModifiersState::SUPER | ModifiersState::ALT))
    }

    fn text(self) -> String {
        let mut output = String::new();
        for (flag, name) in [
            (ModifiersState::SUPER, "Cmd+"),
            (ModifiersState::CONTROL, "Ctrl+"),
            (ModifiersState::ALT, "Alt+"),
            (ModifiersState::SHIFT, "Shift+"),
        ] {
            if self.modifiers.contains(flag) {
                output.push_str(name);
            }
        }
        let name = match self.key {
            KeyId::Space => "Space",
            KeyId::Left => "Left",
            KeyId::Right => "Right",
            KeyId::Up => "Up",
            KeyId::Down => "Down",
            KeyId::Tab => "Tab",
            KeyId::Escape => "Escape",
            KeyId::Enter => "Enter",
            KeyId::Backspace => "Backspace",
            KeyId::Delete => "Delete",
            KeyId::Home => "Home",
            KeyId::End => "End",
            KeyId::PageUp => "PageUp",
            KeyId::PageDown => "PageDown",
            KeyId::Character('\\') => "Backslash",
            KeyId::Character('=') => "Equal",
            KeyId::Character('-') => "Minus",
            KeyId::Character('[') => "BracketLeft",
            KeyId::Character(']') => "BracketRight",
            KeyId::Character(',') => "Comma",
            KeyId::Character('.') => "Period",
            KeyId::Character('/') => "Slash",
            KeyId::Character(';') => "Semicolon",
            KeyId::Character('\'') => "Quote",
            KeyId::Character('`') => "Backquote",
            KeyId::Character(c) => {
                output.push(c.to_ascii_uppercase());
                return output;
            }
            KeyId::Function(n) => {
                output.push_str(&format!("F{n}"));
                return output;
            }
        };
        output.push_str(name);
        output
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Bindings {
    bindings: [Vec<Chord>; Action::ALL.len()],
}

impl Default for Bindings {
    fn default() -> Self {
        Self {
            bindings: std::array::from_fn(|index| {
                Action::ALL[index]
                    .defaults()
                    .iter()
                    .map(|key| Chord::parse(key).expect("valid built-in shortcut"))
                    .collect()
            }),
        }
    }
}

impl Bindings {
    /// Missing actions retain defaults; an empty right-hand side unbinds an action.
    /// Older configurations keep custom bindings that conflict with new defaults.
    /// Validate the complete map before replacing the live configuration.
    pub(super) fn parse(config: &str) -> Result<Self, String> {
        if config.len() > MAX_CONFIG_BYTES {
            return Err("Shortcut settings exceed 64 KiB".into());
        }
        let mut result = Self::default();
        let mut seen = HashSet::new();
        for (index, line) in config.lines().enumerate() {
            let line_number = index + 1;
            if line.len() > MAX_LINE_BYTES {
                return Err(format!("Line {line_number} exceeds {MAX_LINE_BYTES} bytes"));
            }
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (id, keys) = line
                .split_once('=')
                .ok_or_else(|| format!("Line {line_number}: expected action = shortcuts"))?;
            let action = Action::ALL
                .iter()
                .copied()
                .find(|a| a.id() == id.trim())
                .ok_or_else(|| format!("Line {line_number}: unknown action {}", id.trim()))?;
            if !seen.insert(action) {
                return Err(format!(
                    "Line {line_number}: {} is listed twice",
                    action.id()
                ));
            }
            let mut chords = Vec::new();
            if !keys.trim().is_empty() {
                for key in keys.split(',') {
                    if chords.len() == MAX_ALIASES {
                        return Err(format!(
                            "{} allows at most {MAX_ALIASES} shortcuts",
                            action.label()
                        ));
                    }
                    let chord = Chord::parse(key)
                        .map_err(|error| format!("Line {line_number}: {error}"))?;
                    if chord.reserved() {
                        return Err(format!("{} is reserved by macOS", chord.text()));
                    }
                    if chords.contains(&chord) {
                        return Err(format!(
                            "Line {line_number}: duplicate shortcut {}",
                            chord.text()
                        ));
                    }
                    chords.push(chord);
                }
            }
            result.bindings[action as usize] = chords;
        }
        // These actions were added after customizable shortcuts. Preserve users'
        // existing assignments rather than rejecting their entire saved map.
        // The editor emits every action, so explicit conflicts still fail below.
        for action in [Action::RefreshFolder, Action::PhotoDetails] {
            if seen.contains(&action) {
                continue;
            }
            let new_defaults = result.bindings[action as usize].clone();
            result.bindings[action as usize] = new_defaults
                .into_iter()
                .filter(|chord| {
                    !seen
                        .iter()
                        .any(|action| result.bindings[*action as usize].contains(chord))
                })
                .collect();
        }
        let mut owners = std::collections::HashMap::new();
        for &action in Action::ALL {
            for chord in &result.bindings[action as usize] {
                if let Some(other) = owners.insert(*chord, action) {
                    return Err(format!(
                        "{} is assigned to both '{}' and '{}'. Remove it from one action first.",
                        chord.text(),
                        other.label(),
                        action.label()
                    ));
                }
            }
        }
        Ok(result)
    }

    pub(super) fn serialize(&self) -> String {
        let mut output = String::from(
            "# ARW Sprint keyboard shortcuts\n# Empty bindings disable an action. Separate aliases with commas.\n# Modifiers: Cmd, Ctrl, Alt (Option), Shift. Use Comma for the comma key.\n",
        );
        for &action in Action::ALL {
            output.push_str(action.id());
            output.push_str(" = ");
            output.push_str(&self.keys(action));
            output.push('\n');
        }
        output
    }

    /// No allocations, locks or I/O in event dispatch. The table is tiny and bounded.
    pub(super) fn resolve(
        &self,
        key: &Key,
        modifiers: ModifiersState,
        repeat: bool,
    ) -> Option<Action> {
        let chord = Chord::from_event(key, modifiers)?;
        self.action_for(chord)
            .filter(|action| !repeat || action.repeatable())
    }

    /// Exact base-key chords always win, even when auto-repeat suppresses their
    /// action. On layouts such as AZERTY, Shift+& produces 1 and Shift+é produces
    /// 2. Only this narrow shifted-digit case may consume Shift and look up the
    /// produced digit. Customizing or disabling that digit changes the fallback
    /// too; it is not an extra hardcoded rating shortcut or a new saved alias.
    /// Option retains its base-key identity, and Shift+ASCII letters stay exact.
    pub(super) fn resolve_event(
        &self,
        base: &Key,
        logical: &Key,
        modifiers: ModifiersState,
        repeat: bool,
    ) -> Option<Action> {
        if let Some(action) =
            Chord::from_event(base, modifiers).and_then(|chord| self.action_for(chord))
        {
            return (!repeat || action.repeatable()).then_some(action);
        }
        if !modifiers.shift_key() || modifiers.alt_key() {
            return None;
        }
        let Key::Character(base_text) = base.as_ref() else {
            return None;
        };
        let mut characters = base_text.chars();
        if characters.next()?.is_ascii_alphanumeric() || characters.next().is_some() {
            return None;
        }
        let Key::Character(digit) = logical.as_ref() else {
            return None;
        };
        if digit.len() != 1 || !digit.as_bytes()[0].is_ascii_digit() {
            return None;
        }
        self.resolve(logical, modifiers & !ModifiersState::SHIFT, repeat)
    }

    fn action_for(&self, chord: Chord) -> Option<Action> {
        Action::ALL
            .iter()
            .copied()
            .find(|action| self.bindings[*action as usize].contains(&chord))
    }

    pub(super) fn keys(&self, action: Action) -> String {
        self.bindings[action as usize]
            .iter()
            .map(|chord| chord.text())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn help_text(&self) -> String {
        let mut output = String::new();
        for &action in Action::ALL {
            let keys = self.keys(action);
            if !keys.is_empty() {
                output.push_str(&keys);
                output.push_str(": ");
                output.push_str(action.label());
                output.push('\n');
            }
        }
        output.push_str("\nCmd+H: hide ARW Sprint\nCmd+Alt+H: hide other applications\nCmd+M: minimize window\n");
        output
    }

    pub(super) fn editor_rows(&self) -> String {
        let mut output = String::new();
        for &action in Action::ALL {
            output.push_str(action.id());
            output.push('\t');
            output.push_str(action.label());
            output.push('\t');
            output.push_str(&self.keys(action));
            output.push('\n');
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(character: &str) -> Key {
        Key::Character(character.into())
    }

    #[test]
    fn defaults_roundtrip_without_conflicts() {
        let defaults = Bindings::default();
        assert_eq!(Bindings::parse("").unwrap(), defaults);
        assert_eq!(Bindings::parse(&defaults.serialize()).unwrap(), defaults);
        assert_eq!(defaults.editor_rows().lines().count(), Action::ALL.len());
        assert!(defaults.help_text().contains("Cmd+Comma"));
    }

    #[test]
    fn disables_adds_and_reassigns_without_mutating_other_actions() {
        let custom =
            Bindings::parse("move_rejected = \nnext = Space, Right\nhelp = Ctrl+H").unwrap();
        assert_eq!(
            custom.resolve(&key(" "), ModifiersState::empty(), false),
            Some(Action::Next)
        );
        assert_eq!(
            custom.resolve(&key("x"), ModifiersState::empty(), false),
            None
        );
        assert_eq!(
            custom.resolve(&key("h"), ModifiersState::CONTROL, false),
            Some(Action::Help)
        );
        assert_eq!(
            custom.resolve(&key("5"), ModifiersState::empty(), false),
            Some(Action::Rate5)
        );
        assert_eq!(Bindings::parse(&custom.serialize()).unwrap(), custom);
    }

    #[test]
    fn refresh_is_non_repeating_and_can_be_disabled_or_remapped() {
        // Existing saved configurations inherit the new default.
        let bindings = Bindings::parse("zoom_in = V").unwrap();
        assert_eq!(
            bindings.resolve(&key("r"), ModifiersState::SUPER, false),
            Some(Action::RefreshFolder)
        );
        assert_eq!(
            bindings.resolve(&key("r"), ModifiersState::SUPER, true),
            None
        );
        assert_eq!(
            bindings.resolve(&key("r"), ModifiersState::empty(), false),
            Some(Action::ViewRaw)
        );

        let remapped = Bindings::parse("refresh_folder = Ctrl+R\nhelp = Cmd+R").unwrap();
        assert_eq!(
            remapped.resolve(&key("r"), ModifiersState::CONTROL, false),
            Some(Action::RefreshFolder)
        );
        assert_eq!(
            remapped.resolve(&key("r"), ModifiersState::SUPER, false),
            Some(Action::Help)
        );
        let disabled = Bindings::parse("refresh_folder =").unwrap();
        assert_eq!(
            disabled.resolve(&key("r"), ModifiersState::SUPER, false),
            None
        );
        assert_eq!(Bindings::parse(&remapped.serialize()).unwrap(), remapped);
    }

    #[test]
    fn older_custom_cmd_r_assignment_survives_refresh_default() {
        let existing = Bindings::parse("help = Cmd+R").unwrap();
        assert_eq!(
            existing.resolve(&key("r"), ModifiersState::SUPER, false),
            Some(Action::Help)
        );
        assert!(existing.keys(Action::RefreshFolder).is_empty());
        assert_eq!(Bindings::parse(&existing.serialize()).unwrap(), existing);
        assert!(Bindings::parse("help = Cmd+R\nrefresh_folder = Cmd+R").is_err());
    }

    #[test]
    fn photo_details_toggle_is_non_repeating_and_customizable() {
        let bindings = Bindings::parse("zoom_in = V").unwrap();
        assert_eq!(
            bindings.resolve(&key("i"), ModifiersState::empty(), false),
            Some(Action::PhotoDetails)
        );
        assert_eq!(
            bindings.resolve(&key("i"), ModifiersState::empty(), true),
            None
        );
        assert_eq!(
            bindings.resolve(&key("i"), ModifiersState::SUPER, false),
            None
        );
        let remapped = Bindings::parse("photo_details = Cmd+I\nhelp = I").unwrap();
        assert_eq!(
            remapped.resolve(&key("i"), ModifiersState::SUPER, false),
            Some(Action::PhotoDetails)
        );
        assert_eq!(
            remapped.resolve(&key("i"), ModifiersState::empty(), false),
            Some(Action::Help)
        );
        assert!(Bindings::parse("photo_details =")
            .unwrap()
            .keys(Action::PhotoDetails)
            .is_empty());
        assert_eq!(Bindings::parse(&remapped.serialize()).unwrap(), remapped);
        assert!(bindings
            .help_text()
            .contains("I: Show / hide photo details (EXIF)"));
    }

    #[test]
    fn older_custom_i_assignment_survives_photo_details_default() {
        let existing = Bindings::parse("help = I\nrefresh_folder = Cmd+R").unwrap();
        assert_eq!(
            existing.resolve(&key("i"), ModifiersState::empty(), false),
            Some(Action::Help)
        );
        assert!(existing.keys(Action::PhotoDetails).is_empty());
        assert_eq!(Bindings::parse(&existing.serialize()).unwrap(), existing);
        assert!(Bindings::parse("help = I\nphoto_details = I").is_err());

        let original = Bindings::parse("help = I\nfit = Cmd+R").unwrap();
        assert!(original.keys(Action::PhotoDetails).is_empty());
        assert!(original.keys(Action::RefreshFolder).is_empty());
        assert_eq!(Bindings::parse(&original.serialize()).unwrap(), original);
    }

    #[test]
    fn rejects_conflicts_including_default_bindings_and_shifted_punctuation() {
        for config in [
            "next = Space",
            "next = X",
            "help = Shift+Equal",
            "help = Plus",
            "next = G\nprevious = g",
            "help = Cmd+Option+1",
            "zoom_in = Plus, Shift+Equal",
            "help = Shift+Minus",
        ] {
            assert!(Bindings::parse(config).is_err(), "{config}");
        }
    }

    #[test]
    fn invalid_configuration_never_silently_rebinds_a_destructive_action() {
        for config in [
            "unknown = X",
            "move_rejected = UnknownKey",
            "move_rejected = x,",
            "move_rejected = x,,y",
            "move_rejected X",
            "move_rejected = XX",
            "move_rejected = Ctrl+Ctrl+X",
            "move_rejected = Super+Cmd+X",
            "move_rejected = Meta+X",
            "move_rejected = +X",
            "move_rejected = Cmd+",
            "move_rejected = X\nmove_rejected = Y",
            "move_rejected = χ",
            "move_rejected = X\0",
            "move_rejected = A,B,C,D,E,F,G,H,I",
        ] {
            assert!(Bindings::parse(config).is_err(), "{config:?}");
        }
        assert!(Bindings::parse(&" ".repeat(MAX_CONFIG_BYTES + 1)).is_err());
        assert!(Bindings::parse(&format!("#{}", "x".repeat(MAX_LINE_BYTES))).is_err());
    }

    #[test]
    fn native_hide_and_minimize_shortcuts_are_reserved() {
        for chord in ["Cmd+H", "Cmd+Alt+H", "Cmd+Option+H", "Cmd+M"] {
            assert!(Bindings::parse(&format!("help = {chord}"))
                .unwrap_err()
                .contains("reserved"));
        }
    }

    #[test]
    fn modifier_matching_is_exact() {
        let bindings = Bindings::parse("help = Ctrl+Alt+G").unwrap();
        for flags in 0_u8..16 {
            let mut modifiers = ModifiersState::empty();
            for (bit, flag) in [
                ModifiersState::SUPER,
                ModifiersState::CONTROL,
                ModifiersState::ALT,
                ModifiersState::SHIFT,
            ]
            .into_iter()
            .enumerate()
            {
                if flags & (1 << bit) != 0 {
                    modifiers |= flag;
                }
            }
            let expected = (modifiers == (ModifiersState::CONTROL | ModifiersState::ALT))
                .then_some(Action::Help);
            assert_eq!(bindings.resolve(&key("g"), modifiers, false), expected);
        }
        assert_eq!(
            bindings.resolve(&key("x"), ModifiersState::ALT, false),
            None
        );
        assert_eq!(
            bindings.resolve(&key("c"), ModifiersState::SHIFT, false),
            Some(Action::ReplacePin)
        );
        assert_eq!(
            bindings.resolve(&key("c"), ModifiersState::empty(), false),
            Some(Action::Pin)
        );
    }

    #[test]
    fn only_navigation_and_incremental_zoom_or_brightness_repeat() {
        let bindings = Bindings::default();
        for &action in Action::ALL {
            for chord in &bindings.bindings[action as usize] {
                let event_key = match chord.key {
                    KeyId::Character(c) => key(&c.to_string()),
                    KeyId::Space => Key::Named(NamedKey::Space),
                    KeyId::Left => Key::Named(NamedKey::ArrowLeft),
                    KeyId::Right => Key::Named(NamedKey::ArrowRight),
                    KeyId::Tab => Key::Named(NamedKey::Tab),
                    KeyId::Escape => Key::Named(NamedKey::Escape),
                    KeyId::Backspace => Key::Named(NamedKey::Backspace),
                    _ => unreachable!("no other named keys in defaults"),
                };
                assert_eq!(
                    bindings.resolve(&event_key, chord.modifiers, false),
                    Some(action)
                );
                assert_eq!(
                    bindings.resolve(&event_key, chord.modifiers, true),
                    action.repeatable().then_some(action)
                );
            }
        }
        for action in [
            Action::MoveRejected,
            Action::TrashCurrent,
            Action::TrashRejected,
            Action::MoveRejectedBatch,
            Action::Rate1,
            Action::ClearRating,
            Action::Undo,
            Action::Peek,
        ] {
            assert!(!action.repeatable());
        }
    }

    #[test]
    fn supports_space_cases_and_shifted_punctuation_from_both_event_apis() {
        let bindings = Bindings::default();
        for text in ["x", "X", " "] {
            assert_eq!(
                bindings.resolve(&key(text), ModifiersState::empty(), false),
                Some(Action::MoveRejected)
            );
        }
        assert_eq!(
            bindings.resolve(&key("x"), ModifiersState::SHIFT, false),
            Some(Action::MoveRejected)
        );
        assert_eq!(
            bindings.resolve(&key(" "), ModifiersState::SHIFT, false),
            Some(Action::Previous)
        );
        for (text, mods, expected) in [
            ("=", ModifiersState::empty(), Action::ZoomIn),
            ("=", ModifiersState::SHIFT, Action::ZoomIn),
            ("+", ModifiersState::SHIFT, Action::ZoomIn),
            ("+", ModifiersState::empty(), Action::ZoomIn),
            ("-", ModifiersState::SHIFT, Action::ZoomOut),
            ("_", ModifiersState::SHIFT, Action::ZoomOut),
            ("_", ModifiersState::empty(), Action::ZoomOut),
        ] {
            assert_eq!(bindings.resolve(&key(text), mods, false), Some(expected));
        }
        assert_eq!(
            Chord::parse("Cmd++").unwrap(),
            Chord::parse("Cmd+Shift+Equal").unwrap()
        );
    }

    #[test]
    fn unsupported_and_composed_keys_do_not_trigger_actions() {
        let bindings = Bindings::default();
        for key in [
            key(""),
            key("xx"),
            key("χ"),
            key("Space"),
            Key::Dead(Some('x')),
        ] {
            assert_eq!(bindings.resolve(&key, ModifiersState::empty(), false), None);
        }
    }

    #[test]
    fn shifted_layout_digits_use_existing_bindings_and_repeat_policy() {
        let bindings = Bindings::default();
        for (base, digit, action) in [
            ("&", "1", Action::Rate1),
            ("é", "2", Action::Rate2),
            ("\"", "3", Action::Rate3),
            ("'", "4", Action::Rate4),
            ("(", "5", Action::Rate5),
        ] {
            assert_eq!(
                bindings.resolve_event(&key(base), &key(digit), ModifiersState::SHIFT, false),
                Some(action)
            );
            assert_eq!(
                bindings.resolve_event(&key(base), &key(digit), ModifiersState::SHIFT, true),
                None
            );
        }
        assert_eq!(
            bindings.resolve_event(
                &key("&"),
                &key("1"),
                ModifiersState::SUPER | ModifiersState::SHIFT,
                false
            ),
            None
        );
        let custom = Bindings::parse("rate_1 =\nnext = 1").unwrap();
        assert_eq!(
            custom.resolve_event(&key("&"), &key("1"), ModifiersState::SHIFT, true),
            Some(Action::Next)
        );
        let disabled = Bindings::parse("rate_1 =").unwrap();
        assert_eq!(
            disabled.resolve_event(&key("&"), &key("1"), ModifiersState::SHIFT, false),
            None
        );
    }

    #[test]
    fn explicit_base_chord_precedes_layout_fallback_even_when_repeat_suppresses_it() {
        // A shifted apostrophe produces 4 on AZERTY. The explicit Shift+Quote
        // binding must win over the existing plain 4 binding on every event.
        let bindings = Bindings::parse("help = Shift+Quote\nrate_4 =\nnext = 4").unwrap();
        assert_eq!(
            bindings.resolve_event(&key("'"), &key("4"), ModifiersState::SHIFT, false),
            Some(Action::Help)
        );
        assert_eq!(
            bindings.resolve_event(&key("'"), &key("4"), ModifiersState::SHIFT, true),
            None
        );
    }

    #[test]
    fn layout_fallback_never_consumes_option_or_generalizes_shifted_letters() {
        let bindings = Bindings::parse("help = Alt+Shift+G").unwrap();
        assert_eq!(
            bindings.resolve_event(
                &key("g"),
                &key("˝"),
                ModifiersState::ALT | ModifiersState::SHIFT,
                false
            ),
            Some(Action::Help)
        );
        for (base, logical, modifiers) in [
            ("&", "1", ModifiersState::empty()),
            ("&", "1", ModifiersState::SHIFT | ModifiersState::ALT),
            ("g", "1", ModifiersState::SHIFT),
            ("1", "1", ModifiersState::SHIFT),
            ("a", "A", ModifiersState::SHIFT),
            ("&", "11", ModifiersState::SHIFT),
            ("&x", "1", ModifiersState::SHIFT),
        ] {
            assert_eq!(
                bindings.resolve_event(&key(base), &key(logical), modifiers, false),
                None
            );
        }
        assert_eq!(
            bindings.resolve_event(&key("c"), &key("C"), ModifiersState::SHIFT, false),
            Some(Action::ReplacePin)
        );
    }
}
