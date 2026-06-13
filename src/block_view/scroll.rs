//! scroll — extracted from block_view (mechanical split, no logic changes)
use gtk4::prelude::*;
use gtk4::{glib, ScrolledWindow};
use std::cell::{Cell, RefCell};
use std::rc::Rc;



/// Coalesces repeated scroll requests into a single scroll event.
/// Eliminates cascade of timers and provides smooth scrolling under rapid output.
pub(crate) struct ScrollDebouncer {
    pub(crate) dirty: Rc<Cell<bool>>,
    pub(crate) pending_handle: Rc<RefCell<Option<glib::source::SourceId>>>,
    pub(crate) pending_inner: Rc<RefCell<Option<glib::source::SourceId>>>,
    pub(crate) user_scrolled_up: Rc<Cell<bool>>,
    pub(crate) programmatic_scroll: Rc<Cell<bool>>,
}

impl ScrollDebouncer {
    pub(crate) fn with_scroll_lock(
        user_scrolled_up: Rc<Cell<bool>>,
        programmatic_scroll: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            dirty: Rc::new(Cell::new(false)),
            pending_handle: Rc::new(RefCell::new(None)),
            pending_inner: Rc::new(RefCell::new(None)),
            user_scrolled_up,
            programmatic_scroll,
        }
    }

    pub(crate) fn mark_dirty(&self, scroll: &ScrolledWindow) {
        if self.user_scrolled_up.get() {
            return;
        }

        let scroll = scroll.clone();
        let dirty = self.dirty.clone();
        let pending = self.pending_handle.clone();
        let pending_inner = self.pending_inner.clone();
        let programmatic = self.programmatic_scroll.clone();
        let user_up = self.user_scrolled_up.clone();

        if let Some(handle) = pending.borrow_mut().take() {
            handle.remove();
        }
        // Cancel any still-in-flight inner (80ms) timer from a previous burst so
        // they don't accumulate under rapid output, each holding Rc/GLib handles.
        if let Some(handle) = pending_inner.borrow_mut().take() {
            handle.remove();
        }

        dirty.set(true);

        let pending_for_clear = pending.clone();
        let pending_inner_for_set = pending_inner.clone();
        let user_up_inner = user_up.clone();
        let handle =
            glib::timeout_add_local_once(std::time::Duration::from_millis(16), move || {
                let scroll_to_end = |sw: &ScrolledWindow| {
                    // The user may have grabbed the scrollbar between mark_dirty
                    // and this deferred fire — never fight them back to bottom.
                    if user_up_inner.get() {
                        return;
                    }
                    let adj = sw.vadjustment();
                    let target = adj.upper() - adj.page_size();
                    if target > 0.0 && adj.value() < target - 1.0 {
                        programmatic.set(true);
                        adj.set_value(target);
                        programmatic.set(false);
                    }
                };
                scroll_to_end(&scroll);
                // Second scroll after layout fully settles
                let scroll2 = scroll.clone();
                let programmatic2 = programmatic.clone();
                let user_up2 = user_up_inner.clone();
                let pending_inner_for_clear = pending_inner_for_set.clone();
                let inner = glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
                    pending_inner_for_clear.borrow_mut().take();
                    if user_up2.get() {
                        return;
                    }
                    let adj = scroll2.vadjustment();
                    let target = adj.upper() - adj.page_size();
                    if target > 0.0 && adj.value() < target - 1.0 {
                        programmatic2.set(true);
                        adj.set_value(target);
                        programmatic2.set(false);
                    }
                });
                pending_inner_for_set.borrow_mut().replace(inner);
                dirty.set(false);
                pending_for_clear.borrow_mut().take();
            });
        pending.borrow_mut().replace(handle);
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
