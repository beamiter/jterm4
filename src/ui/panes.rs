//! panes — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::{Orientation, Paned};
use libadwaita as adw;
use std::rc::Rc;

use super::*;
use crate::keybindings::Direction;
use crate::state::generate_session_id;
use crate::terminal::{setup_terminal_click_handler, terminal_working_directory, VteTerminalView};

impl UiState {
    /// Create a managed conventional-VTE pane leaf.
    ///
    /// Runtime splits and restored split layouts share this constructor so every
    /// leaf root stores a `PaneLeaf` controller. This keeps process callbacks and
    /// GTK object ownership attached to the same widget that enters the pane tree.
    pub(crate) fn create_vte_leaf(
        &self,
        working_directory: Option<&str>,
        session_id: Option<&str>,
        initial_commands: Option<&str>,
        tab_widget_name: Option<String>,
    ) -> PaneLeaf {
        let sid = session_id
            .map(str::to_owned)
            .unwrap_or_else(generate_session_id);
        let shell_argv = self.shell_argv.borrow();
        let view = Rc::new(VteTerminalView::new(
            self.config.clone(),
            shell_argv.as_slice(),
            working_directory,
            Some(&sid),
            initial_commands,
        ));
        drop(shell_argv);

        let terminal = view.vte().clone();
        setup_terminal_click_handler(&terminal);
        self.setup_context_menu(&terminal);

        let ui_for_exit = UiState::clone(self);
        let terminal_for_exit = terminal.clone();
        view.connect_exited(move |_| {
            ui_for_exit.handle_terminal_exited(&terminal_for_exit.clone().upcast::<gtk4::Widget>());
        });

        if let Some(name) = tab_widget_name {
            let ui_for_bell = self.clone();
            let bell_name = name.clone();
            view.connect_bell(move || {
                log::debug!("Bell signal received (split)");
                ui_for_bell.mark_tab_bell(&bell_name);
            });

            let ui_for_activity = self.clone();
            view.connect_activity(move || {
                ui_for_activity.mark_tab_activity(&name);
            });
        }

        let leaf = PaneLeaf::Vte(view);
        let root = leaf.root_widget();
        leaf.attach_to(&root);
        leaf
    }

    pub(crate) fn split_current(&self, orientation: Orientation) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        let Some(page_node) = PaneNode::from_widget(&page_widget) else {
            return;
        };

        // A Block leaf owns a structured history surface around its live VTE.
        // Refuse before creating a second PTY until split construction itself
        // creates typed Block leaves. Existing VTE split trees remain supported.
        if page_node.contains_block() {
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

        let Some(current_leaf) = page_node.active_leaf() else {
            return;
        };
        let current_term = current_leaf.terminal().clone();
        let working_directory = terminal_working_directory(&current_term);
        let tab_widget_name = Some(page_widget.widget_name().to_string());
        let current_widget = current_leaf.root_widget();
        let parent = current_widget.parent();

        let new_leaf =
            self.create_vte_leaf(working_directory.as_deref(), None, None, tab_widget_name);
        let new_widget = new_leaf.root_widget();

        let paned = Paned::new(orientation);
        paned.set_hexpand(true);
        paned.set_vexpand(true);

        if let Some(ref parent) = parent {
            if let Ok(parent_paned) = parent.clone().downcast::<Paned>() {
                let is_start = parent_paned.start_child().as_ref() == Some(&current_widget);
                if is_start {
                    parent_paned.set_start_child(Some(&paned));
                } else {
                    parent_paned.set_end_child(Some(&paned));
                }
                paned.set_start_child(Some(&current_widget));
                paned.set_end_child(Some(&new_widget));
            } else {
                for index in 0..self.notebook.n_pages() {
                    if let Some(candidate) = self.notebook.nth_page(Some(index)) {
                        if candidate == current_widget {
                            paned.set_widget_name(&candidate.widget_name());
                            let tab_label = self.notebook.tab_label(&candidate);
                            self.notebook.remove_page(Some(index));
                            paned.set_start_child(Some(&current_widget));
                            paned.set_end_child(Some(&new_widget));
                            let inserted =
                                self.notebook
                                    .insert_page(&paned, tab_label.as_ref(), Some(index));
                            self.notebook.set_tab_reorderable(&paned, true);
                            self.notebook.set_current_page(Some(inserted));
                            break;
                        }
                    }
                }
            }
        }

        new_leaf.grab_focus();
    }

    pub(crate) fn cycle_pane_focus(&self, direction: i32) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        let Some(node) = PaneNode::from_widget(&widget) else {
            return;
        };
        let leaves = node.leaves();
        if leaves.len() <= 1 {
            return;
        }

        let focused = leaves
            .iter()
            .position(|leaf| leaf.terminal().has_focus())
            .unwrap_or(0);
        let next = if direction > 0 {
            (focused + 1) % leaves.len()
        } else if focused == 0 {
            leaves.len() - 1
        } else {
            focused - 1
        };
        leaves[next].grab_focus();
    }

    pub(crate) fn resize_pane(&self, target_orientation: Orientation, delta: i32) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        let Some(leaf) = PaneNode::from_widget(&page_widget).and_then(|node| node.active_leaf())
        else {
            return;
        };

        let mut widget = leaf.root_widget().parent();
        while let Some(current) = widget {
            if let Ok(paned) = current.clone().downcast::<Paned>() {
                if paned.orientation() == target_orientation {
                    paned.set_position((paned.position() + delta).max(0));
                    return;
                }
            }
            widget = current.parent();
        }
    }

    pub(crate) fn focus_pane_directional(&self, direction: Direction) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        let Some(node) = PaneNode::from_widget(&page_widget) else {
            return;
        };
        let leaves = node.leaves();
        if leaves.len() <= 1 {
            return;
        }
        let Some(focused) = node.focused_leaf() else {
            return;
        };
        let focused_root = focused.root_widget();
        let Some(bounds) = focused_root.compute_bounds(&page_widget) else {
            return;
        };
        let focused_cx = bounds.x() + bounds.width() / 2.0;
        let focused_cy = bounds.y() + bounds.height() / 2.0;

        let mut best: Option<(f32, PaneLeaf)> = None;
        for leaf in leaves {
            let root = leaf.root_widget();
            if root == focused_root {
                continue;
            }
            let Some(bounds) = root.compute_bounds(&page_widget) else {
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
            let distance = match direction {
                Direction::Left | Direction::Right => dx.abs() + dy.abs() * 0.1,
                Direction::Up | Direction::Down => dy.abs() + dx.abs() * 0.1,
            };
            if best.as_ref().is_none_or(|(current, _)| distance < *current) {
                best = Some((distance, leaf));
            }
        }

        if let Some((_, leaf)) = best {
            leaf.grab_focus();
        }
    }
}
