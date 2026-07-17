//! actions — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::glib;
use gtk4::Orientation;
use libadwaita as adw;
use std::rc::Rc;
use vte4::Format;
use vte4::Terminal;
use vte4::TerminalExt;

use super::*;
use crate::block_view::TermView;
use crate::keybindings::{Action, Direction};
use crate::terminal::terminal_working_directory;

const MIN_AI_PANEL_WIDTH: i32 = 240;
const MAX_AI_PANEL_WIDTH: i32 = 1200;
const MIN_AI_WORKSPACE_WIDTH: i32 = 200;

fn apply_ai_panel_width(paned: &gtk4::Paned, requested_width: u32) {
    let total_width = paned.width();
    let Some(position) = restored_ai_panel_position(total_width, requested_width) else {
        return;
    };
    paned.set_position(position);
}

fn restored_ai_panel_position(total_width: i32, requested_width: u32) -> Option<i32> {
    if total_width <= MIN_AI_PANEL_WIDTH + MIN_AI_WORKSPACE_WIDTH {
        return None;
    }
    let available = total_width - MIN_AI_WORKSPACE_WIDTH;
    let panel_width = (requested_width as i32).clamp(MIN_AI_PANEL_WIDTH, available);
    Some(total_width - panel_width)
}

fn ai_panel_width_from_geometry(total_width: i32, position: i32) -> Option<u32> {
    if total_width <= 0 || position < 0 || position >= total_width {
        return None;
    }
    Some((total_width - position).clamp(MIN_AI_PANEL_WIDTH, MAX_AI_PANEL_WIDTH) as u32)
}

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
            Action::FontReset => {
                log::debug!("Font reset");
                self.set_font_scale_all(1.0);
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
                self.toggle_unified_command_palette();
            }
            Action::ToggleSettings => {
                log::debug!("Toggle settings panel");
                self.toggle_settings_panel();
            }
            Action::ReloadConfig => {
                log::info!("Reload configuration");
                self.reload_config();
            }
            Action::ToggleSidebar => {
                log::debug!("Toggle sidebar");
                self.toggle_sidebar();
            }
            Action::FilterTabs => {
                log::debug!("Filter tabs");
                self.set_sidebar_visible(true, true);
                // The search entry lives on the Tabs sidebar page even when
                // tabs themselves are placed in the top bar. Show that page so
                // the focused entry is never invisible.
                self.apply_sidebar_view(crate::config::SidebarView::Tabs, false);
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
                if let Some(term_view) = self.current_term_view() {
                    term_view.scroll_lines(-3);
                } else if let Some(ref term) = current_terminal {
                    if let Some(adj) = term.vadjustment() {
                        let new_val = (adj.value() - adj.step_increment() * 3.0).max(adj.lower());
                        adj.set_value(new_val);
                    }
                }
            }
            Action::ScrollDown => {
                if let Some(term_view) = self.current_term_view() {
                    term_view.scroll_lines(3);
                } else if let Some(ref term) = current_terminal {
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
                log::info!("Jump to first failed block");
                if let Some(term_view) = self.current_term_view() {
                    term_view.apply_failed_filter();
                }
            }
            Action::FilterSlowBlocks => {
                log::info!("Jump to first slow block");
                if let Some(term_view) = self.current_term_view() {
                    term_view.apply_slow_filter();
                }
            }
            Action::FilterPinnedBlocks => {
                log::info!("Jump to first bookmarked block");
                if let Some(term_view) = self.current_term_view() {
                    term_view.apply_pinned_filter();
                }
            }
            Action::ClearBlockFilter => {
                log::info!("Jump to oldest block");
                if let Some(term_view) = self.current_term_view() {
                    term_view.clear_block_filter();
                }
            }
            Action::SelectAllBlocks => {
                log::info!("Select all finished blocks");
                if let Some(term_view) = self.current_term_view() {
                    term_view.select_all_blocks();
                }
            }
            Action::ClearBlocks => {
                log::info!("Clear finished blocks");
                if let Some(term_view) = self.current_term_view() {
                    term_view.clear_blocks();
                }
            }
            Action::ReinputSelectedCommands => {
                log::info!("Reinput selected commands");
                if let Some(term_view) = self.current_term_view() {
                    term_view.reinput_selected_commands();
                }
            }
            Action::JumpToPrevPinned => {
                if let Some(term_view) = self.current_term_view() {
                    term_view.jump_to_pinned(-1);
                }
            }
            Action::JumpToNextPinned => {
                if let Some(term_view) = self.current_term_view() {
                    term_view.jump_to_pinned(1);
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
            Action::OpenAgent => self.toggle_agent_panel(),
            Action::HistoryPalette => {
                log::debug!("Show history palette");
                self.show_history_palette();
            }
            Action::CrossBlockSearch => {
                log::debug!("Show cross-block search palette");
                self.show_cross_block_search();
            }
            Action::WorkflowsPalette => {
                log::debug!("Show workflows palette");
                self.show_workflows_palette();
            }
            Action::OpenWelcome => self.open_welcome_notebook(),
        }
    }

    /// Show or hide the right-side AI chat panel. Persists the choice in
    /// `config.ai_panel_visible` so the panel state survives restart.
    pub(crate) fn toggle_ai_panel(&self) {
        let next = !self.ai_panel_visible.get();
        if next && !self.config.borrow().ai_enabled {
            self.show_ai_error("AI features are disabled in Settings or safe mode.");
            return;
        }
        if !next {
            // Capture the divider before detaching the end child; once hidden,
            // Paned no longer exposes the panel's allocated width.
            self.capture_ai_panel_width();
        }
        self.ai_panel_visible.set(next);
        if next {
            self.ai_paned.set_end_child(Some(&self.ai_panel.root));
            self.restore_ai_panel_width();
            self.ai_panel.focus_input();
        } else {
            self.ai_paned.set_end_child(None::<&gtk4::Widget>);
            self.focus_current_terminal();
        }
        self.config.borrow_mut().ai_panel_visible = next;
        self.persist_config();
    }

    /// Restore the configured end-child width after GTK has allocated the
    /// Paned. The idle retry covers startup, config reload, and re-showing the
    /// panel before the current layout pass has completed.
    pub(crate) fn restore_ai_panel_width(&self) {
        if !self.ai_panel_visible.get() {
            return;
        }
        let requested_width = self.config.borrow().ai_panel_width;
        self.ai_panel_width_restoring.set(true);
        apply_ai_panel_width(&self.ai_paned, requested_width);

        let paned = self.ai_paned.clone();
        let visible = self.ai_panel_visible.clone();
        let restoring = self.ai_panel_width_restoring.clone();
        glib::idle_add_local_once(move || {
            if visible.get() {
                apply_ai_panel_width(&paned, requested_width);
            }
            glib::idle_add_local_once(move || restoring.set(false));
        });
    }

    /// Copy the currently allocated AI width into Config. Callers decide when
    /// to flush Config so drag notifications can be debounced into one write.
    pub(crate) fn capture_ai_panel_width(&self) -> bool {
        if !self.ai_panel_visible.get() || self.ai_panel_width_restoring.get() {
            return false;
        }
        let total_width = self.ai_paned.width();
        let position = self.ai_paned.position();
        let Some(measured) = ai_panel_width_from_geometry(total_width, position) else {
            return false;
        };
        let mut config = self.config.borrow_mut();
        if config.ai_panel_width == measured {
            return false;
        }
        config.ai_panel_width = measured;
        true
    }

    /// Grab the selected block's context (cmd + output + cwd + exit) from
    /// the active TermView and hand it to the AI panel. Opens the panel
    /// first if it's hidden; no-ops cleanly when nothing's selected or the
    /// active tab is VTE-mode.
    pub(crate) fn ask_ai_about_selected_block(&self) {
        if !self.config.borrow().ai_enabled {
            self.show_ai_error("AI features are disabled in Settings or safe mode.");
            return;
        }
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

    pub(crate) fn show_ai_error(&self, message: &str) {
        let dialog = adw::AlertDialog::new(Some("AI unavailable"), Some(message));
        dialog.add_response("ok", "OK");
        dialog.set_default_response(Some("ok"));
        dialog.present(Some(&self.window));
    }

    pub(crate) fn focus_current_terminal(&self) {
        if let Some(page) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page)) {
                self.focus_terminal_in_page(&widget);
            }
        }
    }

    /// Focus the active leaf through the recursive typed pane tree.
    pub(crate) fn focus_terminal_in_page(&self, widget: &gtk4::Widget) {
        if let Some(node) = PaneNode::from_widget(widget) {
            node.grab_focus();
        }
    }

    /// Return the page's exact Block controller, when the active leaf uses
    /// block mode. This mirrors `terminal_in_page` so tab activation never has to
    /// discover the live surface by walking through read-only snapshot VTEs.
    pub(crate) fn term_view_in_page(&self, widget: &gtk4::Widget) -> Option<Rc<TermView>> {
        PaneNode::from_widget(widget)
            .and_then(|node| node.active_leaf())
            .and_then(|leaf| leaf.block_view())
    }

    /// Return the page's exact live input surface.
    ///
    /// Block pages contain read-only VTE snapshots in addition to the active
    /// input VTE, so callers must use the typed pane controller rather than
    /// walking the GTK widget tree.
    pub(crate) fn terminal_in_page(&self, widget: &gtk4::Widget) -> Option<Terminal> {
        PaneNode::from_widget(widget).and_then(|node| node.active_terminal())
    }

    pub(crate) fn current_terminal(&self) -> Option<Terminal> {
        self.notebook
            .current_page()
            .and_then(|page_num| self.notebook.nth_page(Some(page_num)))
            .and_then(|widget| self.terminal_in_page(&widget))
    }

    pub(crate) fn current_pane_leaf(&self) -> Option<PaneLeaf> {
        self.notebook
            .current_page()
            .and_then(|page_num| self.notebook.nth_page(Some(page_num)))
            .and_then(|widget| PaneNode::from_widget(&widget))
            .and_then(|node| node.active_leaf())
    }

    pub(crate) fn current_term_view(&self) -> Option<Rc<TermView>> {
        self.current_pane_leaf().and_then(|leaf| leaf.block_view())
    }
}

#[cfg(test)]
mod tests {
    use super::{ai_panel_width_from_geometry, restored_ai_panel_position};

    #[test]
    fn ai_panel_geometry_preserves_workspace_and_clamps_configured_limits() {
        assert_eq!(restored_ai_panel_position(800, 360), Some(440));
        assert_eq!(restored_ai_panel_position(800, 1200), Some(200));
        assert_eq!(restored_ai_panel_position(800, 100), Some(560));
        assert_eq!(restored_ai_panel_position(440, 360), None);

        assert_eq!(ai_panel_width_from_geometry(800, 440), Some(360));
        assert_eq!(ai_panel_width_from_geometry(2000, 100), Some(1200));
        assert_eq!(ai_panel_width_from_geometry(800, 800), None);
    }
}
