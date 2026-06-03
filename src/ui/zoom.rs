//! zoom — UiState methods extracted from ui (mechanical split, no logic changes)
use gtk4::Paned;
use libadwaita as adw;
use adw::prelude::*;

use crate::terminal::{
    scrollbar_wrapper_of,
    terminal_working_directory,
    find_first_terminal, find_focused_terminal, reattach_terminal_to_tree,
};
use super::*;

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
        let Some(page_num) = self.notebook.current_page() else { return };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else { return };

        // Only zoom if there are splits (page is a Paned)
        if page_widget.clone().downcast::<Paned>().is_err() { return; }

        let Some(term) = find_focused_terminal(&page_widget) else { return };
        // The effective widget (wrapper box or bare terminal) is what sits in the Paned.
        let eff_widget = scrollbar_wrapper_of(&term.clone().upcast::<gtk4::Widget>())
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| term.clone().upcast::<gtk4::Widget>());
        let Some(parent) = eff_widget.parent() else { return };
        let Ok(parent_paned) = parent.downcast::<Paned>() else { return };

        let tab_label = self.notebook.tab_label(&page_widget);

        // Detach terminal from its parent paned (leave None slot for reattach)
        if parent_paned.start_child().as_ref() == Some(&eff_widget) {
            parent_paned.set_start_child(None::<&gtk4::Widget>);
        } else {
            parent_paned.set_end_child(None::<&gtk4::Widget>);
        }

        let widget_name = page_widget.widget_name().to_string();
        self.notebook.remove_page(Some(page_num));

        // Add terminal (with scrollbar wrapper) as a standalone page
        eff_widget.set_widget_name(&widget_name);
        let new_page = self.notebook.insert_page(
            &eff_widget,
            tab_label.as_ref(),
            Some(page_num),
        );
        self.notebook.set_tab_reorderable(&eff_widget, true);
        self.notebook.set_current_page(Some(new_page));
        self.sync_tab_strip_active(Some(new_page));
        term.grab_focus();

        *self.zoom_state.borrow_mut() = Some(ZoomState {
            original_page: page_widget,
            zoomed_terminal: term,
            page_index: page_num,
            tab_label,
        });
    }

    pub(crate) fn unzoom_pane(&self, state: ZoomState) {
        let Some(page_num) = self.notebook.current_page() else { return };

        // Remove the zoomed terminal's standalone page
        self.notebook.remove_page(Some(page_num));

        // Re-attach the effective widget (wrapper box or terminal) to the Paned tree
        let eff_widget = scrollbar_wrapper_of(&state.zoomed_terminal.clone().upcast::<gtk4::Widget>())
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| state.zoomed_terminal.clone().upcast::<gtk4::Widget>());
        reattach_terminal_to_tree(&state.original_page, &eff_widget);

        // Re-add the original Paned tree as the page
        let widget_name = eff_widget.widget_name().to_string();
        state.original_page.set_widget_name(&widget_name);
        let new_page = self.notebook.insert_page(
            &state.original_page,
            state.tab_label.as_ref(),
            Some(state.page_index),
        );
        self.notebook.set_tab_reorderable(&state.original_page, true);
        self.notebook.set_current_page(Some(new_page));
        self.sync_tab_strip_active(Some(new_page));
        state.zoomed_terminal.grab_focus();
    }

    pub(crate) fn move_pane_to_new_tab(&self) {
        let Some(page_num) = self.notebook.current_page() else { return };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else { return };

        // Only works if there are splits
        if page_widget.clone().downcast::<Paned>().is_err() { return; }

        let Some(term) = find_focused_terminal(&page_widget) else { return };
        let eff_widget = scrollbar_wrapper_of(&term.clone().upcast::<gtk4::Widget>())
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| term.clone().upcast::<gtk4::Widget>());
        let Some(parent) = eff_widget.parent() else { return };
        let Ok(paned) = parent.clone().downcast::<Paned>() else { return };

        let start = paned.start_child();
        let end = paned.end_child();
        let sibling = if start.as_ref() == Some(&eff_widget) {
            end
        } else {
            start
        };

        // Detach both children
        paned.set_start_child(None::<&gtk4::Widget>);
        paned.set_end_child(None::<&gtk4::Widget>);

        // Promote sibling (same logic as handle_terminal_exited)
        if let Some(sibling) = sibling {
            let paned_widget = paned.upcast::<gtk4::Widget>();
            if let Some(grandparent) = paned_widget.parent() {
                if let Ok(gp_paned) = grandparent.clone().downcast::<Paned>() {
                    if gp_paned.start_child().as_ref() == Some(&paned_widget) {
                        gp_paned.set_start_child(Some(&sibling));
                    } else {
                        gp_paned.set_end_child(Some(&sibling));
                    }
                } else {
                    for i in 0..self.notebook.n_pages() {
                        if let Some(pw) = self.notebook.nth_page(Some(i)) {
                            if pw == paned_widget {
                                sibling.set_widget_name(&pw.widget_name());
                                let tab_label = self.notebook.tab_label(&pw);
                                self.notebook.remove_page(Some(i));
                                let new_page_num = self.notebook.insert_page(
                                    &sibling,
                                    tab_label.as_ref(),
                                    Some(i),
                                );
                                self.notebook.set_tab_reorderable(&sibling, true);
                                self.notebook.set_current_page(Some(new_page_num));
                                break;
                            }
                        }
                    }
                }
            }

            if let Some(sibling_term) = find_first_terminal(&sibling) {
                sibling_term.grab_focus();
            }
        }

        // Now the terminal is detached - add it as a new tab
        let working_directory = terminal_working_directory(&term);
        self.add_terminal_as_new_tab(term, working_directory);
    }
}
