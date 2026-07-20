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
    FontReset,
    OpacityIncrease,
    OpacityDecrease,
    ToggleSearch,
    ToggleCommandPalette,
    ToggleSettings,
    ReloadConfig,
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
    ShowRemotePicker,
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
    FilterTabs,
    CloseSelectedTabs,
    MoveTabLeft,
    MoveTabRight,
    DuplicateTab,
    ToggleTabMarked,
    ToggleTabPinned,
    ToggleTabPlacement,
    FilterFailedBlocks,
    FilterSlowBlocks,
    FilterPinnedBlocks,
    ClearBlockFilter,
    /// Select every finished block in the active block-mode pane.
    SelectAllBlocks,
    /// Remove every finished block from the active block-mode pane.
    ClearBlocks,
    /// Put selected commands back into the live editor in terminal order.
    ReinputSelectedCommands,
    JumpToPrevPinned,
    JumpToNextPinned,
    ToggleDebugDashboard,
    /// Show/hide the right-side AI chat panel.
    ToggleAiPanel,
    /// Send the currently selected finished block (cmd + output + exit) to the
    /// AI panel as a fresh "explain this" question.
    AskAiAboutSelectedBlock,
    /// Open the approval-gated multi-turn shell Agent for the active Block pane.
    OpenAgent,
    /// Open a fuzzy palette over this tab's finished-block command history.
    /// Enter pastes the selected command into the live input cell.
    HistoryPalette,
    /// Cross-block ripgrep palette — flat list of every line that matches
    /// the query across all finished blocks; Enter jumps to that hit.
    CrossBlockSearch,
    /// Workflows palette — fuzzy list of saved command templates; Enter
    /// opens an arg-entry dialog that substitutes placeholders and writes
    /// the resolved command into the live PTY (no auto-Enter).
    WorkflowsPalette,
    /// Open the bundled executable quick-start notebook.
    OpenWelcome,
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
            Action::FontReset => "Reset font size",
            Action::OpacityIncrease => "Opacity increase",
            Action::OpacityDecrease => "Opacity decrease",
            Action::ToggleSearch => "Toggle search",
            Action::ToggleCommandPalette => "Command palette",
            Action::ToggleSettings => "Toggle settings panel",
            Action::ReloadConfig => "Reload configuration",
            Action::ToggleSidebar => "Toggle sidebar",
            Action::SplitHorizontal => "Split left/right",
            Action::SplitVertical => "Split top/bottom",
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
            Action::ShowRemotePicker => "Connect to remote host…",
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
            Action::FilterTabs => "Filter tabs",
            Action::CloseSelectedTabs => "Close selected tabs",
            Action::MoveTabLeft => "Move tab left",
            Action::MoveTabRight => "Move tab right",
            Action::DuplicateTab => "Duplicate tab",
            Action::ToggleTabMarked => "Toggle tab marked",
            Action::ToggleTabPinned => "Toggle tab pinned",
            Action::ToggleTabPlacement => "Toggle tab placement (sidebar/top)",
            Action::FilterFailedBlocks => "Jump to first failed block",
            Action::FilterSlowBlocks => "Jump to first slow block",
            Action::FilterPinnedBlocks => "Jump to first bookmarked block",
            Action::ClearBlockFilter => "Jump to oldest block",
            Action::SelectAllBlocks => "Select all blocks",
            Action::ClearBlocks => "Clear blocks",
            Action::ReinputSelectedCommands => "Reinput selected commands",
            Action::JumpToPrevPinned => "Jump to previous bookmarked block",
            Action::JumpToNextPinned => "Jump to next bookmarked block",
            Action::ToggleDebugDashboard => "Toggle debug dashboard",
            Action::ToggleAiPanel => "Toggle AI panel",
            Action::AskAiAboutSelectedBlock => "Ask AI about selected block",
            Action::OpenAgent => "Open shell Agent",
            Action::HistoryPalette => "Command history palette",
            Action::CrossBlockSearch => "Search across blocks (ripgrep)",
            Action::WorkflowsPalette => "Workflows palette",
            Action::OpenWelcome => "Open welcome & quick start notebook",
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
            Action::FontReset => Some("font_reset"),
            Action::OpacityIncrease => Some("opacity_increase"),
            Action::OpacityDecrease => Some("opacity_decrease"),
            Action::ToggleSearch => Some("toggle_search"),
            Action::ToggleCommandPalette => Some("toggle_command_palette"),
            Action::ToggleSettings => Some("toggle_settings"),
            Action::ReloadConfig => Some("reload_config"),
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
            Action::ShowRemotePicker => Some("show_remote_picker"),
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
            Action::FilterTabs => Some("filter_tabs"),
            Action::CloseSelectedTabs => Some("close_selected_tabs"),
            Action::MoveTabLeft => Some("move_tab_left"),
            Action::MoveTabRight => Some("move_tab_right"),
            Action::DuplicateTab => Some("duplicate_tab"),
            Action::ToggleTabMarked => Some("toggle_tab_marked"),
            Action::ToggleTabPinned => Some("toggle_tab_pinned"),
            Action::ToggleTabPlacement => Some("toggle_tab_placement"),
            Action::FilterFailedBlocks => Some("filter_failed_blocks"),
            Action::FilterSlowBlocks => Some("filter_slow_blocks"),
            Action::FilterPinnedBlocks => Some("filter_pinned_blocks"),
            Action::ClearBlockFilter => Some("clear_block_filter"),
            Action::SelectAllBlocks => Some("select_all_blocks"),
            Action::ClearBlocks => Some("clear_blocks"),
            Action::ReinputSelectedCommands => Some("reinput_selected_commands"),
            Action::JumpToPrevPinned => Some("jump_to_prev_pinned"),
            Action::JumpToNextPinned => Some("jump_to_next_pinned"),
            Action::ToggleDebugDashboard => Some("toggle_debug_dashboard"),
            Action::ToggleAiPanel => Some("toggle_ai_panel"),
            Action::AskAiAboutSelectedBlock => Some("ask_ai_about_selected_block"),
            Action::OpenAgent => Some("open_agent"),
            Action::HistoryPalette => Some("history_palette"),
            Action::CrossBlockSearch => Some("cross_block_search"),
            Action::WorkflowsPalette => Some("workflows_palette"),
            Action::OpenWelcome => None,
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
            Action::FontReset,
            Action::OpacityIncrease,
            Action::OpacityDecrease,
            Action::ToggleSearch,
            Action::ToggleCommandPalette,
            Action::ToggleSettings,
            Action::ReloadConfig,
            Action::ToggleSidebar,
            Action::SplitHorizontal,
            Action::SplitVertical,
            Action::PrevTab,
            Action::NextTab,
            Action::ScrollUp,
            Action::ScrollDown,
            Action::CyclePaneFocusForward,
            Action::CyclePaneFocusBackward,
            Action::ShowRemotePicker,
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
            Action::FilterTabs,
            Action::CloseSelectedTabs,
            Action::MoveTabLeft,
            Action::MoveTabRight,
            Action::DuplicateTab,
            Action::ToggleTabMarked,
            Action::ToggleTabPinned,
            Action::ToggleTabPlacement,
            Action::FilterFailedBlocks,
            Action::FilterSlowBlocks,
            Action::FilterPinnedBlocks,
            Action::ClearBlockFilter,
            Action::SelectAllBlocks,
            Action::ClearBlocks,
            Action::ReinputSelectedCommands,
            Action::JumpToPrevPinned,
            Action::JumpToNextPinned,
            Action::ToggleDebugDashboard,
            Action::ToggleAiPanel,
            Action::AskAiAboutSelectedBlock,
            Action::OpenAgent,
            Action::HistoryPalette,
            Action::CrossBlockSearch,
            Action::WorkflowsPalette,
            Action::OpenWelcome,
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
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("Empty key combo".to_string());
    }
    let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();

    // The last part is the key, but "+" itself is special:
    // "Ctrl+Shift++" means Ctrl+Shift and key is "+"
    let (mod_parts, key_str) = if trimmed.ends_with("++") && parts.len() >= 3 {
        (&parts[..parts.len() - 2], "+")
    } else if parts.last() == Some(&"") && parts.len() >= 2 {
        // "Ctrl++" case
        (&parts[..parts.len() - 2], "+")
    } else {
        (&parts[..parts.len() - 1], *parts.last().unwrap())
    };

    for part in mod_parts {
        match part.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= ModifierType::CONTROL_MASK,
            "shift" => modifiers |= ModifierType::SHIFT_MASK,
            "alt" => modifiers |= ModifierType::ALT_MASK,
            other => return Err(format!("Unknown modifier: {other}")),
        }
    }

    let key = match key_str {
        "+" => Key::plus,
        "=" => Key::equal,
        "-" => Key::minus,
        k if k.eq_ignore_ascii_case("plus") => Key::plus,
        k if k.eq_ignore_ascii_case("equal") => Key::equal,
        k if k.eq_ignore_ascii_case("minus") => Key::minus,
        k if k.eq_ignore_ascii_case("PageUp") => Key::Page_Up,
        k if k.eq_ignore_ascii_case("PageDown") => Key::Page_Down,
        k if k.eq_ignore_ascii_case("Tab") => Key::Tab,
        k if k.eq_ignore_ascii_case("Escape") || k.eq_ignore_ascii_case("Esc") => Key::Escape,
        k if k.eq_ignore_ascii_case("Return") || k.eq_ignore_ascii_case("Enter") => Key::Return,
        k if k.eq_ignore_ascii_case("Up") => Key::Up,
        k if k.eq_ignore_ascii_case("Down") => Key::Down,
        k if k.eq_ignore_ascii_case("Left") => Key::Left,
        k if k.eq_ignore_ascii_case("Right") => Key::Right,
        "!" => Key::exclam,
        k if k.eq_ignore_ascii_case("exclam") => Key::exclam,
        k if k.eq_ignore_ascii_case("Space") => Key::space,
        k if k.eq_ignore_ascii_case("Backspace") => Key::BackSpace,
        k if k.eq_ignore_ascii_case("Delete") => Key::Delete,
        k if k.eq_ignore_ascii_case("Home") => Key::Home,
        k if k.eq_ignore_ascii_case("End") => Key::End,
        k if k.eq_ignore_ascii_case("Insert") => Key::Insert,
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
        Key::equal => "=".to_string(),
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
        k => k
            .name()
            .map(|n| {
                let s = n.to_string();
                if s.len() == 1 {
                    s.to_uppercase()
                } else {
                    s
                }
            })
            .unwrap_or_else(|| "?".to_string()),
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
        // Keep zoom on the same low-strain row in all jterm variants.
        bind("Ctrl+equal", Action::FontIncrease);
        bind("Ctrl+minus", Action::FontDecrease);
        bind("Ctrl+0", Action::FontReset);
        bind("Ctrl+Alt+equal", Action::OpacityIncrease);
        bind("Ctrl+Alt+minus", Action::OpacityDecrease);
        bind("Ctrl+Shift+F", Action::ToggleSearch);
        bind("Ctrl+Shift+P", Action::ToggleCommandPalette);
        bind("Ctrl+Shift+O", Action::ToggleSettings);
        bind("Ctrl+Shift+R", Action::ReloadConfig);
        bind("Ctrl+backslash", Action::ToggleSidebar);
        bind("Ctrl+Shift+L", Action::FilterTabs);
        bind("Ctrl+Shift+X", Action::FilterFailedBlocks);
        bind("Ctrl+Shift+N", Action::ClearBlockFilter);
        bind("Ctrl+Shift+A", Action::SelectAllBlocks);
        bind("Ctrl+Shift+I", Action::ReinputSelectedCommands);
        bind("Ctrl+Shift+K", Action::ClearBlocks);
        // Keep Warp's Ctrl+Shift+B available for block bookmarks.
        bind("Ctrl+Alt+B", Action::ToggleTabPlacement);
        bind("Ctrl+Shift+E", Action::SplitHorizontal);
        bind("Ctrl+Shift+D", Action::SplitVertical);
        bind("Ctrl+Shift+Tab", Action::PrevTab);
        bind("Ctrl+Tab", Action::NextTab);
        bind("Ctrl+Up", Action::ScrollUp);
        bind("Ctrl+Down", Action::ScrollDown);
        bind("Ctrl+PageUp", Action::PrevTab);
        bind("Ctrl+PageDown", Action::NextTab);
        // Browser-style numbering: Ctrl+1..8 select those tabs, Ctrl+9 always
        // selects the last tab, and Ctrl+0 resets zoom.
        for digit in 1..=8u8 {
            bind(&format!("Ctrl+{digit}"), Action::QuickSwitchTab(digit - 1));
        }
        bind("Ctrl+9", Action::QuickSwitchTab(9));
        bind("Ctrl+Shift+S", Action::ShowRemotePicker);

        // One spatial layer across every terminal: add Shift for the rarer
        // resize operation. Ctrl+Alt reaches the app under JWM, unlike Alt-only.
        bind("Ctrl+Alt+Left", Action::FocusPaneLeft);
        bind("Ctrl+Alt+Right", Action::FocusPaneRight);
        bind("Ctrl+Alt+Up", Action::FocusPaneUp);
        bind("Ctrl+Alt+Down", Action::FocusPaneDown);
        bind("Ctrl+Alt+Shift+Left", Action::ResizePaneLeft);
        bind("Ctrl+Alt+Shift+Right", Action::ResizePaneRight);
        bind("Ctrl+Alt+Shift+Up", Action::ResizePaneUp);
        bind("Ctrl+Alt+Shift+Down", Action::ResizePaneDown);
        // GNOME (and some other DEs) grab Ctrl+Alt+arrows for workspace
        // switching before the app ever sees the event, so the spatial pane
        // operations also get vim-letter chords that no DE claims by default.
        bind("Ctrl+Alt+H", Action::FocusPaneLeft);
        bind("Ctrl+Alt+J", Action::FocusPaneDown);
        bind("Ctrl+Alt+K", Action::FocusPaneUp);
        bind("Ctrl+Alt+L", Action::FocusPaneRight);
        bind("Ctrl+Alt+Shift+H", Action::ResizePaneLeft);
        bind("Ctrl+Alt+Shift+J", Action::ResizePaneDown);
        bind("Ctrl+Alt+Shift+K", Action::ResizePaneUp);
        bind("Ctrl+Alt+Shift+L", Action::ResizePaneRight);
        bind("Ctrl+Shift+Z", Action::TogglePaneZoom);
        bind("Ctrl+Shift+!", Action::MovePaneToNewTab);
        bind("F12", Action::ToggleDebugDashboard);
        bind("Ctrl+Alt+Shift+A", Action::ToggleAiPanel);
        bind("Ctrl+Shift+Q", Action::AskAiAboutSelectedBlock);
        bind("Ctrl+Alt+G", Action::OpenAgent);
        // Ctrl+R is consumed by bash readline in the live VTE, so the chord
        // for our block-history palette is Ctrl+Shift+H ("history").
        bind("Ctrl+Shift+H", Action::HistoryPalette);
        // Ctrl+Shift+G — "grep" — cross-block ripgrep palette. Ctrl+Shift+F
        // already drives the within-block VTE highlighter (different UX).
        bind("Ctrl+Shift+G", Action::CrossBlockSearch);
        // Ctrl+Shift+M — "macros" / workflows palette over saved command
        // templates. (Ctrl+Shift+W is ClosePaneOrTab in browser muscle memory.)
        bind("Ctrl+Shift+M", Action::WorkflowsPalette);
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
            // `false`, an empty string, "none", and "disabled" intentionally
            // leave the action unbound. This makes it possible to resolve a
            // desktop/window-manager conflict without inventing a dummy chord.
            if value.as_bool() == Some(false) {
                self.bindings.retain(|_, a| *a != action);
                continue;
            }
            let Some(key_str) = value.as_str() else {
                log::warn!("Keybinding value for {config_key} must be a chord string or false");
                continue;
            };
            if key_str.trim().is_empty()
                || key_str.eq_ignore_ascii_case("none")
                || key_str.eq_ignore_ascii_case("disabled")
            {
                self.bindings.retain(|_, a| *a != action);
                continue;
            }

            let combo = match parse_key_combo(key_str) {
                Ok(combo) => combo,
                Err(e) => {
                    // A typo must not silently make the action unreachable.
                    log::warn!("Invalid keybinding '{key_str}' for {config_key}: {e}");
                    continue;
                }
            };
            if let Some(existing) = self.bindings.get(&combo).copied() {
                if existing != action {
                    // Keep both existing defaults instead of stealing another
                    // action's chord and leaving that action unreachable.
                    log::warn!(
                        "Keybinding '{key_str}' for {config_key} conflicts with '{}'",
                        existing.name()
                    );
                    continue;
                }
            }

            // Mutate only after parsing and conflict validation succeeded.
            self.bindings.retain(|_, a| *a != action);
            self.bindings.insert(combo, action);
        }
    }

    pub(crate) fn lookup(&self, combo: &KeyCombo) -> Option<Action> {
        self.bindings.get(combo).copied()
    }

    pub(crate) fn binding_display(&self, action: &Action) -> String {
        let combos: Vec<_> = self
            .bindings
            .iter()
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

#[cfg(test)]
mod tests {
    //! Table-driven regression tests over the default keybinding map. Pure
    //! data — no GTK runtime — so they run in CI without a display. They
    //! pin the two failure modes we've actually hit:
    //!
    //! - An action loses its default binding during a refactor (the
    //!   `bind!` call gets dropped or the `Action::` variant is renamed)
    //!   and silently becomes unreachable from the UI.
    //! - parse_key_combo / key_combo_to_string drift apart and config
    //!   round-trip breaks (`Ctrl+=`, digit keys, named keys).
    //!
    //! They do NOT cover the runtime "VTE swallow" question (whether the
    //! live VTE consumes a chord before the block-mode capture phase sees
    //! it) — that needs a GTK event loop and is tracked separately.
    use super::*;

    /// Every action advertised by `all_actions()` either has a default
    /// binding or is on the explicit "palette / TOML-only" allowlist
    /// below. The allowlist exists so a newly-added Action without a
    /// default still trips this test — forcing the author to either add
    /// a chord or consciously declare it palette-only.
    #[test]
    fn every_advertised_action_has_a_default_binding_or_is_allowlisted() {
        // Actions intentionally reachable only from the command palette
        // and/or user TOML overrides — they have no default chord on
        // purpose (chord exhaustion + low frequency of use).
        let palette_only: &[Action] = &[
            Action::CloseTab,
            Action::CyclePaneFocusForward,
            Action::CyclePaneFocusBackward,
            Action::CloseSelectedTabs,
            Action::MoveTabLeft,
            Action::MoveTabRight,
            Action::DuplicateTab,
            Action::ToggleTabMarked,
            Action::ToggleTabPinned,
            Action::OpenWelcome,
            Action::FilterSlowBlocks,
            Action::FilterPinnedBlocks,
            Action::JumpToPrevPinned,
            Action::JumpToNextPinned,
        ];

        let map = KeybindingMap::from_defaults();
        let bound_actions: std::collections::HashSet<Action> =
            map.bindings.values().copied().collect();
        let allowed: std::collections::HashSet<Action> = palette_only.iter().copied().collect();

        let missing: Vec<_> = Action::all_actions()
            .into_iter()
            .filter(|a| !bound_actions.contains(a) && !allowed.contains(a))
            .collect();
        assert!(
            missing.is_empty(),
            "actions advertised by all_actions() but neither bound nor \
             marked palette-only: {missing:?} — either add a default chord \
             in `from_defaults()` or extend the `palette_only` allowlist here."
        );

        // Symmetric guard: allowlist must not list actions that DO have a
        // default binding (otherwise the allowlist drifts and stops being
        // a source of truth).
        let stale: Vec<_> = palette_only
            .iter()
            .copied()
            .filter(|a| bound_actions.contains(a))
            .collect();
        assert!(
            stale.is_empty(),
            "palette_only allowlist contains actions that DO have a default \
             chord: {stale:?} — remove them from the allowlist."
        );
    }

    /// Defaults round-trip through the string parser. If
    /// key_combo_to_string emits a form parse_key_combo can't read, the
    /// user's settings export silently drops bindings on every reload.
    #[test]
    fn every_default_combo_round_trips_through_string_form() {
        let map = KeybindingMap::from_defaults();
        for (combo, action) in &map.bindings {
            let s = key_combo_to_string(combo);
            let parsed = parse_key_combo(&s)
                .unwrap_or_else(|e| panic!("round-trip failed for {action:?} → {s:?}: {e}"));
            assert_eq!(
                parsed, *combo,
                "round-trip mismatch for {action:?}: {combo:?} → {s:?} → {parsed:?}"
            );
        }
    }

    /// Frozen list of chord strings the docs and settings UI publish.
    /// Adding more accepted forms is fine; removing one breaks config
    /// files in the wild.
    #[test]
    fn published_chord_strings_all_parse() {
        let known_good = [
            "Ctrl+Shift+T",
            "Ctrl+Shift+W",
            "Ctrl+Shift+C",
            "Ctrl+Shift+V",
            "Ctrl+=",
            "Ctrl+Shift+!",
            "Ctrl+Alt+Shift+A",
            "Ctrl+Alt+=",
            "Ctrl+Alt+-",
            "Ctrl+backslash",
            "Ctrl+minus",
            "Ctrl+Up",
            "Ctrl+Down",
            "Ctrl+PageUp",
            "Ctrl+PageDown",
            "Ctrl+Tab",
            "Ctrl+Shift+Tab",
            "Ctrl+Alt+Left",
            "Ctrl+Alt+Right",
            "Ctrl+Alt+Up",
            "Ctrl+Alt+Down",
            "Ctrl+Alt+Shift+Left",
            "Ctrl+Alt+Shift+Right",
            "Ctrl+Alt+Shift+Up",
            "Ctrl+Alt+Shift+Down",
            "Ctrl+Alt+B",
            "F12",
            "Ctrl+0",
            "Ctrl+9",
        ];
        for s in known_good {
            assert!(
                parse_key_combo(s).is_ok(),
                "documented chord {s:?} must parse"
            );
        }
    }

    /// Load-bearing block-mode (chord → action) pairs. The list lives
    /// here on purpose: when you intentionally rebind one, you'll fix
    /// this test in the same commit and a reviewer can spot the change.
    #[test]
    fn frozen_block_mode_chord_table() {
        let map = KeybindingMap::from_defaults();
        let expectations: &[(&str, Action)] = &[
            // Block list scroll.
            ("Ctrl+Up", Action::ScrollUp),
            ("Ctrl+Down", Action::ScrollDown),
            // Pane focus (used in block mode to jump between paned block lists).
            ("Ctrl+Alt+Left", Action::FocusPaneLeft),
            ("Ctrl+Alt+Right", Action::FocusPaneRight),
            ("Ctrl+Alt+Up", Action::FocusPaneUp),
            ("Ctrl+Alt+Down", Action::FocusPaneDown),
            // Vim-letter fallbacks for DEs that grab Ctrl+Alt+arrows (GNOME
            // workspace switching).
            ("Ctrl+Alt+H", Action::FocusPaneLeft),
            ("Ctrl+Alt+J", Action::FocusPaneDown),
            ("Ctrl+Alt+K", Action::FocusPaneUp),
            ("Ctrl+Alt+L", Action::FocusPaneRight),
            // Block-discovery surface.
            ("Ctrl+Shift+F", Action::ToggleSearch),
            ("Ctrl+Shift+P", Action::ToggleCommandPalette),
            ("Ctrl+Shift+R", Action::ReloadConfig),
            ("Ctrl+Shift+S", Action::ShowRemotePicker),
            // Selection copy out of finished blocks.
            ("Ctrl+Shift+C", Action::Copy),
            // Tab placement / sidebar — adjacent to the block list.
            ("Ctrl+Alt+B", Action::ToggleTabPlacement),
            ("Ctrl+backslash", Action::ToggleSidebar),
            // Tab filter palette.
            ("Ctrl+Shift+L", Action::FilterTabs),
            ("Ctrl+Shift+X", Action::FilterFailedBlocks),
            ("Ctrl+Shift+N", Action::ClearBlockFilter),
            // jterm1/Warp block actions.
            ("Ctrl+Shift+A", Action::SelectAllBlocks),
            ("Ctrl+Shift+I", Action::ReinputSelectedCommands),
            ("Ctrl+Shift+K", Action::ClearBlocks),
            // AI sidebar keeps a non-conflicting chord.
            ("Ctrl+Alt+Shift+A", Action::ToggleAiPanel),
            ("Ctrl+Shift+Q", Action::AskAiAboutSelectedBlock),
            ("Ctrl+Alt+G", Action::OpenAgent),
            // Block-history palette (Ctrl+R is bash readline, so we use Ctrl+Shift+H).
            ("Ctrl+Shift+H", Action::HistoryPalette),
            // Cross-block ripgrep palette.
            ("Ctrl+Shift+G", Action::CrossBlockSearch),
            // Workflows palette (parameterized templates).
            ("Ctrl+Shift+M", Action::WorkflowsPalette),
        ];
        for (chord, want_action) in expectations {
            let combo = parse_key_combo(chord).expect("chord must parse");
            match map.lookup(&combo) {
                Some(actual) => assert_eq!(
                    actual, *want_action,
                    "{chord} expected {want_action:?}, got {actual:?}"
                ),
                None => panic!("{chord} is unbound in the default map"),
            }
        }
    }

    /// Shared ergonomic contract used by jterm1..4. Project-specific actions
    /// may add chords, but these common actions must never drift again.
    #[test]
    fn common_default_chord_table() {
        let map = KeybindingMap::from_defaults();
        let expectations = [
            ("Ctrl+Shift+T", Action::NewTab),
            ("Ctrl+Shift+W", Action::ClosePaneOrTab),
            ("Ctrl+Shift+C", Action::Copy),
            ("Ctrl+Shift+V", Action::Paste),
            ("Ctrl+Shift+F", Action::ToggleSearch),
            ("Ctrl+Shift+P", Action::ToggleCommandPalette),
            ("Ctrl+Shift+O", Action::ToggleSettings),
            ("Ctrl+backslash", Action::ToggleSidebar),
            ("Ctrl+Shift+E", Action::SplitHorizontal),
            ("Ctrl+Shift+D", Action::SplitVertical),
            ("Ctrl+Alt+Left", Action::FocusPaneLeft),
            ("Ctrl+Alt+Right", Action::FocusPaneRight),
            ("Ctrl+Alt+Up", Action::FocusPaneUp),
            ("Ctrl+Alt+Down", Action::FocusPaneDown),
            ("Ctrl+Alt+Shift+Left", Action::ResizePaneLeft),
            ("Ctrl+Alt+Shift+Right", Action::ResizePaneRight),
            ("Ctrl+Alt+Shift+Up", Action::ResizePaneUp),
            ("Ctrl+Alt+Shift+Down", Action::ResizePaneDown),
            ("Ctrl+Alt+H", Action::FocusPaneLeft),
            ("Ctrl+Alt+J", Action::FocusPaneDown),
            ("Ctrl+Alt+K", Action::FocusPaneUp),
            ("Ctrl+Alt+L", Action::FocusPaneRight),
            ("Ctrl+Alt+Shift+H", Action::ResizePaneLeft),
            ("Ctrl+Alt+Shift+J", Action::ResizePaneDown),
            ("Ctrl+Alt+Shift+K", Action::ResizePaneUp),
            ("Ctrl+Alt+Shift+L", Action::ResizePaneRight),
            ("Ctrl+Tab", Action::NextTab),
            ("Ctrl+Shift+Tab", Action::PrevTab),
            ("Ctrl+=", Action::FontIncrease),
            ("Ctrl+-", Action::FontDecrease),
            ("Ctrl+0", Action::FontReset),
            ("F12", Action::ToggleDebugDashboard),
        ];
        for (chord, expected) in expectations {
            let combo = parse_key_combo(chord).expect("common chord must parse");
            assert_eq!(map.lookup(&combo), Some(expected), "{chord}");
        }
    }

    /// Quick-switch digits follow browser muscle memory: 1..8 are direct and
    /// 9 always selects the last tab. Ctrl+0 belongs to font reset.
    #[test]
    fn browser_style_ctrl_digit_shortcuts_are_stable() {
        let map = KeybindingMap::from_defaults();
        for digit in 1u8..=8 {
            let chord = format!("Ctrl+{digit}");
            let combo = parse_key_combo(&chord).expect("digit chord must parse");
            match map.lookup(&combo) {
                Some(Action::QuickSwitchTab(got)) => assert_eq!(got, digit - 1),
                other => panic!(
                    "{chord} expected QuickSwitchTab({}), got {other:?}",
                    digit - 1
                ),
            }
        }
        let nine = parse_key_combo("Ctrl+9").expect("digit chord must parse");
        assert_eq!(map.lookup(&nine), Some(Action::QuickSwitchTab(9)));
        let zero = parse_key_combo("Ctrl+0").expect("digit chord must parse");
        assert_eq!(map.lookup(&zero), Some(Action::FontReset));
    }

    /// `+` is also the chord separator, so it needs special-casing in
    /// parse_key_combo. Both documented forms must produce the same combo.
    #[test]
    fn plus_key_chord_special_cases_agree() {
        let a = parse_key_combo("Ctrl+Shift++").expect("'Ctrl+Shift++' form");
        let b = parse_key_combo("Ctrl+Shift+plus").expect("'Ctrl+Shift+plus' form");
        assert_eq!(a, b);
    }

    #[test]
    fn equal_key_has_config_and_display_aliases() {
        let symbolic = parse_key_combo("Ctrl+=").expect("'Ctrl+=' form");
        let named = parse_key_combo("Ctrl+equal").expect("'Ctrl+equal' form");
        assert_eq!(symbolic, named);
        assert_eq!(key_combo_to_string(&symbolic), "Ctrl+=");
    }

    /// Shift+Tab arrives from GTK as ISO_Left_Tab. normalize_key rewrites
    /// it to Tab so a single chord entry covers both; the display must
    /// also surface as Tab so the settings UI shows one canonical form.
    #[test]
    fn shift_tab_normalises_to_tab_in_display() {
        let combo = parse_key_combo("Ctrl+Shift+Tab").expect("must parse");
        let s = key_combo_to_string(&combo);
        assert!(s.ends_with("+Tab"), "expected ends-with `+Tab`, got {s:?}");
    }

    /// Each Action variant's config_key must be unique — otherwise the
    /// TOML override path silently rebinds the wrong action.
    #[test]
    fn config_keys_are_unique_across_actions() {
        let mut seen: HashMap<&'static str, Action> = HashMap::new();
        for action in Action::all_actions() {
            if let Some(key) = action.config_key() {
                if let Some(prev) = seen.insert(key, action) {
                    panic!(
                        "config_key {key:?} reused: {prev:?} vs {action:?} — \
                         TOML override path would silently rebind one of them"
                    );
                }
            }
        }
    }

    /// User-override path: applying a one-entry table drops the old
    /// binding and installs the new one. Guards against the rebind path
    /// losing its "drop old binding" step.
    #[test]
    fn user_override_replaces_default_binding() {
        let mut map = KeybindingMap::from_defaults();

        // ScrollUp is bound to Ctrl+Up by default; remap to F11.
        let mut table = toml::Table::new();
        table.insert("scroll_up".into(), toml::Value::String("F11".into()));
        map.apply_user_overrides(&table);

        let old = parse_key_combo("Ctrl+Up").unwrap();
        let new = parse_key_combo("F11").unwrap();
        assert_eq!(map.lookup(&old), None, "old default must be removed");
        assert_eq!(map.lookup(&new), Some(Action::ScrollUp));
    }

    #[test]
    fn modifier_names_are_case_insensitive() {
        assert_eq!(
            parse_key_combo("control+shift+t").unwrap(),
            parse_key_combo("Ctrl+Shift+T").unwrap()
        );
    }

    #[test]
    fn parser_trims_whitespace_and_accepts_case_insensitive_named_keys() {
        assert_eq!(
            parse_key_combo("  control + shift + pageup  ").unwrap(),
            parse_key_combo("Ctrl+Shift+PageUp").unwrap()
        );
        assert_eq!(
            parse_key_combo("ctrl+BACKSPACE").unwrap(),
            parse_key_combo("Ctrl+Backspace").unwrap()
        );
    }

    #[test]
    fn invalid_override_keeps_default_binding() {
        let mut map = KeybindingMap::from_defaults();
        let original = parse_key_combo("Ctrl+Shift+T").unwrap();
        let table = "new_tab = 'Ctrl+NoSuchModifier+T'"
            .parse::<toml::Table>()
            .unwrap();
        map.apply_user_overrides(&table);
        assert_eq!(map.lookup(&original), Some(Action::NewTab));
    }

    #[test]
    fn conflicting_override_keeps_both_defaults() {
        let mut map = KeybindingMap::from_defaults();
        let new_tab = parse_key_combo("Ctrl+Shift+T").unwrap();
        let paste = parse_key_combo("Ctrl+Shift+V").unwrap();
        let table = "new_tab = 'Ctrl+Shift+V'".parse::<toml::Table>().unwrap();
        map.apply_user_overrides(&table);
        assert_eq!(map.lookup(&new_tab), Some(Action::NewTab));
        assert_eq!(map.lookup(&paste), Some(Action::Paste));
    }

    #[test]
    fn user_can_explicitly_disable_a_binding() {
        let mut map = KeybindingMap::from_defaults();
        let original = parse_key_combo("Ctrl+Up").unwrap();
        let mut table = toml::Table::new();
        table.insert("scroll_up".into(), toml::Value::Boolean(false));
        map.apply_user_overrides(&table);
        assert_eq!(map.lookup(&original), None);
    }
    #[test]
    fn jterm1_block_action_defaults_are_not_shadowed() {
        let map = KeybindingMap::from_defaults();
        let cases = [
            ("Ctrl+Shift+A", Action::SelectAllBlocks),
            ("Ctrl+Shift+I", Action::ReinputSelectedCommands),
            ("Ctrl+Shift+K", Action::ClearBlocks),
            ("Ctrl+Alt+Shift+A", Action::ToggleAiPanel),
            ("Ctrl+Alt+=", Action::OpacityIncrease),
        ];
        for (binding, expected) in cases {
            let combo = parse_key_combo(binding).expect("valid built-in binding");
            assert_eq!(map.lookup(&combo), Some(expected), "{binding}");
        }
    }
}
