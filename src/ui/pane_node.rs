//! Typed structural view of a notebook pane tree.
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

    pub(crate) fn leaves(&self) -> Vec<PaneLeaf> {
        let mut leaves = Vec::new();
        self.collect_leaves(&mut leaves);
        leaves
    }

    pub(crate) fn focused_leaf(&self) -> Option<PaneLeaf> {
        match self {
            Self::Leaf(controller) if controller.terminal().has_focus() => Some(controller.clone()),
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
