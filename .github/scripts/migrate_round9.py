from pathlib import Path

ui = Path("src/ui")

mod_path = ui / "mod.rs"
mod_text = mod_path.read_text()
if "mod pane_tree_edit;\n" not in mod_text:
    mod_text = mod_text.replace("mod pane_node;\n", "mod pane_node;\nmod pane_tree_edit;\n", 1)
if "pub(crate) use pane_tree_edit::detach_leaf_and_promote;\n" not in mod_text:
    mod_text = mod_text.replace(
        "pub(crate) use pane_node::PaneNode;\n",
        "pub(crate) use pane_node::PaneNode;\npub(crate) use pane_tree_edit::detach_leaf_and_promote;\n",
        1,
    )
mod_path.write_text(mod_text)

editor = '''//! Structural edits for the native GTK pane tree.
//!
//! Closing and moving a split leaf both perform the same mutation: detach the
//! target leaf, remove its parent `Paned`, and promote the sibling into either the
//! ancestor `Paned` or the original Notebook page. Keeping that mutation here
//! prevents lifecycle paths from implementing subtly different widget surgery.

use gtk4::prelude::*;
use gtk4::{Notebook, Paned, Widget};

/// Detach `leaf_root` from its parent split and promote its sibling.
///
/// Returns the promoted sibling. A direct Notebook leaf has no split to collapse
/// and returns `None`; callers can then apply their normal whole-tab behavior.
pub(crate) fn detach_leaf_and_promote(
    notebook: &Notebook,
    leaf_root: &Widget,
) -> Option<Widget> {
    let parent = leaf_root.parent()?.downcast::<Paned>().ok()?;
    let start = parent.start_child();
    let end = parent.end_child();
    let sibling = if start.as_ref() == Some(leaf_root) {
        end?
    } else if end.as_ref() == Some(leaf_root) {
        start?
    } else {
        return None;
    };

    parent.set_start_child(None::<&Widget>);
    parent.set_end_child(None::<&Widget>);

    let parent_widget = parent.upcast::<Widget>();
    if let Some(grandparent) = parent_widget.parent() {
        if let Ok(grandparent) = grandparent.downcast::<Paned>() {
            if grandparent.start_child().as_ref() == Some(&parent_widget) {
                grandparent.set_start_child(Some(&sibling));
            } else if grandparent.end_child().as_ref() == Some(&parent_widget) {
                grandparent.set_end_child(Some(&sibling));
            } else {
                return None;
            }
            return Some(sibling);
        }
    }

    let page_index = notebook.page_num(&parent_widget)?;
    let page_name = parent_widget.widget_name().to_string();
    let tab_label = notebook.tab_label(&parent_widget);
    notebook.remove_page(Some(page_index));
    sibling.set_widget_name(&page_name);
    let inserted = notebook.insert_page(&sibling, tab_label.as_ref(), Some(page_index));
    notebook.set_tab_reorderable(&sibling, true);
    notebook.set_current_page(Some(inserted));
    Some(sibling)
}
'''
(ui / "pane_tree_edit.rs").write_text(editor)


def replace_between(text: str, start_marker: str, end_marker: str, replacement: str) -> str:
    start = text.find(start_marker)
    end = text.find(end_marker, start)
    if start < 0 or end < 0:
        raise SystemExit(f"markers not found: {start_marker!r} -> {end_marker!r}")
    return text[:start] + replacement + text[end:]

tabs_path = ui / "tabs.rs"
tabs = tabs_path.read_text()
tabs = tabs.replace("use gtk4::{glib, Label, Paned};", "use gtk4::{glib, Label};", 1)
handle = '''    /// Handle a terminal exiting: collapse its split or close the whole tab.
    pub(crate) fn handle_terminal_exited(&self, term_widget: &gtk4::Widget) {
        {
            let zoom = self.zoom_state.borrow();
            if let Some(ref state) = *zoom {
                if state.zoomed_terminal.upcast_ref::<gtk4::Widget>() == term_widget {
                    drop(zoom);
                    self.zoom_state.borrow_mut().take();
                }
            }
        }

        let leaf_root = scrollbar_wrapper_of(term_widget)
            .map(|wrapper| wrapper.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| term_widget.clone());

        if let Some(sibling) = detach_leaf_and_promote(&self.notebook, &leaf_root) {
            if let Some(node) = PaneNode::from_widget(&sibling) {
                node.grab_focus();
            }
        } else {
            self.remove_tab_by_widget(&leaf_root);
        }
    }

'''
tabs = replace_between(
    tabs,
    "    /// Handle a terminal exiting: unsplit if in a Paned, or close the tab.\n",
    "    pub(crate) fn remove_current_tab",
    handle,
)
add_leaf = '''    /// Add an existing typed pane leaf as a new tab.
    pub(crate) fn add_pane_leaf_as_new_tab(
        &self,
        leaf: PaneLeaf,
        working_directory: Option<String>,
    ) {
        let tab_num = self.tab_counter.get();
        self.tab_counter.set(tab_num + 1);

        let sid = generate_session_id();
        self.session_ids.borrow_mut().insert(tab_num, sid);
        let tab_name = default_tab_title(tab_num, working_directory.as_deref());

        let page_widget = leaf.root_widget();
        page_widget.set_widget_name(&format!("tab-{tab_num}"));
        let label = Label::new(Some(&tab_name));
        let page_num = self.notebook.append_page(&page_widget, Some(&label));
        self.notebook.set_tab_reorderable(&page_widget, true);

        let button = ToggleButton::builder()
            .label(&tab_name)
            .css_classes(["flat", "tab-strip-btn"])
            .build();
        button.set_focus_on_click(false);
        button.set_can_focus(false);
        button.set_widget_name(&format!("tab-{tab_num}"));

        let ui_for_button = self.clone();
        button.connect_clicked(move |button| {
            let target_name = button.widget_name();
            for index in 0..ui_for_button.notebook.n_pages() {
                if let Some(candidate) = ui_for_button.notebook.nth_page(Some(index)) {
                    if candidate.widget_name() == target_name {
                        ui_for_button.notebook.set_current_page(Some(index));
                        break;
                    }
                }
            }
        });

        self.tab_strip.append(&button);
        self.notebook.set_current_page(Some(page_num));
        self.sync_tab_strip_active(Some(page_num));
        self.sync_tab_bar_visibility();
        leaf.grab_focus();
    }

'''
tabs = replace_between(
    tabs,
    "    /// Add an existing terminal widget as a new tab (used by move_pane_to_new_tab).\n",
    "    pub(crate) fn add_new_tab",
    add_leaf,
)
tabs_path.write_text(tabs)

zoom_path = ui / "zoom.rs"
zoom = zoom_path.read_text()
move_start = zoom.find("    pub(crate) fn move_pane_to_new_tab(&self) {")
move_end = zoom.rfind("\n}")
if move_start < 0 or move_end < 0:
    raise SystemExit("move-pane function markers not found")
move_fn = '''    pub(crate) fn move_pane_to_new_tab(&self) {
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
'''
zoom_path.write_text(zoom[:move_start] + move_fn + zoom[move_end:])
