//! Structural edits for the native GTK pane tree.
//!
//! Closing and moving a split leaf both perform the same mutation: detach the
//! target leaf, remove its parent `Paned`, and promote the sibling into either the
//! ancestor `Paned` or the original Notebook page. Keeping that mutation here
//! prevents lifecycle paths from implementing subtly different widget surgery.

use gtk4::prelude::*;
use gtk4::{Notebook, Paned, Widget};

use crate::terminal::reattach_terminal_to_tree;

/// Detach `leaf_root` from its parent split and promote its sibling.
///
/// Returns the promoted sibling. A direct Notebook leaf has no split to collapse
/// and returns `None`; callers can then apply their normal whole-tab behavior.
pub(crate) fn detach_leaf_and_promote(notebook: &Notebook, leaf_root: &Widget) -> Option<Widget> {
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
pub(crate) fn restore_zoomed_leaf(notebook: &Notebook, swap: &ZoomPageSwap) -> Option<u32> {
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
