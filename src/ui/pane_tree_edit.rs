//! Structural edits for the native GTK pane tree.
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
