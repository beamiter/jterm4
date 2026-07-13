//! panes — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::{Orientation, Paned};
use libadwaita as adw;
use vte4::Terminal;
use vte4::TerminalExt;

use super::*;
use crate::keybindings::Direction;
use crate::state::generate_session_id;
use crate::terminal::{
    collect_terminals, create_terminal, scrollbar_wrapper_of, setup_terminal_click_handler,
    spawn_shell, terminal_working_directory, wrap_with_scrollbar,
};

impl UiState {
    pub(crate) fn create_split_terminal(
        &self,
        working_directory: Option<&str>,
        tab_widget_name: Option<String>,
    ) -> Terminal {
        let terminal = create_terminal(&self.config.borrow());
        setup_terminal_click_handler(&terminal);
        self.setup_context_menu(&terminal);

        let ui_for_exit = UiState::clone(self);
        let terminal_clone = terminal.clone();
        terminal.connect_child_exited(move |_, _| {
            ui_for_exit.handle_terminal_exited(&terminal_clone.clone().upcast::<gtk4::Widget>());
        });

        // Bell and activity signals for split pane terminals
        if let Some(ref name) = tab_widget_name {
            let ui_for_bell = self.clone();
            let bell_name = name.clone();
            terminal.connect_bell(move |_| {
                log::debug!("Bell signal received (split)");
                ui_for_bell.mark_tab_bell(&bell_name);
            });

            let ui_for_activity = self.clone();
            let activity_name = name.clone();
            terminal.connect_commit(move |_, _, _| {
                ui_for_activity.mark_tab_activity(&activity_name);
            });
        }

        // Split panes get a fresh session ID (new shell instance)
        let sid = generate_session_id();
        let shell_argv = self.shell_argv.borrow();
        spawn_shell(
            &terminal,
            shell_argv.as_slice(),
            working_directory,
            Some(&sid),
            None,
        );
        terminal
    }

    pub(crate) fn split_current(&self, orientation: Orientation) {
        // A TermView owns a whole block list around its live VTE. Treating that
        // inner VTE as a pane leaf starts an orphan shell because the widget
        // cannot be replaced in the Notebook/Paned tree. Fail visibly and
        // safely until block panes use the dedicated PaneLeaf abstraction.
        if self.current_term_view().is_some() {
            let dialog = adw::AlertDialog::new(
                Some("Split panes require VTE mode"),
                Some(
                    "Block mode keeps command history in a structured view and cannot yet be split safely. Change terminal_mode to \"vte\" for split panes.",
                ),
            );
            dialog.add_response("ok", "OK");
            dialog.set_default_response(Some("ok"));
            dialog.present(Some(&self.window));
            log::warn!("Blocked an unsupported block-mode split before spawning a PTY");
            return;
        }
        let Some(current_term) = self.current_terminal() else {
            return;
        };
        let working_directory = terminal_working_directory(&current_term);

        // Find the tab widget name for bell/activity signals
        let tab_widget_name = self
            .notebook
            .current_page()
            .and_then(|p| self.notebook.nth_page(Some(p)))
            .map(|w| w.widget_name().to_string());

        // The effective widget in the Paned/notebook tree is the scrollbar wrapper
        // (if present) rather than the bare terminal.
        let current_widget = scrollbar_wrapper_of(&current_term.clone().upcast::<gtk4::Widget>())
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| current_term.clone().upcast::<gtk4::Widget>());
        let parent = current_widget.parent();

        let new_term = self.create_split_terminal(working_directory.as_deref(), tab_widget_name);
        let new_widget = wrap_with_scrollbar(&new_term);

        let paned = Paned::new(orientation);
        paned.set_hexpand(true);
        paned.set_vexpand(true);

        if let Some(ref parent) = parent {
            if let Ok(parent_paned) = parent.clone().downcast::<Paned>() {
                // Current terminal is in a Paned - replace it with a new nested Paned
                let is_start = parent_paned.start_child().as_ref() == Some(&current_widget);
                if is_start {
                    parent_paned.set_start_child(Some(&paned));
                } else {
                    parent_paned.set_end_child(Some(&paned));
                }
                paned.set_start_child(Some(&current_widget));
                paned.set_end_child(Some(&new_widget));
            } else {
                // Parent is the notebook - replace the page
                for i in 0..self.notebook.n_pages() {
                    if let Some(page_widget) = self.notebook.nth_page(Some(i)) {
                        if page_widget == current_widget {
                            // Transfer widget name so strip button mapping is preserved
                            paned.set_widget_name(&page_widget.widget_name());
                            let tab_label = self.notebook.tab_label(&page_widget);
                            self.notebook.remove_page(Some(i));
                            paned.set_start_child(Some(&current_widget));
                            paned.set_end_child(Some(&new_widget));
                            let new_page_num =
                                self.notebook
                                    .insert_page(&paned, tab_label.as_ref(), Some(i));
                            self.notebook.set_tab_reorderable(&paned, true);
                            self.notebook.set_current_page(Some(new_page_num));
                            break;
                        }
                    }
                }
            }
        }

        new_term.grab_focus();
    }

    pub(crate) fn cycle_pane_focus(&self, direction: i32) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        let mut terms = Vec::new();
        collect_terminals(&widget, &mut terms);
        if terms.len() <= 1 {
            return;
        }

        let focused_idx = terms.iter().position(|t| t.has_focus()).unwrap_or(0);
        let next_idx = if direction > 0 {
            (focused_idx + 1) % terms.len()
        } else {
            if focused_idx == 0 {
                terms.len() - 1
            } else {
                focused_idx - 1
            }
        };
        terms[next_idx].grab_focus();
    }

    pub(crate) fn resize_pane(&self, target_orientation: Orientation, delta: i32) {
        let Some(term) = self.current_terminal() else {
            return;
        };
        let term_widget = term.upcast::<gtk4::Widget>();
        // Walk up from the terminal to find the nearest Paned with matching orientation
        let mut widget = term_widget.parent();
        while let Some(w) = widget {
            if let Ok(paned) = w.clone().downcast::<Paned>() {
                if paned.orientation() == target_orientation {
                    let new_pos = (paned.position() + delta).max(0);
                    paned.set_position(new_pos);
                    return;
                }
            }
            widget = w.parent();
        }
    }

    pub(crate) fn focus_pane_directional(&self, direction: Direction) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        let mut terms = Vec::new();
        collect_terminals(&page_widget, &mut terms);
        if terms.len() <= 1 {
            return;
        }

        let focused = terms.iter().find(|t| t.has_focus());
        let Some(focused) = focused else { return };

        let focused_widget = focused.clone().upcast::<gtk4::Widget>();
        let Some(focused_bounds) = focused_widget.compute_bounds(&page_widget) else {
            return;
        };
        let focused_cx = focused_bounds.x() + focused_bounds.width() / 2.0;
        let focused_cy = focused_bounds.y() + focused_bounds.height() / 2.0;

        let mut best: Option<(f32, &Terminal)> = None;

        for term in &terms {
            if term.has_focus() {
                continue;
            }

            let tw = term.clone().upcast::<gtk4::Widget>();
            let Some(bounds) = tw.compute_bounds(&page_widget) else {
                continue;
            };
            let cx = bounds.x() + bounds.width() / 2.0;
            let cy = bounds.y() + bounds.height() / 2.0;

            let dx = cx - focused_cx;
            let dy = cy - focused_cy;

            let in_direction = match direction {
                Direction::Left => dx < -1.0,
                Direction::Right => dx > 1.0,
                Direction::Up => dy < -1.0,
                Direction::Down => dy > 1.0,
            };

            if !in_direction {
                continue;
            }

            let dist = match direction {
                Direction::Left | Direction::Right => dx.abs() + dy.abs() * 0.1,
                Direction::Up | Direction::Down => dy.abs() + dx.abs() * 0.1,
            };

            if best.is_none() || dist < best.unwrap().0 {
                best = Some((dist, term));
            }
        }

        if let Some((_, term)) = best {
            term.grab_focus();
        }
    }
}
