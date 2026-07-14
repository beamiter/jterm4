from pathlib import Path


def replace_function(text: str, signature: str, replacement: str) -> str:
    start = text.find(signature)
    if start < 0:
        raise SystemExit(f"function marker not found: {signature}")
    brace = text.find("{", start)
    if brace < 0:
        raise SystemExit(f"opening brace not found: {signature}")
    depth = 0
    for index in range(brace, len(text)):
        char = text[index]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return text[:start] + replacement.rstrip() + text[index + 1:]
    raise SystemExit(f"closing brace not found: {signature}")


Path("src/ui/pane_node.rs").write_text(r'''//! Typed structural view of a notebook pane tree.
//!
//! Every direct or split pane root carries a `PaneLeaf` controller. Reconstructing
//! the GTK `Paned` hierarchy into this recursive model gives navigation, focus,
//! close, move, and zoom operations one ownership-aware path without introducing
//! Relm4 or enabling Block splits.

use gtk4::prelude::*;
use gtk4::{Paned, Widget};
use vte4::Terminal;

use super::PaneLeaf;

#[derive(Clone)]
pub(crate) enum PaneNode {
    Leaf(PaneLeaf),
    Split {
        start: Box<PaneNode>,
        end: Box<PaneNode>,
    },
}

impl PaneNode {
    pub(crate) fn from_widget(widget: &Widget) -> Option<Self> {
        if let Ok(paned) = widget.clone().downcast::<Paned>() {
            let start = paned.start_child()?;
            let end = paned.end_child()?;
            return Some(Self::Split {
                start: Box::new(Self::from_widget(&start)?),
                end: Box::new(Self::from_widget(&end)?),
            });
        }

        PaneLeaf::from_widget(widget).map(Self::Leaf)
    }

    pub(crate) fn is_split(&self) -> bool {
        matches!(self, Self::Split { .. })
    }

    pub(crate) fn contains_block(&self) -> bool {
        match self {
            Self::Leaf(controller) => controller.is_block(),
            Self::Split { start, end } => start.contains_block() || end.contains_block(),
        }
    }

    pub(crate) fn leaves(&self) -> Vec<PaneLeaf> {
        let mut leaves = Vec::new();
        self.collect_leaves(&mut leaves);
        leaves
    }

    pub(crate) fn focused_leaf(&self) -> Option<PaneLeaf> {
        match self {
            Self::Leaf(controller) if controller.terminal().has_focus() => {
                Some(controller.clone())
            }
            Self::Leaf(_) => None,
            Self::Split { start, end } => start.focused_leaf().or_else(|| end.focused_leaf()),
        }
    }

    pub(crate) fn active_leaf(&self) -> Option<PaneLeaf> {
        self.focused_leaf().or_else(|| self.first_leaf())
    }

    pub(crate) fn active_terminal(&self) -> Option<Terminal> {
        self.active_leaf()
            .map(|controller| controller.terminal().clone())
    }

    pub(crate) fn grab_focus(&self) {
        if let Some(controller) = self.active_leaf() {
            controller.grab_focus();
        }
    }

    fn collect_leaves(&self, leaves: &mut Vec<PaneLeaf>) {
        match self {
            Self::Leaf(controller) => leaves.push(controller.clone()),
            Self::Split { start, end } => {
                start.collect_leaves(leaves);
                end.collect_leaves(leaves);
            }
        }
    }

    fn first_leaf(&self) -> Option<PaneLeaf> {
        match self {
            Self::Leaf(controller) => Some(controller.clone()),
            Self::Split { start, end } => start.first_leaf().or_else(|| end.first_leaf()),
        }
    }
}
''')

panes_path = Path("src/ui/panes.rs")
panes = panes_path.read_text()
panes = panes.replace("use vte4::Terminal;\n", "")
panes = panes.replace(
    "use crate::terminal::{\n    collect_terminals, scrollbar_wrapper_of, setup_terminal_click_handler,\n    terminal_working_directory, VteTerminalView,\n};",
    "use crate::terminal::{setup_terminal_click_handler, terminal_working_directory, VteTerminalView};",
)

panes = replace_function(
    panes,
    "    pub(crate) fn split_current(&self, orientation: Orientation)",
    r'''    pub(crate) fn split_current(&self, orientation: Orientation) {
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
                                self.notebook.insert_page(&paned, tab_label.as_ref(), Some(index));
                            self.notebook.set_tab_reorderable(&paned, true);
                            self.notebook.set_current_page(Some(inserted));
                            break;
                        }
                    }
                }
            }
        }

        new_leaf.grab_focus();
    }''',
)

panes = replace_function(
    panes,
    "    pub(crate) fn cycle_pane_focus(&self, direction: i32)",
    r'''    pub(crate) fn cycle_pane_focus(&self, direction: i32) {
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
    }''',
)

panes = replace_function(
    panes,
    "    pub(crate) fn resize_pane(&self, target_orientation: Orientation, delta: i32)",
    r'''    pub(crate) fn resize_pane(&self, target_orientation: Orientation, delta: i32) {
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
    }''',
)

panes = replace_function(
    panes,
    "    pub(crate) fn focus_pane_directional(&self, direction: Direction)",
    r'''    pub(crate) fn focus_pane_directional(&self, direction: Direction) {
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
    }''',
)
panes_path.write_text(panes)

zoom_path = Path("src/ui/zoom.rs")
zoom = zoom_path.read_text()
zoom = zoom.replace(
    "use crate::terminal::{\n    find_first_terminal, find_focused_terminal, reattach_terminal_to_tree, scrollbar_wrapper_of,\n    terminal_working_directory,\n};",
    "use crate::terminal::{reattach_terminal_to_tree, terminal_working_directory};",
)
zoom = replace_function(
    zoom,
    "    pub(crate) fn zoom_pane(&self)",
    r'''    pub(crate) fn zoom_pane(&self) {
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
    }''',
)
zoom = replace_function(
    zoom,
    "    pub(crate) fn unzoom_pane(&self, state: ZoomState)",
    r'''    pub(crate) fn unzoom_pane(&self, state: ZoomState) {
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
    }''',
)
zoom = replace_function(
    zoom,
    "    pub(crate) fn move_pane_to_new_tab(&self)",
    r'''    pub(crate) fn move_pane_to_new_tab(&self) {
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
        let Ok(paned) = parent.clone().downcast::<Paned>() else {
            return;
        };

        let start = paned.start_child();
        let end = paned.end_child();
        let sibling = if start.as_ref() == Some(&effective_widget) {
            end
        } else {
            start
        };
        paned.set_start_child(None::<&gtk4::Widget>);
        paned.set_end_child(None::<&gtk4::Widget>);

        if let Some(sibling) = sibling {
            let paned_widget = paned.upcast::<gtk4::Widget>();
            if let Some(grandparent) = paned_widget.parent() {
                if let Ok(grandparent_paned) = grandparent.clone().downcast::<Paned>() {
                    if grandparent_paned.start_child().as_ref() == Some(&paned_widget) {
                        grandparent_paned.set_start_child(Some(&sibling));
                    } else {
                        grandparent_paned.set_end_child(Some(&sibling));
                    }
                } else {
                    for index in 0..self.notebook.n_pages() {
                        if let Some(candidate) = self.notebook.nth_page(Some(index)) {
                            if candidate == paned_widget {
                                sibling.set_widget_name(&candidate.widget_name());
                                let tab_label = self.notebook.tab_label(&candidate);
                                self.notebook.remove_page(Some(index));
                                let inserted = self.notebook.insert_page(
                                    &sibling,
                                    tab_label.as_ref(),
                                    Some(index),
                                );
                                self.notebook.set_tab_reorderable(&sibling, true);
                                self.notebook.set_current_page(Some(inserted));
                                break;
                            }
                        }
                    }
                }
            }
            if let Some(sibling_node) = PaneNode::from_widget(&sibling) {
                sibling_node.grab_focus();
            }
        }

        let working_directory = terminal_working_directory(&terminal);
        self.add_terminal_as_new_tab(terminal, working_directory);
    }''',
)
zoom_path.write_text(zoom)

tabs_path = Path("src/ui/tabs.rs")
tabs = tabs_path.read_text()
tabs = replace_function(
    tabs,
    "    pub(crate) fn close_focused_pane_or_tab(&self)",
    r'''    pub(crate) fn close_focused_pane_or_tab(&self) {
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        if let Some(node) = PaneNode::from_widget(&page_widget) {
            if node.is_split() {
                if let Some(leaf) = node.active_leaf() {
                    let terminal = leaf.terminal().clone();
                    kill_terminal_child(&terminal);
                    self.handle_terminal_exited(&terminal.upcast::<gtk4::Widget>());
                    return;
                }
            }
        }
        self.remove_current_tab();
    }''',
)
tabs = tabs.replace(
    "                if let Some(term) = find_first_terminal(&sibling) {\n                    term.grab_focus();\n                }",
    "                if let Some(node) = PaneNode::from_widget(&sibling) {\n                    node.grab_focus();\n                }",
)
tabs_path.write_text(tabs)
