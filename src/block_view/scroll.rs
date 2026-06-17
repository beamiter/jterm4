//! scroll — extracted from block_view (mechanical split, no logic changes)
use gtk4::prelude::*;
use gtk4::ScrolledWindow;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Scrolls the block list to follow the live prompt — jterm1's `autoscroll`
/// model, ported faithfully.
///
/// The key (and subtle) property is that the scroll happens **synchronously**,
/// from inside the PTY-reader's event handling, *before* GTK lays out any block
/// that was just appended. At that instant `upper` still reflects the previous
/// layout, so `upper - page` lands the view at the *top* of the freshly-finished
/// block rather than at the bottom of the page-tall live holder. Because nothing
/// re-scrolls after layout settles, the last finished block stays visible with
/// the prompt directly below it. Deferring this to a timer (or re-running it from
/// the adjustment's `changed` signal) reads the settled, larger `upper` and parks
/// the view at the bottom of the blank holder, hiding all history.
pub(crate) struct ScrollDebouncer {
    pub(crate) user_scrolled_up: Rc<Cell<bool>>,
    pub(crate) programmatic_scroll: Rc<Cell<bool>>,
}

impl ScrollDebouncer {
    pub(crate) fn with_scroll_lock(
        user_scrolled_up: Rc<Cell<bool>>,
        programmatic_scroll: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            user_scrolled_up,
            programmatic_scroll,
        }
    }

    pub(crate) fn mark_dirty(&self, scroll: &ScrolledWindow) {
        if self.user_scrolled_up.get() {
            return;
        }
        let adj = scroll.vadjustment();
        let target = (adj.upper() - adj.page_size()).max(adj.lower());
        // Guard the scroll with the programmatic flag so the scroll-lock detector
        // doesn't mistake it for the user dragging the scrollbar.
        self.programmatic_scroll.set(true);
        adj.set_value(target);
        self.programmatic_scroll.set(false);
    }

    pub(crate) fn reset_scroll_lock(&self) {
        self.user_scrolled_up.set(false);
    }
}

// ─── Virtual Scrolling ────────────────────────────────────────────────────────

pub(crate) struct ViewportState {
    pub(crate) first_visible: usize,
    pub(crate) last_visible: usize,
    pub(crate) total_height: i32,
}

impl Clone for ViewportState {
    fn clone(&self) -> Self {
        Self {
            first_visible: self.first_visible,
            last_visible: self.last_visible,
            total_height: self.total_height,
        }
    }
}

pub(crate) struct WidgetPool {
    pub(crate) available: Vec<gtk4::Box>,
    pub(crate) max_pool_size: usize,
}

impl WidgetPool {
    pub(crate) fn new() -> Self {
        Self {
            available: Vec::new(),
            max_pool_size: 20,
        }
    }

    pub(crate) fn acquire(&mut self) -> Option<gtk4::Box> {
        self.available.pop()
    }

    pub(crate) fn release(&mut self, widget: gtk4::Box) {
        if self.available.len() < self.max_pool_size {
            self.available.push(widget);
        }
    }
}

// ─── TermView ─────────────────────────────────────────────────────────────────

/// Shared lists of observer callbacks, keyed by the payload they receive.
pub(crate) type StrCallbacks = Rc<RefCell<Vec<Box<dyn Fn(&str)>>>>;
pub(crate) type IntCallbacks = Rc<RefCell<Vec<Box<dyn Fn(i32)>>>>;
pub(crate) type VoidCallbacks = Rc<RefCell<Vec<Box<dyn Fn()>>>>;
