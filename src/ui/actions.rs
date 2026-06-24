//! actions — UiState methods extracted from ui (mechanical split, no logic changes)
use gtk4::Orientation;
use libadwaita as adw;
use adw::prelude::*;
use std::rc::Rc;
use vte4::Format;
use vte4::{Terminal};
use vte4::TerminalExt;

use crate::keybindings::{Action, Direction};
use crate::block_view::TermView;
use crate::terminal::{
    terminal_working_directory,
    find_first_terminal, find_focused_terminal,
};
use super::*;

impl UiState {
    pub(crate) fn execute_action(&self, action: Action) {
        let font_step = 0.025;
        let opacity_step = 0.025;
        let current_terminal = self.current_terminal();

        match action {
            Action::NewTab => {
                log::info!("New tab");
                let working_directory = current_terminal
                    .as_ref()
                    .and_then(terminal_working_directory);
                let startup = self.config.borrow().startup_commands.clone();
                self.add_new_tab(working_directory, None, None, startup);
            }
            Action::CloseTab => {
                log::info!("Close tab");
                self.remove_current_tab();
            }
            Action::ClosePaneOrTab => {
                log::info!("Close focused pane or tab");
                self.close_focused_pane_or_tab();
            }
            Action::Copy => {
                log::debug!(">>> UI Action::Copy triggered");
                if let Some(term_view) = self.current_term_view() {
                    log::debug!(">>> UI Copy: calling term_view.copy_to_clipboard");
                    term_view.copy_to_clipboard();
                } else {
                    log::debug!(">>> UI Copy: no current term_view, falling back to VTE");
                    if let Some(ref term) = current_terminal {
                        term.copy_clipboard_format(Format::Text);
                    }
                }
            }
            Action::Paste => {
                log::debug!(">>> UI Action::Paste triggered");
                if let Some(term_view) = self.current_term_view() {
                    log::debug!(">>> UI Paste: calling term_view.paste_from_clipboard");
                    term_view.paste_from_clipboard();
                } else {
                    log::debug!(">>> UI Paste: no current term_view, falling back to VTE");
                    if let Some(ref term) = current_terminal {
                        term.paste_clipboard();
                    }
                }
            }
            Action::FontIncrease => {
                log::debug!("Font increase");
                let new_scale = (self.font_scale.get() + font_step).min(10.0);
                self.set_font_scale_all(new_scale);
            }
            Action::FontDecrease => {
                log::debug!("Font decrease");
                let new_scale = (self.font_scale.get() - font_step).max(0.1);
                self.set_font_scale_all(new_scale);
            }
            Action::OpacityIncrease => {
                log::debug!("Opacity increase");
                self.window_opacity
                    .set((self.window_opacity.get() + opacity_step).clamp(0.01, 1.0));
                self.window.set_opacity(self.window_opacity.get());
            }
            Action::OpacityDecrease => {
                log::debug!("Opacity decrease");
                self.window_opacity
                    .set((self.window_opacity.get() - opacity_step).clamp(0.01, 1.0));
                self.window.set_opacity(self.window_opacity.get());
            }
            Action::ToggleSearch => {
                log::debug!("Toggle search");
                self.toggle_search();
            }
            Action::ToggleCommandPalette => {
                log::debug!("Toggle command palette");
                self.toggle_command_palette();
            }
            Action::ToggleSettings => {
                log::debug!("Toggle settings panel");
                self.toggle_settings_panel();
            }
            Action::ToggleSidebar => {
                log::debug!("Toggle sidebar");
                self.toggle_sidebar();
            }
            Action::FilterTabs => {
                log::debug!("Filter tabs");
                self.sidebar.set_visible(true);
                self.tab_search_entry.set_can_focus(true);
                self.tab_search_entry.set_focusable(true);
                self.tab_search_entry.grab_focus();
            }
            Action::CloseSelectedTabs => {
                log::debug!("Close selected tabs");
                self.close_selected_tabs();
            }
            Action::SplitHorizontal => {
                log::debug!("Split horizontal");
                self.split_current(Orientation::Horizontal);
            }
            Action::SplitVertical => {
                log::debug!("Split vertical");
                self.split_current(Orientation::Vertical);
            }
            Action::PrevTab => {
                self.switch_tab(-1);
            }
            Action::NextTab => {
                self.switch_tab(1);
            }
            Action::ScrollUp => {
                if let Some(ref term) = current_terminal {
                    if let Some(adj) = term.vadjustment() {
                        let new_val = (adj.value() - adj.step_increment() * 3.0).max(adj.lower());
                        adj.set_value(new_val);
                    }
                }
            }
            Action::ScrollDown => {
                if let Some(ref term) = current_terminal {
                    if let Some(adj) = term.vadjustment() {
                        let max_val = adj.upper() - adj.page_size();
                        let new_val = (adj.value() + adj.step_increment() * 3.0).min(max_val);
                        adj.set_value(new_val);
                    }
                }
            }
            Action::CyclePaneFocusForward => {
                self.cycle_pane_focus(1);
            }
            Action::CyclePaneFocusBackward => {
                self.cycle_pane_focus(-1);
            }
            Action::QuickSwitchTab(n) => {
                let n_pages = self.notebook.n_pages();
                if n_pages > 0 {
                    let target = if n == 9 {
                        n_pages - 1
                    } else {
                        (n as u32).min(n_pages - 1)
                    };
                    self.notebook.set_current_page(Some(target));
                }
            }
            Action::ShowRemotePicker => {
                self.show_remote_picker();
            }
            Action::ResizePaneLeft => {
                self.resize_pane(Orientation::Horizontal, -30);
            }
            Action::ResizePaneRight => {
                self.resize_pane(Orientation::Horizontal, 30);
            }
            Action::ResizePaneUp => {
                self.resize_pane(Orientation::Vertical, -30);
            }
            Action::ResizePaneDown => {
                self.resize_pane(Orientation::Vertical, 30);
            }
            Action::TogglePaneZoom => {
                self.toggle_pane_zoom();
            }
            Action::MovePaneToNewTab => {
                self.move_pane_to_new_tab();
            }
            Action::FocusPaneLeft => {
                self.focus_pane_directional(Direction::Left);
            }
            Action::FocusPaneRight => {
                self.focus_pane_directional(Direction::Right);
            }
            Action::FocusPaneUp => {
                self.focus_pane_directional(Direction::Up);
            }
            Action::FocusPaneDown => {
                self.focus_pane_directional(Direction::Down);
            }
            Action::MoveTabLeft => {
                self.move_tab_left();
            }
            Action::MoveTabRight => {
                self.move_tab_right();
            }
            Action::DuplicateTab => {
                self.duplicate_current_tab();
            }
            Action::ToggleTabMarked => {
                self.toggle_current_tab_marked();
            }
            Action::ToggleTabPinned => {
                self.toggle_current_tab_pinned();
            }
            Action::ToggleTabPlacement => {
                self.toggle_tab_placement();
            }
            Action::FilterFailedBlocks => {
                log::info!("Filter failed blocks");
                if let Some(term_view) = self.current_term_view() {
                    let failed_indices = term_view.get_failed_blocks();
                    log::info!("Found {} failed blocks", failed_indices.len());
                    // TODO: Highlight or show only these blocks in UI
                    if !failed_indices.is_empty() {
                        term_view.scroll_to_block(failed_indices[0]);
                    }
                }
            }
            Action::FilterSlowBlocks => {
                log::info!("Filter slow blocks");
                if let Some(term_view) = self.current_term_view() {
                    let slow_threshold = 1000; // 1 second
                    let slow_indices = term_view.get_slow_blocks(slow_threshold);
                    log::info!("Found {} slow blocks (>{}ms)", slow_indices.len(), slow_threshold);
                    if !slow_indices.is_empty() {
                        term_view.scroll_to_block(slow_indices[0]);
                    }
                }
            }
            Action::ClearBlockFilter => {
                log::info!("Clear block filter");
                if let Some(term_view) = self.current_term_view() {
                    term_view.scroll_to_block(0);
                }
            }
            Action::ToggleDebugDashboard => {
                log::debug!("Toggle debug dashboard");
                self.toggle_debug_dashboard();
            }
            Action::ToggleAiPanel => {
                log::debug!("Toggle AI panel");
                self.toggle_ai_panel();
            }
            Action::AskAiAboutSelectedBlock => {
                log::debug!("Ask AI about selected block");
                self.ask_ai_about_selected_block();
            }
            Action::HistoryPalette => {
                log::debug!("Show history palette");
                self.show_history_palette();
            }
            Action::CrossBlockSearch => {
                log::debug!("Show cross-block search palette");
                self.show_cross_block_search();
            }
        }
    }

    /// Show or hide the right-side AI chat panel. Persists the choice in
    /// `config.ai_panel_visible` so the panel state survives restart.
    pub(crate) fn toggle_ai_panel(&self) {
        let next = !self.ai_panel_visible.get();
        self.ai_panel_visible.set(next);
        if next {
            self.ai_paned.set_end_child(Some(&self.ai_panel.root));
            // Place the splitter so the AI panel gets its configured width
            // (Paned position is measured from the left edge).
            let width = self.ai_paned.width();
            let panel_w = self.config.borrow().ai_panel_width as i32;
            if width > panel_w + 200 {
                self.ai_paned.set_position(width - panel_w);
            }
        } else {
            self.ai_paned.set_end_child(None::<&gtk4::Widget>);
        }
        self.config.borrow_mut().ai_panel_visible = next;
        crate::config::save_config(&self.config.borrow());
    }

    /// Grab the selected block's context (cmd + output + cwd + exit) from
    /// the active TermView and hand it to the AI panel. Opens the panel
    /// first if it's hidden; no-ops cleanly when nothing's selected or the
    /// active tab is VTE-mode.
    pub(crate) fn ask_ai_about_selected_block(&self) {
        let Some(term_view) = self.current_term_view() else {
            log::debug!("AI: no active block-mode tab");
            return;
        };
        let Some(ctx) = term_view.selected_block_context(80) else {
            log::debug!("AI: no block selected");
            return;
        };
        if !self.ai_panel_visible.get() {
            self.toggle_ai_panel();
        }
        self.ai_panel.ask_about_block(ctx);
    }

    pub(crate) fn focus_current_terminal(&self) {
        if let Some(page) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page)) {
                if let Some(term) = find_first_terminal(&widget) {
                    term.grab_focus();
                }
            }
        }
    }

    pub(crate) fn current_terminal(&self) -> Option<Terminal> {
        self.notebook.current_page().and_then(|page_num| {
            self.notebook.nth_page(Some(page_num)).and_then(|widget| {
                // Try focused terminal first (for split panes), then fall back to first terminal
                find_focused_terminal(&widget).or_else(|| find_first_terminal(&widget))
            })
        })
    }

    pub(crate) fn current_terminal_view_type(&self) -> Option<TerminalViewType> {
        self.notebook.current_page().and_then(|page_num| {
            self.notebook.nth_page(Some(page_num)).and_then(|widget| {
                // SAFETY: data() returns a NonNull to data we stored on the widget
                unsafe {
                    widget.data::<TerminalViewType>("terminal-view-type")
                        .map(|ptr| ptr.as_ref().clone())
                }
            })
        })
    }

    pub(crate) fn current_term_view(&self) -> Option<Rc<TermView>> {
        match self.current_terminal_view_type() {
            Some(TerminalViewType::Block(term_view)) => Some(term_view),
            _ => None,
        }
    }
}
