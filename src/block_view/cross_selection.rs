//! Cross-block pseudo-continuous text selection.
//!
//! VTE selection is per-Terminal: each finished block owns a separate VTE so a
//! pointer drag from the tail of one block into another paints two unrelated
//! selections in a vanilla setup. This module sits above `block_scroll` and
//! turns a drag that crosses widget boundaries into a contiguous selection
//! across the involved VTEs.
//!
//! V1 granularity: per-widget. Crossed blocks (and the active live VTE if the
//! drag reaches it) are fully `select_all()`'d; endpoints keep whatever
//! per-cell selection VTE has already painted from the same drag.
//! vte-rs 0.10 does not expose `select_text(col, row, col, row)`, so finer
//! granularity would have to go through subclassing — left for V2.
//!
//! Single-block drags are untouched: the controller installs in the Capture
//! phase but only claims the gesture once the pointer leaves the block where
//! the drag started, so VTE's native per-cell selection still owns the common
//! case.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use vte4::TerminalExt;

use crate::block_view::blocks::FinishedBlock;

pub(crate) struct CrossSelection {
    finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
    active_vte: vte4::Terminal,
    /// Index in widget-order of where the current drag began (None when idle
    /// or the drag started outside any tracked VTE).
    start_idx: Cell<Option<usize>>,
    /// Once we've claimed the gesture and started painting cross-block
    /// selection, stay claimed for the rest of the drag.
    claimed: Cell<bool>,
}

impl CrossSelection {
    pub(crate) fn install(
        block_scroll: &gtk4::ScrolledWindow,
        finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
        active_vte: vte4::Terminal,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            finished_blocks,
            active_vte,
            start_idx: Cell::new(None),
            claimed: Cell::new(false),
        });

        let drag = gtk4::GestureDrag::new();
        drag.set_button(gtk4::gdk::BUTTON_PRIMARY);
        drag.set_propagation_phase(gtk4::PropagationPhase::Capture);

        let scroll_for_begin = block_scroll.clone();
        let this_for_begin = this.clone();
        drag.connect_drag_begin(move |_, x, y| {
            this_for_begin
                .start_idx
                .set(this_for_begin.vte_index_at(&scroll_for_begin, x, y));
            this_for_begin.claimed.set(false);
        });

        let scroll_for_update = block_scroll.clone();
        let this_for_update = this.clone();
        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some(start) = this_for_update.start_idx.get() else {
                return;
            };
            let Some((sx, sy)) = gesture.start_point() else {
                return;
            };
            let cur = this_for_update.vte_index_at(&scroll_for_update, sx + dx, sy + dy);
            let Some(cur_idx) = cur else { return };
            if cur_idx == start && !this_for_update.claimed.get() {
                // Still within the original widget — let VTE's native gesture
                // own the per-cell selection.
                return;
            }
            // Crossed a boundary: claim and paint per-widget select_all on the
            // covered range.
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            this_for_update.claimed.set(true);
            this_for_update.paint_range(start, cur_idx);
        });

        let this_for_end = this.clone();
        drag.connect_drag_end(move |_, _, _| {
            this_for_end.start_idx.set(None);
            // Leave `claimed` and the painted selections in place so the user
            // can copy with Ctrl+Shift+C after releasing.
        });

        block_scroll.add_controller(drag);
        this
    }

    /// Total VTE widgets in document order: each finished block contributes one
    /// (its output_vte — the command_vte is a single row above it and rarely
    /// the drag target), and the live active_vte sits at the end.
    fn ordered_vtes(&self) -> Vec<vte4::Terminal> {
        let mut v: Vec<vte4::Terminal> = self
            .finished_blocks
            .borrow()
            .iter()
            .map(|b| b.output_vte.clone())
            .collect();
        v.push(self.active_vte.clone());
        v
    }

    /// Find which VTE in `ordered_vtes()` the pointer `(x, y)` (in
    /// `block_scroll` coords) lies over. Returns None when the pointer is over
    /// chrome/empty space.
    fn vte_index_at(&self, block_scroll: &gtk4::ScrolledWindow, x: f64, y: f64) -> Option<usize> {
        let picked = block_scroll.pick(x, y, gtk4::PickFlags::DEFAULT)?;
        let vtes = self.ordered_vtes();
        for (i, vte) in vtes.iter().enumerate() {
            if widget_contains(vte, &picked) {
                return Some(i);
            }
        }
        None
    }

    /// Set selection on every VTE in [min(a,b)..=max(a,b)] and clear all
    /// others. Idempotent — safe to call on every drag-update frame.
    fn paint_range(&self, a: usize, b: usize) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        for (i, vte) in self.ordered_vtes().iter().enumerate() {
            if i >= lo && i <= hi {
                vte.select_all();
            } else {
                vte.unselect_all();
            }
        }
    }

    /// Collect text from every VTE that currently has a selection, in widget
    /// order, joined with newlines. Used by Ctrl+Shift+C when more than one
    /// VTE is selected (e.g. after a cross-block drag).
    pub(crate) fn copy_text(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        for vte in self.ordered_vtes() {
            if !vte.has_selection() {
                continue;
            }
            if let Some(text) = vte.text_selected(vte4::Format::Text) {
                let s = text.to_string();
                if !s.is_empty() {
                    parts.push(s);
                }
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    /// True when at least two VTEs are currently selected — the signal that
    /// `copy_text()` is preferable to a single-widget VTE read.
    pub(crate) fn has_cross_selection(&self) -> bool {
        let mut count = 0;
        for vte in self.ordered_vtes() {
            if vte.has_selection() {
                count += 1;
                if count >= 2 {
                    return true;
                }
            }
        }
        false
    }
}

/// True if `needle` is `haystack` or one of its descendants. GTK's `pick()`
/// returns the deepest widget at a coordinate (often a text view inside the
/// VTE), so direct identity comparison won't match the VTE itself.
fn widget_contains(haystack: &impl IsA<gtk4::Widget>, needle: &gtk4::Widget) -> bool {
    let haystack = haystack.upcast_ref::<gtk4::Widget>();
    let mut cur: Option<gtk4::Widget> = Some(needle.clone());
    while let Some(w) = cur {
        if &w == haystack {
            return true;
        }
        cur = w.parent();
    }
    false
}
