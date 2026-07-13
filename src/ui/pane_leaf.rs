//! Typed terminal-pane leaf shared by Block and conventional VTE tabs.
//!
//! The GTK widget tree is a rendering detail; callers should not need to know
//! whether a pane root owns a structured `TermView` or a conventional
//! `VteTerminalView` just to get its live terminal, focus it, or retain the
//! controller on a widget. This is the first step toward a native Block-pane
//! model without introducing Relm4 or enabling unsafe Block splits prematurely.

use std::rc::Rc;

use gtk4::glib::prelude::ObjectExt;
use vte4::Terminal;

use crate::block_view::TermView;
use crate::terminal::{focus_terminal_deferred, VteTerminalView};

const PANE_LEAF_DATA_KEY: &str = "terminal-view-type";

#[derive(Clone)]
pub(crate) enum PaneLeaf {
    Block(Rc<TermView>),
    Vte(Rc<VteTerminalView>),
}

impl PaneLeaf {
    /// Root widget inserted into a Notebook page or Paned leaf.
    pub(crate) fn root_widget(&self) -> gtk4::Widget {
        match self {
            Self::Block(view) => view.widget(),
            Self::Vte(view) => view.widget(),
        }
    }

    /// The live, input-capable VTE owned by this pane.
    ///
    /// Finished Block snapshots contain additional read-only VTE widgets, so
    /// generic widget-tree traversal cannot reliably identify this terminal.
    pub(crate) fn terminal(&self) -> &Terminal {
        match self {
            Self::Block(view) => view.vte(),
            Self::Vte(view) => view.vte(),
        }
    }

    /// Focus the actual live surface rather than the first VTE found in the tree.
    pub(crate) fn grab_focus(&self) {
        match self {
            Self::Block(view) => view.grab_focus(),
            Self::Vte(view) => focus_terminal_deferred(view.vte()),
        }
    }

    pub(crate) fn block_view(&self) -> Option<Rc<TermView>> {
        match self {
            Self::Block(view) => Some(view.clone()),
            Self::Vte(_) => None,
        }
    }

    pub(crate) fn is_block(&self) -> bool {
        matches!(self, Self::Block(_))
    }

    /// Store the typed controller on its GTK leaf root. Keeping the unsafe GTK
    /// object-data boundary here prevents feature code from repeating raw pointer
    /// access and accidentally using different data keys or types.
    pub(crate) fn attach_to(&self, widget: &gtk4::Widget) {
        unsafe {
            widget.set_data::<Self>(PANE_LEAF_DATA_KEY, self.clone());
        }
    }

    /// Recover a directly attached pane leaf from a GTK root widget.
    ///
    /// Split roots are `Paned` containers and intentionally return `None`; focus
    /// and navigation code can then select a concrete child leaf geometrically.
    pub(crate) fn from_widget(widget: &gtk4::Widget) -> Option<Self> {
        unsafe {
            widget
                .data::<Self>(PANE_LEAF_DATA_KEY)
                .map(|pointer| pointer.as_ref().clone())
        }
    }
}
