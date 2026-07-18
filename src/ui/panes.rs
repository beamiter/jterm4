//! panes — UiState methods extracted from ui (mechanical split, no logic changes)
use gtk4::prelude::*;
use gtk4::{Orientation, Paned};
use std::rc::Rc;

use super::*;
use crate::keybindings::Direction;
use crate::state::generate_session_id;
use crate::terminal::{setup_terminal_click_handler, terminal_working_directory, VteTerminalView};

/// Number of equal-size pane slots a subtree occupies along one axis.
///
/// A split on the other axis stacks its children instead of consuming more
/// space on this axis, so only the widest/tallest child determines its span.
/// This lets a mixed 2x2 tree balance like a grid while repeated same-axis
/// splits receive one equal slot per leaf instead of 1/2, 1/4, 1/8… widths.
fn pane_axis_span(widget: &gtk4::Widget, axis: Orientation) -> u32 {
    let Ok(paned) = widget.clone().downcast::<Paned>() else {
        return 1;
    };
    let Some(start) = paned.start_child() else {
        return 1;
    };
    let Some(end) = paned.end_child() else {
        return 1;
    };
    let start_span = pane_axis_span(&start, axis);
    let end_span = pane_axis_span(&end, axis);
    if paned.orientation() == axis {
        start_span.saturating_add(end_span)
    } else {
        start_span.max(end_span)
    }
}

fn balanced_split_position(extent: i32, start_span: u32, end_span: u32) -> Option<i32> {
    if extent <= 1 || start_span == 0 || end_span == 0 {
        return None;
    }
    let total_span = u64::from(start_span) + u64::from(end_span);
    let position = i64::from(extent) * i64::from(start_span) / total_span as i64;
    Some(position.clamp(1, i64::from(extent - 1)) as i32)
}

fn nearest_directional_index(
    centers: &[(f32, f32)],
    focused: usize,
    direction: Direction,
) -> Option<usize> {
    let (focused_x, focused_y) = *centers.get(focused)?;
    centers
        .iter()
        .enumerate()
        .filter_map(|(index, &(x, y))| {
            if index == focused {
                return None;
            }
            let dx = x - focused_x;
            let dy = y - focused_y;
            let in_direction = match direction {
                Direction::Left => dx < -1.0,
                Direction::Right => dx > 1.0,
                Direction::Up => dy < -1.0,
                Direction::Down => dy > 1.0,
            };
            if !in_direction {
                return None;
            }
            let distance = match direction {
                Direction::Left | Direction::Right => dx.abs() + dy.abs() * 0.1,
                Direction::Up | Direction::Down => dy.abs() + dx.abs() * 0.1,
            };
            Some((index, distance))
        })
        .min_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(index, _)| index)
}

/// Rebalance every split according to the number of pane slots below it.
///
/// GTK Paned defaults each newly nested split to 50/50. Repeatedly splitting
/// the newest pane therefore leaves the first pane at half the window and
/// squeezes every later sibling into the remaining half. Recomputing the
/// proportions from subtree spans gives three same-axis panes 1/3 each, four
/// panes 1/4 each, and sensible dimensions for mixed-axis grids.
fn rebalance_pane_tree(widget: &gtk4::Widget) {
    let Ok(paned) = widget.clone().downcast::<Paned>() else {
        return;
    };
    let Some(start) = paned.start_child() else {
        return;
    };
    let Some(end) = paned.end_child() else {
        return;
    };
    let axis = paned.orientation();
    let extent = if axis == Orientation::Horizontal {
        paned.width()
    } else {
        paned.height()
    };
    let start_span = pane_axis_span(&start, axis);
    let end_span = pane_axis_span(&end, axis);
    if let Some(position) = balanced_split_position(extent, start_span, end_span) {
        paned.set_position(position);
    }
    rebalance_pane_tree(&start);
    rebalance_pane_tree(&end);
}

fn schedule_pane_rebalance(page: gtk4::Widget) {
    // The first idle runs after the new Paned enters the widget tree. A second
    // pass catches nested panes whose allocation changes because an ancestor
    // divider moved during the first pass.
    gtk4::glib::idle_add_local_once(move || {
        rebalance_pane_tree(&page);
        let page = page.clone();
        gtk4::glib::idle_add_local_once(move || {
            rebalance_pane_tree(&page);
        });
    });
}

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

        let leaf = PaneLeaf::Vte(view);
        let root = leaf.root_widget();
        leaf.attach_to(&root);
        leaf.set_session_id(&sid);
        leaf.set_remote(false);
        if tab_widget_name.is_some() {
            let ui_for_bell = self.clone();
            let leaf_for_bell = leaf.clone();
            if let PaneLeaf::Vte(view) = &leaf {
                view.connect_bell(move || {
                    log::debug!("Bell signal received (split)");
                    ui_for_bell.mark_tab_bell(&leaf_for_bell.root_widget().widget_name());
                });
            }

            let ui_for_activity = self.clone();
            let leaf_for_activity = leaf.clone();
            if let PaneLeaf::Vte(view) = &leaf {
                view.connect_activity(move || {
                    ui_for_activity
                        .mark_tab_activity(&leaf_for_activity.root_widget().widget_name());
                });
            }
        }
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
        paned.set_resize_start_child(true);
        paned.set_resize_end_child(true);
        paned.set_shrink_start_child(true);
        paned.set_shrink_end_child(true);

        let current_extent = if orientation == Orientation::Horizontal {
            current_widget.width()
        } else {
            current_widget.height()
        };
        if let Some(position) = balanced_split_position(current_extent, 1, 1) {
            paned.set_position(position);
        }

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

        if let Some(page) = self
            .notebook
            .current_page()
            .and_then(|page| self.notebook.nth_page(Some(page)))
        {
            schedule_pane_rebalance(page);
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
        // Focus can temporarily live on a finished Block VTE, a scrollbar, or
        // another descendant rather than the leaf's live input VTE. active_leaf
        // resolves the full focus subtree and falls back to the last active pane
        // instead of silently dropping the shortcut.
        let Some(focused) = node.active_leaf() else {
            return;
        };
        let focused_root = focused.root_widget();

        let mut positioned = Vec::with_capacity(leaves.len());
        for leaf in leaves {
            let root = leaf.root_widget();
            let Some(bounds) = root.compute_bounds(&page_widget) else {
                continue;
            };
            let cx = bounds.x() + bounds.width() / 2.0;
            let cy = bounds.y() + bounds.height() / 2.0;
            positioned.push((leaf, (cx, cy)));
        }

        let Some(focused_index) = positioned
            .iter()
            .position(|(leaf, _)| leaf.root_widget() == focused_root)
        else {
            return;
        };
        let centers = positioned
            .iter()
            .map(|(_, center)| *center)
            .collect::<Vec<_>>();
        let Some(target) = nearest_directional_index(&centers, focused_index, direction) else {
            return;
        };
        positioned[target].0.grab_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::{balanced_split_position, nearest_directional_index};
    use crate::keybindings::Direction;

    #[test]
    fn balanced_position_allocates_equal_same_axis_slots() {
        assert_eq!(balanced_split_position(1_200, 1, 1), Some(600));
        assert_eq!(balanced_split_position(1_200, 1, 2), Some(400));
        assert_eq!(balanced_split_position(1_200, 2, 1), Some(800));
        assert_eq!(balanced_split_position(1_200, 3, 1), Some(900));
    }

    #[test]
    fn balanced_position_rejects_unallocated_or_empty_splits() {
        assert_eq!(balanced_split_position(0, 1, 1), None);
        assert_eq!(balanced_split_position(100, 0, 1), None);
        assert_eq!(balanced_split_position(100, 1, 0), None);
    }

    #[test]
    fn directional_focus_selects_the_nearest_pane_on_each_axis() {
        // 2x2 pane grid in visual order.
        let centers = [(25.0, 25.0), (75.0, 25.0), (25.0, 75.0), (75.0, 75.0)];

        assert_eq!(
            nearest_directional_index(&centers, 3, Direction::Left),
            Some(2)
        );
        assert_eq!(
            nearest_directional_index(&centers, 2, Direction::Right),
            Some(3)
        );
        assert_eq!(
            nearest_directional_index(&centers, 3, Direction::Up),
            Some(1)
        );
        assert_eq!(
            nearest_directional_index(&centers, 1, Direction::Down),
            Some(3)
        );
    }

    #[test]
    fn directional_focus_does_not_wrap_at_an_outer_edge() {
        let centers = [(25.0, 25.0), (75.0, 25.0)];

        assert_eq!(
            nearest_directional_index(&centers, 0, Direction::Left),
            None
        );
        assert_eq!(
            nearest_directional_index(&centers, 1, Direction::Right),
            None
        );
        assert_eq!(nearest_directional_index(&centers, 0, Direction::Up), None);
        assert_eq!(
            nearest_directional_index(&centers, 0, Direction::Down),
            None
        );
    }
}
