use gtk4::Notebook;
use gtk4::{CssProvider, ScrolledWindow, SearchBar, SearchEntry, Stack, ToggleButton};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use vte4::Terminal;

use crate::config::{Config, SidebarView, TabPlacement, Theme};
use crate::keybindings::KeybindingMap;

mod actions;
mod agent_panel;
mod ai_panel;
mod command_palette;
mod config_apply;
mod dialogs;
mod file_tree;
mod layout;
mod notebooks;
mod pane_leaf;
mod pane_node;
mod pane_tree_edit;
mod panes;
mod search;
mod session;
mod tab_strip;
mod tabs;
mod zoom;

pub(crate) use ai_panel::AiPanel;
pub(crate) use file_tree::{build_file_tree_widgets, FileTreeModel};
pub(crate) use pane_leaf::PaneLeaf;
pub(crate) use pane_node::PaneNode;
pub(crate) use pane_tree_edit::{
    detach_leaf_and_promote, detach_leaf_for_zoom, restore_zoomed_leaf, ZoomPageSwap,
};

pub(crate) struct ZoomState {
    pub(crate) swap: ZoomPageSwap,
    pub(crate) zoomed_terminal: Terminal,
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
    /// Stable identity for this concrete connection attempt. The map key may
    /// change when its pane moves to another tab while a reconnect timer is
    /// pending, but this value moves with the record.
    pub(crate) identity: u32,
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
    pub(crate) shell_argv: Rc<RefCell<Vec<String>>>,
    pub(crate) config: Rc<RefCell<Config>>,
    pub(crate) available_themes: Rc<Vec<Theme>>,
    pub(crate) search_bar: SearchBar,
    pub(crate) search_entry: SearchEntry,
    pub(crate) tab_strip: gtk4::Box,
    pub(crate) sidebar: gtk4::Box,
    /// Flexible spacer in the top bar; its hexpand toggles with tab placement.
    pub(crate) top_spacer: gtk4::Box,
    /// Sidebar scroll holder for the (vertical) tab strip.
    pub(crate) tab_strip_scroll: ScrolledWindow,
    /// Top-bar scroll holder for the (horizontal) tab strip.
    pub(crate) top_tab_scroll: ScrolledWindow,
    /// Current tab placement (sidebar vs top bar).
    pub(crate) tab_placement: Rc<Cell<TabPlacement>>,
    /// Sidebar content stack (one of: tab list, file tree).
    pub(crate) sidebar_stack: Stack,
    pub(crate) sidebar_tabs_btn: ToggleButton,
    pub(crate) sidebar_files_btn: ToggleButton,
    /// Which sidebar view the user prefers (persisted).
    pub(crate) sidebar_view: Rc<Cell<SidebarView>>,
    pub(crate) file_tree_model: FileTreeModel,
    pub(crate) file_tree_root: Rc<RefCell<PathBuf>>,
    pub(crate) file_tree_root_label: gtk4::Label,
    pub(crate) tab_search_entry: SearchEntry,
    pub(crate) selected_tabs: Rc<RefCell<Vec<String>>>,
    pub(crate) command_palette_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) remote_picker_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) history_palette_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) cross_block_search_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) workflows_palette_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) settings_dialog: Rc<RefCell<Option<adw::PreferencesDialog>>>,
    pub(crate) debug_dashboard_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) agent_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    /// Suppresses a storm of identical persistence alerts while a continuous
    /// setting (opacity/font size) emits multiple change notifications.
    pub(crate) config_save_error_visible: Rc<Cell<bool>>,
    pub(crate) keybinding_map: Rc<RefCell<KeybindingMap>>,
    pub(crate) zoom_state: Rc<RefCell<Option<ZoomState>>>,
    pub(crate) scrollbar_css: CssProvider,
    /// Maps tab_num → session_id for rsh session persistence.
    pub(crate) session_ids: Rc<RefCell<HashMap<u32, String>>>,
    /// Maps tab_num → remote connection record (status + reconnect info).
    pub(crate) tab_connections: Rc<RefCell<HashMap<u32, TabConnection>>>,
    /// Right-side AI chat panel. Always built; visibility lives in the
    /// outer `ai_paned` (and `config.ai_panel_visible` for persistence).
    pub(crate) ai_panel: AiPanel,
    /// Horizontal Paned that puts the AI panel to the right of the notebook
    /// area. Toggling visibility flips the end child + resize start_child.
    pub(crate) ai_paned: gtk4::Paned,
    pub(crate) ai_panel_visible: Rc<Cell<bool>>,
    /// Suppresses divider notifications caused by restoring a configured
    /// width; only user-driven positions should flow back into Config.
    pub(crate) ai_panel_width_restoring: Rc<Cell<bool>>,
}
