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
const PANE_SESSION_ID_DATA_KEY: &str = "terminal-session-id";
const PANE_REMOTE_DATA_KEY: &str = "terminal-remote-pane";

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

    /// Insert bytes into the pane's live input surface without submitting a
    /// trailing newline. Palettes and workflows use this review-first path for
    /// both terminal backends.
    pub(crate) fn write_input(&self, data: &[u8]) {
        match self {
            Self::Block(view) => view.write_input(data),
            Self::Vte(view) => view.write_input(data),
        }
    }

    /// Insert one command for review without submitting it. Unlike the generic
    /// input path, this rejects all terminal control characters so a history,
    /// workflow, file name, or model response cannot smuggle Enter into the PTY.
    pub(crate) fn write_review_input(
        &self,
        text: &str,
    ) -> Result<(), crate::review_input::ReviewInputError> {
        let text = crate::review_input::validate(text)?;
        self.write_input(text.as_bytes());
        Ok(())
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

    /// `(pty master fd, shell pid)` used for foreground-process inspection.
    /// Block mode owns an `OwnedPty`, while conventional mode lets VTE own it;
    /// keeping that distinction here prevents feature code from accidentally
    /// asking the Block live VTE for a PTY it does not own.
    pub(crate) fn process_probe(&self) -> (i32, i32) {
        match self {
            Self::Block(view) => (view.pty_fd_i32(), view.pid_i32()),
            Self::Vte(view) => (view.pty_fd_i32(), view.pid_i32()),
        }
    }

    /// Persist the per-pane session identity on the actual leaf root. Split
    /// descendants cannot all be represented by the tab-number keyed map.
    pub(crate) fn set_session_id(&self, session_id: &str) {
        unsafe {
            self.root_widget()
                .set_data::<String>(PANE_SESSION_ID_DATA_KEY, session_id.to_owned());
        }
    }

    pub(crate) fn session_id(&self) -> Option<String> {
        unsafe {
            self.root_widget()
                .data::<String>(PANE_SESSION_ID_DATA_KEY)
                .map(|value| value.as_ref().clone())
        }
    }

    /// Mark the process hosted by this exact leaf as a remote connection.
    ///
    /// A tab may later be split, at which point tab-level connection metadata is
    /// no longer enough to tell the remote primary from a local sibling. Keeping
    /// the bit on the leaf lets structural operations avoid moving a controller
    /// whose reconnect callbacks are intentionally bound to its original tab.
    pub(crate) fn set_remote(&self, remote: bool) {
        unsafe {
            self.root_widget()
                .set_data::<bool>(PANE_REMOTE_DATA_KEY, remote);
        }
    }

    pub(crate) fn is_remote(&self) -> bool {
        unsafe {
            self.root_widget()
                .data::<bool>(PANE_REMOTE_DATA_KEY)
                .is_some_and(|value| *value.as_ref())
        }
    }

    pub(crate) fn restorable_command(&self) -> Option<String> {
        let (pty_fd, shell_pid) = self.process_probe();
        crate::state::restorable_command_for_pty(pty_fd, shell_pid)
    }

    pub(crate) fn foreground_process_name(&self) -> Option<String> {
        let (pty_fd, shell_pid) = self.process_probe();
        crate::state::foreground_process_name_for_pty(pty_fd, shell_pid)
    }

    /// Terminate this leaf's shell and its process group through the
    /// backend-neutral process teardown path.
    pub(crate) fn kill(&self) {
        let (_, shell_pid) = self.process_probe();
        crate::state::terminate_terminal_process(shell_pid);
    }

    /// Store the typed controller on its GTK leaf root. Keeping the unsafe GTK
    /// object-data boundary here prevents feature code from repeating raw pointer
    /// access and accidentally using different data keys or types.
    pub(crate) fn attach_to(&self, widget: &gtk4::Widget) {
        unsafe {
            widget.set_data::<Self>(PANE_LEAF_DATA_KEY, self.clone());
        }
        if let Self::Block(view) = self {
            crate::command_fix::attach_to_block(view);
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
