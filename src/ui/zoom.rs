//! zoom — UiState methods extracted from ui (mechanical split, no logic changes)
use gtk4::prelude::WidgetExt;

use super::*;
use crate::terminal::terminal_working_directory;

impl UiState {
    pub(crate) fn toggle_pane_zoom(&self) {
        let has_zoom = self.zoom_state.borrow().is_some();
        if has_zoom {
            let state = self.zoom_state.borrow_mut().take().unwrap();
            self.unzoom_pane(state);
        } else {
            self.zoom_pane();
        }
    }

    pub(crate) fn zoom_pane(&self) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        let Some(node) = PaneNode::from_widget(&page_widget) else {
            return;
        };
        if !node.is_split() {
            return;
        }
        let Some(leaf) = node.active_leaf() else {
            return;
        };

        let terminal = leaf.terminal().clone();
        let leaf_root = leaf.root_widget();
        let Some(swap) = detach_leaf_for_zoom(&self.notebook, &page_widget, &leaf_root) else {
            return;
        };
        self.sync_tab_strip_active(Some(swap.page_index));
        leaf.grab_focus();

        *self.zoom_state.borrow_mut() = Some(ZoomState {
            swap,
            zoomed_terminal: terminal,
        });
    }

    pub(crate) fn unzoom_pane(&self, state: ZoomState) {
        let Some(inserted) = restore_zoomed_leaf(&self.notebook, &state.swap) else {
            return;
        };
        self.sync_tab_strip_active(Some(inserted));
        state.zoomed_terminal.grab_focus();
    }

    pub(crate) fn move_pane_to_new_tab(&self) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        let Some(node) = PaneNode::from_widget(&page_widget) else {
            return;
        };
        if !node.is_split() {
            return;
        }
        let Some(leaf) = node.active_leaf() else {
            return;
        };
        let working_directory = terminal_working_directory(leaf.terminal());
        let leaf_root = leaf.root_widget();
        let Some(sibling) = detach_leaf_and_promote(&self.notebook, &leaf_root) else {
            return;
        };
        if let Some(node) = PaneNode::from_widget(&sibling) {
            node.grab_focus();
        }
        self.add_pane_leaf_as_new_tab(leaf, working_directory);
    }
}
