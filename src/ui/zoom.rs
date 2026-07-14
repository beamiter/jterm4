//! zoom — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::Paned;
use libadwaita as adw;

use super::*;
use crate::terminal::{reattach_terminal_to_tree, terminal_working_directory};

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
        let effective_widget = leaf.root_widget();
        let Some(parent) = effective_widget.parent() else {
            return;
        };
        let Ok(parent_paned) = parent.downcast::<Paned>() else {
            return;
        };
        let tab_label = self.notebook.tab_label(&page_widget);

        if parent_paned.start_child().as_ref() == Some(&effective_widget) {
            parent_paned.set_start_child(None::<&gtk4::Widget>);
        } else {
            parent_paned.set_end_child(None::<&gtk4::Widget>);
        }

        let widget_name = page_widget.widget_name().to_string();
        self.notebook.remove_page(Some(page_num));
        effective_widget.set_widget_name(&widget_name);
        let inserted =
            self.notebook
                .insert_page(&effective_widget, tab_label.as_ref(), Some(page_num));
        self.notebook.set_tab_reorderable(&effective_widget, true);
        self.notebook.set_current_page(Some(inserted));
        self.sync_tab_strip_active(Some(inserted));
        leaf.grab_focus();

        *self.zoom_state.borrow_mut() = Some(ZoomState {
            original_page: page_widget,
            zoomed_terminal: terminal,
            page_index: page_num,
            tab_label,
        });
    }

    pub(crate) fn unzoom_pane(&self, state: ZoomState) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(effective_widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        let widget_name = effective_widget.widget_name().to_string();
        self.notebook.remove_page(Some(page_num));

        reattach_terminal_to_tree(&state.original_page, &effective_widget);
        state.original_page.set_widget_name(&widget_name);
        let inserted = self.notebook.insert_page(
            &state.original_page,
            state.tab_label.as_ref(),
            Some(state.page_index),
        );
        self.notebook
            .set_tab_reorderable(&state.original_page, true);
        self.notebook.set_current_page(Some(inserted));
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
