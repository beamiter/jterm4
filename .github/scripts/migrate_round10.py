from pathlib import Path
import re

editor_path = Path("src/ui/pane_tree_edit.rs")
editor = editor_path.read_text()
if "use crate::terminal::reattach_terminal_to_tree;" not in editor:
    editor = editor.replace(
        "use gtk4::{Notebook, Paned, Widget};\n",
        "use gtk4::{Notebook, Paned, Widget};\n\nuse crate::terminal::reattach_terminal_to_tree;\n",
        1,
    )

editor += r'''

/// Notebook-page swap retained while one split leaf is zoomed.
pub(crate) struct ZoomPageSwap {
    pub(crate) original_page: Widget,
    pub(crate) zoomed_page: Widget,
    pub(crate) page_index: u32,
    pub(crate) tab_label: Option<Widget>,
}

/// Detach one leaf from its split tree and expose it as the Notebook page.
pub(crate) fn detach_leaf_for_zoom(
    notebook: &Notebook,
    page_widget: &Widget,
    leaf_root: &Widget,
) -> Option<ZoomPageSwap> {
    let parent = leaf_root.parent()?.downcast::<Paned>().ok()?;
    if parent.start_child().as_ref() == Some(leaf_root) {
        parent.set_start_child(None::<&Widget>);
    } else if parent.end_child().as_ref() == Some(leaf_root) {
        parent.set_end_child(None::<&Widget>);
    } else {
        return None;
    }

    let page_index = notebook.page_num(page_widget)?;
    let page_name = page_widget.widget_name().to_string();
    let tab_label = notebook.tab_label(page_widget);
    notebook.remove_page(Some(page_index));

    leaf_root.set_widget_name(&page_name);
    let inserted = notebook.insert_page(leaf_root, tab_label.as_ref(), Some(page_index));
    notebook.set_tab_reorderable(leaf_root, true);
    notebook.set_current_page(Some(inserted));

    Some(ZoomPageSwap {
        original_page: page_widget.clone(),
        zoomed_page: leaf_root.clone(),
        page_index,
        tab_label,
    })
}

/// Restore a zoomed leaf to its empty split slot and reinstate the original page.
pub(crate) fn restore_zoomed_leaf(
    notebook: &Notebook,
    swap: &ZoomPageSwap,
) -> Option<u32> {
    let current_page = notebook.page_num(&swap.zoomed_page)?;
    let page_name = swap.zoomed_page.widget_name().to_string();
    notebook.remove_page(Some(current_page));

    reattach_terminal_to_tree(&swap.original_page, &swap.zoomed_page);
    swap.original_page.set_widget_name(&page_name);
    let inserted = notebook.insert_page(
        &swap.original_page,
        swap.tab_label.as_ref(),
        Some(swap.page_index),
    );
    notebook.set_tab_reorderable(&swap.original_page, true);
    notebook.set_current_page(Some(inserted));
    Some(inserted)
}
'''
editor_path.write_text(editor)

mod_path = Path("src/ui/mod.rs")
mod_text = mod_path.read_text()
mod_text = mod_text.replace(
    "pub(crate) use pane_tree_edit::detach_leaf_and_promote;",
    "pub(crate) use pane_tree_edit::{\n"
    "    detach_leaf_and_promote, detach_leaf_for_zoom, restore_zoomed_leaf, ZoomPageSwap,\n"
    "};",
    1,
)
zoom_state_pattern = re.compile(
    r"pub\(crate\) struct ZoomState \{\n"
    r"    pub\(crate\) original_page: gtk4::Widget,\n"
    r"    pub\(crate\) zoomed_terminal: Terminal,\n"
    r"    pub\(crate\) page_index: u32,\n"
    r"    pub\(crate\) tab_label: Option<gtk4::Widget>,\n"
    r"\}"
)
mod_text, count = zoom_state_pattern.subn(
    "pub(crate) struct ZoomState {\n"
    "    pub(crate) swap: ZoomPageSwap,\n"
    "    pub(crate) zoomed_terminal: Terminal,\n"
    "}",
    mod_text,
    count=1,
)
if count != 1:
    raise SystemExit("ZoomState shape not found")
mod_path.write_text(mod_text)

zoom_path = Path("src/ui/zoom.rs")
zoom = zoom_path.read_text()
zoom = zoom.replace("use gtk4::Paned;\n", "", 1)
zoom = zoom.replace(
    "use crate::terminal::{reattach_terminal_to_tree, terminal_working_directory};",
    "use crate::terminal::terminal_working_directory;",
    1,
)

zoom_start = zoom.find("    pub(crate) fn zoom_pane(&self) {")
unzoom_start = zoom.find("    pub(crate) fn unzoom_pane(&self, state: ZoomState) {", zoom_start)
move_start = zoom.find("    pub(crate) fn move_pane_to_new_tab(&self) {", unzoom_start)
if min(zoom_start, unzoom_start, move_start) < 0:
    raise SystemExit("zoom operation markers not found")

new_zoom_ops = r'''    pub(crate) fn zoom_pane(&self) {
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

'''
zoom = zoom[:zoom_start] + new_zoom_ops + zoom[move_start:]
zoom_path.write_text(zoom)
