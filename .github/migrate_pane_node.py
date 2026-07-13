from pathlib import Path
import re

ui_dir = Path("src/ui")

for path in ui_dir.glob("*.rs"):
    text = path.read_text()
    if "TerminalViewType" in text:
        path.write_text(text.replace("TerminalViewType", "PaneLeaf"))

mod_path = ui_dir / "mod.rs"
mod_text = mod_path.read_text()
mod_text = mod_text.replace("mod pane_leaf;\n", "mod pane_leaf;\nmod pane_node;\n", 1)
mod_text = mod_text.replace(
    "pub(crate) use pane_leaf::PaneLeaf;\n",
    "pub(crate) use pane_leaf::PaneLeaf;\npub(crate) use pane_node::PaneNode;\n",
    1,
)
mod_text = re.sub(
    r"\n/// Compatibility name while existing GTK call sites migrate incrementally to\n"
    r"/// the pane-oriented terminology\. This remains the same typed enum, not a second\n"
    r"/// controller or framework layer\.\n"
    r"pub\(crate\) type PaneLeaf = PaneLeaf;\n",
    "\n",
    mod_text,
)
mod_path.write_text(mod_text)

pane_node = r'''//! Typed structural view of a notebook pane tree.
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
            Self::Split { start, end } => start
                .focused_terminal()
                .or_else(|| end.focused_terminal()),
        }
    }

    fn first_terminal(&self) -> Option<Terminal> {
        match self {
            Self::Leaf { terminal, .. } => Some(terminal.clone()),
            Self::Split { start, end } => start.first_terminal().or_else(|| end.first_terminal()),
        }
    }
}
'''
(ui_dir / "pane_node.rs").write_text(pane_node)

actions_path = ui_dir / "actions.rs"
actions = actions_path.read_text()
actions = actions.replace(
    "use crate::terminal::{\n    find_first_terminal, find_focused_terminal, focus_terminal_deferred, terminal_working_directory,\n};",
    "use crate::terminal::terminal_working_directory;",
)
start = actions.find("    /// Focus a direct pane leaf through its typed controller.")
end = actions.find("    pub(crate) fn current_term_view", start)
if start < 0 or end < 0:
    raise SystemExit("actions PaneLeaf accessor markers not found")
replacement = '''    /// Focus the active leaf through the recursive typed pane tree.
    pub(crate) fn focus_terminal_in_page(&self, widget: &gtk4::Widget) {
        if let Some(node) = PaneNode::from_widget(widget) {
            node.grab_focus();
        }
    }

    pub(crate) fn current_terminal(&self) -> Option<Terminal> {
        self.notebook
            .current_page()
            .and_then(|page_num| self.notebook.nth_page(Some(page_num)))
            .and_then(|widget| PaneNode::from_widget(&widget))
            .and_then(|node| node.active_terminal())
    }

    pub(crate) fn current_pane_leaf(&self) -> Option<PaneLeaf> {
        self.notebook
            .current_page()
            .and_then(|page_num| self.notebook.nth_page(Some(page_num)))
            .and_then(|widget| PaneLeaf::from_widget(&widget))
    }

'''
actions_path.write_text(actions[:start] + replacement + actions[end:])

panes_path = ui_dir / "panes.rs"
panes = panes_path.read_text()
guard_start = panes.find("        // A TermView owns a whole block list around its live VTE.")
guard_end = panes.find(
    "        let Some(current_term) = self.current_terminal() else {", guard_start
)
if guard_start < 0 or guard_end < 0:
    raise SystemExit("split guard markers not found")
guard = '''        let page_node = self
            .notebook
            .current_page()
            .and_then(|page| self.notebook.nth_page(Some(page)))
            .and_then(|widget| PaneNode::from_widget(&widget));

        // A Block leaf owns a structured history surface around its live VTE.
        // Refuse before creating a second PTY until split construction itself
        // creates typed Block leaves. Existing VTE split trees remain supported.
        if page_node.as_ref().is_some_and(PaneNode::contains_block) {
            let dialog = adw::AlertDialog::new(
                Some("Split panes require VTE mode"),
                Some(
                    "Block mode keeps command history in a structured view and cannot yet be split safely. Change terminal_mode to \\"vte\\" for split panes.",
                ),
            );
            dialog.add_response("ok", "OK");
            dialog.set_default_response(Some("ok"));
            dialog.present(Some(&self.window));
            log::warn!("Blocked an unsupported block-mode split before spawning a PTY");
            return;
        }
'''
panes_path.write_text(panes[:guard_start] + guard + panes[guard_end:])

leftovers = [
    str(path)
    for path in ui_dir.glob("*.rs")
    if "TerminalViewType" in path.read_text()
]
if leftovers:
    raise SystemExit(f"legacy alias remains in {leftovers}")
