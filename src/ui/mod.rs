use gtk4::Notebook;
use gtk4::{CssProvider, SearchBar, SearchEntry};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use vte4::Terminal;

use crate::config::{Config, Theme};
use crate::keybindings::KeybindingMap;
use crate::block_view::TermView;
use crate::terminal::VteTerminalView;

mod actions;
mod config_apply;
mod dialogs;
mod panes;
mod search;
mod session;
mod tab_strip;
mod tabs;
mod zoom;


#[derive(Clone)]
pub(crate) enum TerminalViewType {
    Block(Rc<TermView>),
    Vte(Rc<VteTerminalView>),
}

pub(crate) struct ZoomState {
    pub(crate) original_page: gtk4::Widget,
    pub(crate) zoomed_terminal: Terminal,
    pub(crate) page_index: u32,
    pub(crate) tab_label: Option<gtk4::Widget>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnStatus {
    Connecting,
    Connected,
    Disconnected,
}

/// Per-tab record for a remote (ssh) connection, enabling status display and
/// auto-reconnect. Keyed by tab_num in `UiState::tab_connections`.
#[derive(Clone)]
pub(crate) struct TabConnection {
    /// The host this tab connects to — used to rebuild the same argv (and thus
    /// the same remote `--session` id) on reconnect.
    pub(crate) host: crate::config::RemoteHost,
    pub(crate) status: ConnStatus,
    /// Reconnect backoff counter; a session that stayed up long enough resets it.
    pub(crate) attempt: u32,
    /// When this connection attempt was spawned — used to distinguish a brief
    /// failed handshake (grow backoff) from a long-lived session that dropped
    /// (reset backoff).
    pub(crate) spawn_at: std::time::Instant,
}

#[derive(Clone)]
pub(crate) struct UiState {
    pub(crate) window: adw::ApplicationWindow,
    pub(crate) notebook: Notebook,
    pub(crate) tab_counter: Rc<Cell<u32>>,
    pub(crate) font_scale: Rc<Cell<f64>>,
    pub(crate) window_opacity: Rc<Cell<f64>>,
    pub(crate) shell_argv: Rc<Vec<String>>,
    pub(crate) config: Rc<RefCell<Config>>,
    pub(crate) available_themes: Rc<Vec<Theme>>,
    pub(crate) search_bar: SearchBar,
    pub(crate) search_entry: SearchEntry,
    pub(crate) tab_strip: gtk4::Box,
    pub(crate) sidebar: gtk4::Box,
    pub(crate) tab_search_entry: SearchEntry,
    pub(crate) selected_tabs: Rc<RefCell<Vec<String>>>,
    pub(crate) command_palette_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) remote_picker_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) settings_dialog: Rc<RefCell<Option<adw::PreferencesDialog>>>,
    pub(crate) debug_dashboard_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) keybinding_map: Rc<RefCell<KeybindingMap>>,
    pub(crate) zoom_state: Rc<RefCell<Option<ZoomState>>>,
    pub(crate) scrollbar_css: CssProvider,
    /// Maps tab_num → session_id for rsh session persistence.
    pub(crate) session_ids: Rc<RefCell<HashMap<u32, String>>>,
    /// Maps tab_num → remote connection record (status + reconnect info).
    pub(crate) tab_connections: Rc<RefCell<HashMap<u32, TabConnection>>>,
}
