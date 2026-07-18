//! Cross-block pseudo-continuous text selection.
//!
//! VTE selection is per-Terminal: each finished block owns separate command and
//! output VTEs, so a pointer drag across widgets would otherwise leave unrelated
//! selections behind. This controller keeps the visible selection model singular,
//! while still allowing a deliberate cross-widget drag to copy in document order.
//!
//! V1 granularity is per-widget. Once a drag crosses a VTE boundary, every visible
//! surface in the covered range is selected in full. vte-rs 0.10 does not expose
//! a public cell-range selection API, so finer endpoint granularity would require
//! subclassing.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use vte4::TerminalExt;

use super::{clear_finished_block_selection, FinishedBlock, SelectedBlockIds};

pub(crate) struct CrossSelection {
    finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
    active_vte: vte4::Terminal,
    selected_block_ids: SelectedBlockIds,
    selected_block_id: Rc<Cell<Option<u64>>>,
    selection_anchor_id: Rc<Cell<Option<u64>>>,
    /// Index in the currently mapped document-order VTE list where a drag began.
    start_idx: Cell<Option<usize>>,
    claimed: Cell<bool>,
}

impl CrossSelection {
    pub(crate) fn install(
        block_scroll: &gtk4::ScrolledWindow,
        finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
        active_vte: vte4::Terminal,
        selected_block_ids: SelectedBlockIds,
        selected_block_id: Rc<Cell<Option<u64>>>,
        selection_anchor_id: Rc<Cell<Option<u64>>>,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            finished_blocks,
            active_vte,
            selected_block_ids,
            selected_block_id,
            selection_anchor_id,
            start_idx: Cell::new(None),
            claimed: Cell::new(false),
        });

        // A normal click starts a new native VTE selection. Clear every other
        // terminal first, otherwise two unrelated drags look like one cross-block
        // selection and Ctrl+Shift+C concatenates stale text.
        let click = gtk4::GestureClick::new();
        click.set_button(gtk4::gdk::BUTTON_PRIMARY);
        click.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let scroll_for_click = block_scroll.clone();
        let this_for_click = this.clone();
        click.connect_pressed(move |gesture, _n_press, x, y| {
            let target = this_for_click.vte_at(&scroll_for_click, x, y);
            if target.is_some() {
                this_for_click.clear_block_selection();
            }
            this_for_click.clear_other_selections(target.as_ref());
            // Let the child VTE keep ownership of ordinary word/line/cell selection.
            gesture.set_state(gtk4::EventSequenceState::Denied);
        });
        block_scroll.add_controller(click);

        let drag = gtk4::GestureDrag::new();
        drag.set_button(gtk4::gdk::BUTTON_PRIMARY);
        drag.set_propagation_phase(gtk4::PropagationPhase::Capture);

        let scroll_for_begin = block_scroll.clone();
        let this_for_begin = this.clone();
        drag.connect_drag_begin(move |_, x, y| {
            let start = this_for_begin.vte_index_at(&scroll_for_begin, x, y);
            this_for_begin.start_idx.set(start);
            this_for_begin.claimed.set(false);
            if let Some(start) = start {
                this_for_begin.clear_block_selection();
                let vtes = this_for_begin.ordered_vtes();
                this_for_begin.clear_other_selections(vtes.get(start));
            }
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
            let Some(cur_idx) = this_for_update.vte_index_at(&scroll_for_update, sx + dx, sy + dy)
            else {
                return;
            };
            if cur_idx == start && !this_for_update.claimed.get() {
                return;
            }

            gesture.set_state(gtk4::EventSequenceState::Claimed);
            this_for_update.claimed.set(true);
            this_for_update.paint_range(start, cur_idx);
        });

        let this_for_end = this.clone();
        drag.connect_drag_end(move |_, _, _| {
            this_for_end.start_idx.set(None);
            // Keep the painted selection after release so the copy shortcut works.
        });

        block_scroll.add_controller(drag);
        this
    }

    fn clear_block_selection(&self) {
        if self.selected_block_id.get().is_none() {
            return;
        }
        let finished = self.finished_blocks.borrow();
        clear_finished_block_selection(
            &finished,
            &self.selected_block_ids,
            &self.selected_block_id,
            &self.selection_anchor_id,
        );
    }

    /// Every terminal surface in document order, including currently hidden ones.
    fn all_vtes(&self) -> Vec<vte4::Terminal> {
        let finished = self.finished_blocks.borrow();
        let mut vtes = Vec::with_capacity(finished.len().saturating_mul(2) + 1);
        for block in finished.iter() {
            vtes.push(block.command_vte.clone());
            vtes.push(block.output_vte.clone());
        }
        vtes.push(self.active_vte.clone());
        vtes
    }

    /// Only mapped surfaces participate in visual selection/copy. A collapsed
    /// output VTE or a virtualized off-screen card must not contribute hidden text.
    fn ordered_vtes(&self) -> Vec<vte4::Terminal> {
        self.all_vtes()
            .into_iter()
            .filter(|vte| vte.is_mapped() && vte.is_visible())
            .collect()
    }

    fn clear_other_selections(&self, keep: Option<&vte4::Terminal>) {
        for vte in self.all_vtes() {
            if keep.map(|target| target != &vte).unwrap_or(true) {
                vte.unselect_all();
            }
        }
    }

    fn vte_at(
        &self,
        block_scroll: &gtk4::ScrolledWindow,
        x: f64,
        y: f64,
    ) -> Option<vte4::Terminal> {
        let picked = block_scroll.pick(x, y, gtk4::PickFlags::DEFAULT)?;
        self.ordered_vtes()
            .into_iter()
            .find(|vte| widget_contains(vte, &picked))
    }

    fn vte_index_at(&self, block_scroll: &gtk4::ScrolledWindow, x: f64, y: f64) -> Option<usize> {
        let picked = block_scroll.pick(x, y, gtk4::PickFlags::DEFAULT)?;
        self.ordered_vtes()
            .iter()
            .position(|vte| widget_contains(vte, &picked))
    }

    fn paint_range(&self, a: usize, b: usize) {
        let vtes = self.ordered_vtes();
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        self.clear_all();
        for (index, vte) in vtes.iter().enumerate() {
            if index >= lo && index <= hi {
                vte.select_all();
            }
        }
    }

    pub(crate) fn clear_all(&self) {
        for vte in self.all_vtes() {
            vte.unselect_all();
        }
    }

    /// Collect every visible selected VTE in document order. This handles both a
    /// native single-surface selection and a cross-widget drag.
    pub(crate) fn copy_text(&self) -> Option<String> {
        let mut parts = Vec::new();
        for vte in self.ordered_vtes() {
            if !vte.has_selection() {
                continue;
            }
            if let Some(text) = vte.text_selected(vte4::Format::Text) {
                let text = text.to_string();
                if !text.is_empty() {
                    parts.push(text);
                }
            }
        }

        (!parts.is_empty()).then(|| parts.join("\n"))
    }
}

/// True if `needle` is `haystack` or one of its descendants. GTK's `pick()`
/// returns the deepest widget at a coordinate, not necessarily the VTE itself.
fn widget_contains(haystack: &impl IsA<gtk4::Widget>, needle: &gtk4::Widget) -> bool {
    let haystack = haystack.upcast_ref::<gtk4::Widget>();
    let mut current = Some(needle.clone());
    while let Some(widget) = current {
        if &widget == haystack {
            return true;
        }
        current = widget.parent();
    }
    false
}
