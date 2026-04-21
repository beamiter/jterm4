use gtk4::gdk::Key;
use gtk4::gdk::ModifierType;
use gtk4::glib::translate::IntoGlib;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Action {
    NewTab,
    CloseTab,
    ClosePaneOrTab,
    Copy,
    Paste,
    FontIncrease,
    FontDecrease,
    OpacityIncrease,
    OpacityDecrease,
    ToggleSearch,
    ToggleCommandPalette,
    ToggleSettings,
    ToggleSidebar,
    SplitHorizontal,
    SplitVertical,
    PrevTab,
    NextTab,
    ScrollUp,
    ScrollDown,
    CyclePaneFocusForward,
    CyclePaneFocusBackward,
    QuickSwitchTab(u8),
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    TogglePaneZoom,
    MovePaneToNewTab,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
}

impl Action {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Action::NewTab => "New tab",
            Action::CloseTab => "Close tab",
            Action::ClosePaneOrTab => "Close focused pane or tab",
            Action::Copy => "Copy",
            Action::Paste => "Paste",
            Action::FontIncrease => "Font size increase",
            Action::FontDecrease => "Font size decrease",
            Action::OpacityIncrease => "Opacity increase",
            Action::OpacityDecrease => "Opacity decrease",
            Action::ToggleSearch => "Toggle search",
            Action::ToggleCommandPalette => "Command palette",
            Action::ToggleSettings => "Toggle settings panel",
            Action::ToggleSidebar => "Toggle sidebar",
            Action::SplitHorizontal => "Split horizontal",
            Action::SplitVertical => "Split vertical",
            Action::PrevTab => "Previous tab",
            Action::NextTab => "Next tab",
            Action::ScrollUp => "Scroll up",
            Action::ScrollDown => "Scroll down",
            Action::CyclePaneFocusForward => "Cycle pane focus forward",
            Action::CyclePaneFocusBackward => "Cycle pane focus backward",
            Action::QuickSwitchTab(n) => match n {
                0 => "Switch to tab 1",
                1 => "Switch to tab 2",
                2 => "Switch to tab 3",
                3 => "Switch to tab 4",
                4 => "Switch to tab 5",
                5 => "Switch to tab 6",
                6 => "Switch to tab 7",
                7 => "Switch to tab 8",
                8 => "Switch to tab 9",
                _ => "Switch to last tab",
            },
            Action::ResizePaneLeft => "Resize pane left",
            Action::ResizePaneRight => "Resize pane right",
            Action::ResizePaneUp => "Resize pane up",
            Action::ResizePaneDown => "Resize pane down",
            Action::TogglePaneZoom => "Toggle pane zoom",
            Action::MovePaneToNewTab => "Move pane to new tab",
            Action::FocusPaneLeft => "Focus pane left",
            Action::FocusPaneRight => "Focus pane right",
            Action::FocusPaneUp => "Focus pane up",
            Action::FocusPaneDown => "Focus pane down",
        }
    }

    pub(crate) fn config_key(&self) -> Option<&'static str> {
        match self {
            Action::NewTab => Some("new_tab"),
            Action::CloseTab => Some("close_tab"),
            Action::ClosePaneOrTab => Some("close_pane_or_tab"),
            Action::Copy => Some("copy"),
            Action::Paste => Some("paste"),
            Action::FontIncrease => Some("font_increase"),
            Action::FontDecrease => Some("font_decrease"),
            Action::OpacityIncrease => Some("opacity_increase"),
            Action::OpacityDecrease => Some("opacity_decrease"),
            Action::ToggleSearch => Some("toggle_search"),
            Action::ToggleCommandPalette => Some("toggle_command_palette"),
            Action::ToggleSettings => Some("toggle_settings"),
            Action::ToggleSidebar => Some("toggle_sidebar"),
            Action::SplitHorizontal => Some("split_horizontal"),
            Action::SplitVertical => Some("split_vertical"),
            Action::PrevTab => Some("prev_tab"),
            Action::NextTab => Some("next_tab"),
            Action::ScrollUp => Some("scroll_up"),
            Action::ScrollDown => Some("scroll_down"),
            Action::CyclePaneFocusForward => Some("cycle_pane_focus_forward"),
            Action::CyclePaneFocusBackward => Some("cycle_pane_focus_backward"),
            Action::QuickSwitchTab(_) => None,
            Action::ResizePaneLeft => Some("resize_pane_left"),
            Action::ResizePaneRight => Some("resize_pane_right"),
            Action::ResizePaneUp => Some("resize_pane_up"),
            Action::ResizePaneDown => Some("resize_pane_down"),
            Action::TogglePaneZoom => Some("toggle_pane_zoom"),
            Action::MovePaneToNewTab => Some("move_pane_to_new_tab"),
            Action::FocusPaneLeft => Some("focus_pane_left"),
            Action::FocusPaneRight => Some("focus_pane_right"),
            Action::FocusPaneUp => Some("focus_pane_up"),
            Action::FocusPaneDown => Some("focus_pane_down"),
        }
    }

    pub(crate) fn all_actions() -> Vec<Action> {
        vec![
            Action::NewTab,
            Action::CloseTab,
            Action::ClosePaneOrTab,
            Action::Copy,
            Action::Paste,
            Action::FontIncrease,
            Action::FontDecrease,
            Action::OpacityIncrease,
            Action::OpacityDecrease,
            Action::ToggleSearch,
            Action::ToggleCommandPalette,
            Action::ToggleSettings,
            Action::ToggleSidebar,
            Action::SplitHorizontal,
            Action::SplitVertical,
            Action::PrevTab,
            Action::NextTab,
            Action::ScrollUp,
            Action::ScrollDown,
            Action::CyclePaneFocusForward,
            Action::CyclePaneFocusBackward,
            Action::ResizePaneLeft,
            Action::ResizePaneRight,
            Action::ResizePaneUp,
            Action::ResizePaneDown,
            Action::TogglePaneZoom,
            Action::MovePaneToNewTab,
            Action::FocusPaneLeft,
            Action::FocusPaneRight,
            Action::FocusPaneUp,
            Action::FocusPaneDown,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyCombo {
    pub(crate) modifiers: ModifierType,
    pub(crate) key: Key,
}

impl PartialEq for KeyCombo {
    fn eq(&self, other: &Self) -> bool {
        self.modifiers == other.modifiers && self.key == other.key
    }
}

impl Eq for KeyCombo {}

impl Hash for KeyCombo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.modifiers.bits().hash(state);
        self.key.into_glib().hash(state);
    }
}

pub(crate) fn normalize_key(key: Key) -> Key {
    // ISO_Left_Tab is what GTK sends for Shift+Tab - normalize to Tab
    if key == Key::ISO_Left_Tab {
        return Key::Tab;
    }
    key.to_lower()
}

pub(crate) fn parse_key_combo(s: &str) -> Result<KeyCombo, String> {
    let mut modifiers = ModifierType::empty();
    let parts: Vec<&str> = s.split('+').collect();
    if parts.is_empty() {
        return Err("Empty key combo".to_string());
    }

    // The last part is the key, but "+" itself is special:
    // "Ctrl+Shift++" means Ctrl+Shift and key is "+"
    let (mod_parts, key_str) = if s.ends_with("++") && parts.len() >= 3 {
        (&parts[..parts.len() - 2], "+")
    } else if parts.last() == Some(&"") && parts.len() >= 2 {
        // "Ctrl++" case
        (&parts[..parts.len() - 2], "+")
    } else {
        (&parts[..parts.len() - 1], *parts.last().unwrap())
    };

    for part in mod_parts {
        match *part {
            "Ctrl" => modifiers |= ModifierType::CONTROL_MASK,
            "Shift" => modifiers |= ModifierType::SHIFT_MASK,
            "Alt" => modifiers |= ModifierType::ALT_MASK,
            other => return Err(format!("Unknown modifier: {other}")),
        }
    }

    let key = match key_str {
        "+" | "plus" => Key::plus,
        "-" | "minus" => Key::minus,
        "PageUp" => Key::Page_Up,
        "PageDown" => Key::Page_Down,
        "Tab" => Key::Tab,
        "Escape" | "Esc" => Key::Escape,
        "Return" | "Enter" => Key::Return,
        "Up" => Key::Up,
        "Down" => Key::Down,
        "Left" => Key::Left,
        "Right" => Key::Right,
        "!" | "exclam" => Key::exclam,
        "Space" => Key::space,
        "Backspace" => Key::BackSpace,
        "Delete" => Key::Delete,
        "Home" => Key::Home,
        "End" => Key::End,
        "Insert" => Key::Insert,
        s if s.len() == 1 => {
            let c = s.chars().next().unwrap();
            if c.is_ascii_digit() {
                match c {
                    '0' => Key::_0,
                    '1' => Key::_1,
                    '2' => Key::_2,
                    '3' => Key::_3,
                    '4' => Key::_4,
                    '5' => Key::_5,
                    '6' => Key::_6,
                    '7' => Key::_7,
                    '8' => Key::_8,
                    '9' => Key::_9,
                    _ => unreachable!(),
                }
            } else if c.is_ascii_alphabetic() {
                Key::from_name(c.to_lowercase().to_string())
                    .ok_or_else(|| format!("Unknown key: {s}"))?
            } else {
                return Err(format!("Unknown key: {s}"));
            }
        }
        s => Key::from_name(s).ok_or_else(|| format!("Unknown key: {s}"))?,
    };

    Ok(KeyCombo {
        modifiers,
        key: normalize_key(key),
    })
}

pub(crate) fn key_combo_to_string(combo: &KeyCombo) -> String {
    let mut parts = Vec::new();
    if combo.modifiers.contains(ModifierType::CONTROL_MASK) {
        parts.push("Ctrl");
    }
    if combo.modifiers.contains(ModifierType::SHIFT_MASK) {
        parts.push("Shift");
    }
    if combo.modifiers.contains(ModifierType::ALT_MASK) {
        parts.push("Alt");
    }

    let key_name = match combo.key {
        Key::plus => "+".to_string(),
        Key::minus => "-".to_string(),
        Key::Page_Up => "PageUp".to_string(),
        Key::Page_Down => "PageDown".to_string(),
        Key::Tab | Key::ISO_Left_Tab => "Tab".to_string(),
        Key::Escape => "Escape".to_string(),
        Key::Return => "Enter".to_string(),
        Key::Up => "Up".to_string(),
        Key::Down => "Down".to_string(),
        Key::Left => "Left".to_string(),
        Key::Right => "Right".to_string(),
        Key::exclam => "!".to_string(),
        Key::space => "Space".to_string(),
        Key::BackSpace => "Backspace".to_string(),
        Key::Delete => "Delete".to_string(),
        Key::Home => "Home".to_string(),
        Key::End => "End".to_string(),
        k => k.name().map(|n| {
            let s = n.to_string();
            if s.len() == 1 { s.to_uppercase() } else { s }
        }).unwrap_or_else(|| "?".to_string()),
    };

    let mut result = parts.join("+");
    if !result.is_empty() {
        result.push('+');
    }
    result.push_str(&key_name);
    result
}

#[derive(Clone)]
pub(crate) struct KeybindingMap {
    pub(crate) bindings: HashMap<KeyCombo, Action>,
}

impl KeybindingMap {
    pub(crate) fn from_defaults() -> Self {
        let mut bindings = HashMap::new();

        let mut bind = |s: &str, action: Action| {
            if let Ok(combo) = parse_key_combo(s) {
                bindings.insert(combo, action);
            }
        };

        // Existing keybindings
        bind("Ctrl+Shift+T", Action::NewTab);
        bind("Ctrl+Shift+W", Action::ClosePaneOrTab);
        bind("Ctrl+Shift+C", Action::Copy);
        bind("Ctrl+Shift+V", Action::Paste);
        bind("Ctrl+Shift++", Action::FontIncrease);
        bind("Ctrl+Shift+I", Action::FontDecrease);
        bind("Ctrl+Shift+J", Action::OpacityDecrease);
        bind("Ctrl+Shift+K", Action::OpacityIncrease);
        bind("Ctrl+Shift+F", Action::ToggleSearch);
        bind("Ctrl+Shift+P", Action::ToggleCommandPalette);
        bind("Ctrl+Shift+O", Action::ToggleSettings);
        bind("Ctrl+backslash", Action::ToggleSidebar);
        bind("Ctrl+Shift+E", Action::SplitHorizontal);
        bind("Ctrl+Shift+D", Action::SplitVertical);
        bind("Ctrl+Shift+PageUp", Action::PrevTab);
        bind("Ctrl+Shift+PageDown", Action::NextTab);
        bind("Ctrl+Shift+Tab", Action::PrevTab);
        bind("Ctrl+Tab", Action::NextTab);
        bind("Ctrl+Up", Action::ScrollUp);
        bind("Ctrl+Down", Action::ScrollDown);
        bind("Ctrl+minus", Action::FontDecrease);
        bind("Ctrl+PageUp", Action::PrevTab);
        bind("Ctrl+PageDown", Action::NextTab);
        for i in 0..=9u8 {
            bind(&format!("Ctrl+{i}"), Action::QuickSwitchTab(i));
        }
        bind("Alt+Tab", Action::CyclePaneFocusForward);
        bind("Alt+Shift+Tab", Action::CyclePaneFocusBackward);

        // New pane management keybindings
        bind("Alt+Shift+Left", Action::ResizePaneLeft);
        bind("Alt+Shift+Right", Action::ResizePaneRight);
        bind("Alt+Shift+Up", Action::ResizePaneUp);
        bind("Alt+Shift+Down", Action::ResizePaneDown);
        bind("Ctrl+Shift+Z", Action::TogglePaneZoom);
        bind("Ctrl+Shift+!", Action::MovePaneToNewTab);
        bind("Alt+Left", Action::FocusPaneLeft);
        bind("Alt+Right", Action::FocusPaneRight);
        bind("Alt+Up", Action::FocusPaneUp);
        bind("Alt+Down", Action::FocusPaneDown);

        KeybindingMap { bindings }
    }

    pub(crate) fn apply_user_overrides(&mut self, table: &toml::Table) {
        // Build reverse map: config_key -> Action
        let mut key_to_action: HashMap<&str, Action> = HashMap::new();
        for action in Action::all_actions() {
            if let Some(key) = action.config_key() {
                key_to_action.insert(key, action);
            }
        }

        for (config_key, value) in table {
            let Some(&action) = key_to_action.get(config_key.as_str()) else {
                log::warn!("Unknown keybinding action: {config_key}");
                continue;
            };
            let Some(key_str) = value.as_str() else {
                log::warn!("Keybinding value for {config_key} must be a string");
                continue;
            };

            // Remove old bindings for this action
            self.bindings.retain(|_, a| *a != action);

            // Parse and add new binding
            match parse_key_combo(key_str) {
                Ok(combo) => { self.bindings.insert(combo, action); }
                Err(e) => { log::warn!("Invalid keybinding '{key_str}' for {config_key}: {e}"); }
            }
        }
    }

    pub(crate) fn lookup(&self, combo: &KeyCombo) -> Option<Action> {
        self.bindings.get(combo).copied()
    }

    pub(crate) fn binding_display(&self, action: &Action) -> String {
        let combos: Vec<_> = self.bindings.iter()
            .filter(|(_, a)| *a == action)
            .map(|(k, _)| key_combo_to_string(k))
            .collect();
        combos.join(", ")
    }

    pub(crate) fn all_bound_actions(&self) -> Vec<(Action, String)> {
        let mut result = Vec::new();
        for action in Action::all_actions() {
            let display = self.binding_display(&action);
            result.push((action, display));
        }
        result
    }
}
