//! Typed structural view of a notebook pane tree.
//!
//! Every direct or split pane root carries a `PaneLeaf` controller. Reconstructing
//! the GTK `Paned` hierarchy into this recursive model gives focus, active-terminal
//! lookup, and split safety checks one ownership-aware path without introducing
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

    pub(crate) fn contains_block(&self) -> bool {
        match self {
            Self::Leaf(controller) => controller.is_block(),
            Self::Split { start, end } => start.contains_block() || end.contains_block(),
        }
    }

    pub(crate) fn active_terminal(&self) -> Option<Terminal> {
        self.focused_terminal().or_else(|| self.first_terminal())
    }

    pub(crate) fn grab_focus(&self) {
        match self {
            Self::Leaf(controller) => controller.grab_focus(),
            Self::Split { start, end } => {
                if start.contains_focus() {
                    start.grab_focus();
                } else if end.contains_focus() {
                    end.grab_focus();
                } else {
                    start.grab_focus();
                }
            }
        }
    }

    fn contains_focus(&self) -> bool {
        match self {
            Self::Leaf(controller) => controller.terminal().has_focus(),
            Self::Split { start, end } => start.contains_focus() || end.contains_focus(),
        }
    }

    fn focused_terminal(&self) -> Option<Terminal> {
        match self {
            Self::Leaf(controller) if controller.terminal().has_focus() => {
                Some(controller.terminal().clone())
            }
            Self::Leaf(_) => None,
            Self::Split { start, end } => {
                start.focused_terminal().or_else(|| end.focused_terminal())
            }
        }
    }

    fn first_terminal(&self) -> Option<Terminal> {
        match self {
            Self::Leaf(controller) => Some(controller.terminal().clone()),
            Self::Split { start, end } => start.first_terminal().or_else(|| end.first_terminal()),
        }
    }
}
