//! Typed structural view of a notebook pane tree.
//!
//! Direct Block/VTE roots carry a `PaneLeaf` controller. Older split-created VTE
//! leaves do not yet have a controller, but they are still represented by their
//! live terminal. This lets focus and split guards reason about one recursive
//! `Leaf`/`Split` model without introducing Relm4 or enabling Block splits.

use gtk4::prelude::*;
use gtk4::{Paned, Widget};
use vte4::Terminal;

use super::PaneLeaf;
use crate::terminal::{find_first_terminal, focus_terminal_deferred};

#[derive(Clone)]
pub(crate) enum PaneNode {
    Leaf {
        controller: Option<PaneLeaf>,
        terminal: Terminal,
    },
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

        let controller = PaneLeaf::from_widget(widget);
        let terminal = controller
            .as_ref()
            .map(|leaf| leaf.terminal().clone())
            .or_else(|| find_first_terminal(widget))?;
        Some(Self::Leaf {
            controller,
            terminal,
        })
    }

    pub(crate) fn contains_block(&self) -> bool {
        match self {
            Self::Leaf { controller, .. } => controller.as_ref().is_some_and(PaneLeaf::is_block),
            Self::Split { start, end } => start.contains_block() || end.contains_block(),
        }
    }

    pub(crate) fn active_terminal(&self) -> Option<Terminal> {
        self.focused_terminal().or_else(|| self.first_terminal())
    }

    pub(crate) fn grab_focus(&self) {
        match self {
            Self::Leaf {
                controller: Some(controller),
                ..
            } => controller.grab_focus(),
            Self::Leaf { terminal, .. } => focus_terminal_deferred(terminal),
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
            Self::Leaf { terminal, .. } => terminal.has_focus(),
            Self::Split { start, end } => start.contains_focus() || end.contains_focus(),
        }
    }

    fn focused_terminal(&self) -> Option<Terminal> {
        match self {
            Self::Leaf { terminal, .. } if terminal.has_focus() => Some(terminal.clone()),
            Self::Leaf { .. } => None,
            Self::Split { start, end } => {
                start.focused_terminal().or_else(|| end.focused_terminal())
            }
        }
    }

    fn first_terminal(&self) -> Option<Terminal> {
        match self {
            Self::Leaf { terminal, .. } => Some(terminal.clone()),
            Self::Split { start, end } => start.first_terminal().or_else(|| end.first_terminal()),
        }
    }
}
