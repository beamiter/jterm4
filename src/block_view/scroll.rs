//! Block-view scrolling, follow-bottom settling, and widget virtualization.
use gtk::glib;
use gtk::prelude::*;
use gtk::ScrolledWindow;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Ignore sub-pixel adjustment churn. GTK can report tiny floating-point
/// differences after layout even though the viewport is visually stationary;
/// writing those values back wakes every value-changed observer for no benefit.
const SCROLL_EPSILON_PX: f64 = 0.5;
/// Once the computed bottom target is unchanged for this many layout frames,
/// virtualized blocks have had enough time to realize and the pin can release.
const BOTTOM_STABLE_FRAMES: u8 = 4;
/// Safety bound for unusually slow virtual-layout settling.
const MAX_BOTTOM_PIN_TRIES: u8 = 12;

fn scroll_value_changed(current: f64, target: f64) -> bool {
    (current - target).abs() > SCROLL_EPSILON_PX
}

fn next_stable_frame_count(last_target: Option<f64>, target: f64, current: u8) -> u8 {
    match last_target {
        Some(last) if !scroll_value_changed(last, target) => current.saturating_add(1),
        _ => 0,
    }
}

/// Claim or refresh the single follow-bottom settling source.
///
/// A coalesced request still increments `generation`. The active timer observes
/// that change on its next frame and resets its retry/stability counters, giving
/// newly-added virtualized content a full settling window without starting a
/// second timer.
fn request_bottom_pin(
    user_scrolled: bool,
    active: &Cell<bool>,
    generation: &Cell<u64>,
) -> bool {
    if user_scrolled {
        return false;
    }
    generation.set(generation.get().wrapping_add(1));
    !active.replace(true)
}

/// Scrolls the block list to follow the live prompt.
///
/// The synchronous update in `mark_dirty` intentionally runs before GTK lays out
/// a freshly appended block. The short frame-spaced settling pass then handles
/// virtualized blocks whose heights become known only after subsequent layouts.
pub(crate) struct ScrollDebouncer {
    pub(crate) user_scrolled_up: Rc<Cell<bool>>,
    pub(crate) programmatic_scroll: Rc<Cell<bool>>,
    /// At most one frame-spaced follow-bottom source may run at a time. Repeated
    /// output/layout notifications refresh its generation instead of creating
    /// overlapping timers.
    bottom_pin_active: Rc<Cell<bool>>,
    bottom_pin_generation: Rc<Cell<u64>>,
}

impl ScrollDebouncer {
    pub(crate) fn with_scroll_lock(
        user_scrolled_up: Rc<Cell<bool>>,
        programmatic_scroll: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            user_scrolled_up,
            programmatic_scroll,
            bottom_pin_active: Rc::new(Cell::new(false)),
            bottom_pin_generation: Rc::new(Cell::new(0)),
        }
    }

    pub(crate) fn mark_dirty(&self, scroll: &ScrolledWindow) {
        if self.user_scrolled_up.get() {
            return;
        }
        let adj = scroll.vadjustment();
        let target = (adj.upper() - adj.page_size()).max(adj.lower());
        if !scroll_value_changed(adj.value(), target) {
            return;
        }
        // Guard the scroll with the programmatic flag so the scroll-lock detector
        // does not mistake it for the user dragging the scrollbar.
        self.programmatic_scroll.set(true);
        adj.set_value(target);
        self.programmatic_scroll.set(false);
    }

    /// Follow the live prompt across a few settled layout passes.
    ///
    /// With virtual scrolling, hidden blocks have zero GTK height until the
    /// scroll position brings them near the viewport. A single bottom jump can
    /// therefore land short: the jump reveals more blocks, those blocks expand,
    /// and the adjustment upper bound grows on the next frame.
    pub(crate) fn pin_to_bottom_deferred(&self, scroll: &ScrolledWindow) {
        if !request_bottom_pin(
            self.user_scrolled_up.get(),
            &self.bottom_pin_active,
            &self.bottom_pin_generation,
        ) {
            return;
        }

        let scroll = scroll.clone();
        let user_scrolled = self.user_scrolled_up.clone();
        let programmatic = self.programmatic_scroll.clone();
        let bottom_pin_active = self.bottom_pin_active.clone();
        let bottom_pin_generation = self.bottom_pin_generation.clone();
        let observed_generation = Rc::new(Cell::new(bottom_pin_generation.get()));
        let tries = Rc::new(Cell::new(0u8));
        let last_target = Rc::new(Cell::new(None::<f64>));
        let stable_frames = Rc::new(Cell::new(0u8));

        // An idle source that returns Continue can consume all retries before GTK
        // reaches another layout frame. Space retries by roughly one frame so
        // every observation can see newly realized block geometry.
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            if user_scrolled.get() {
                bottom_pin_active.set(false);
                return glib::ControlFlow::Break;
            }

            // Output or layout changed while the timer was active. Refresh the
            // settling budget rather than creating another timer.
            let generation = bottom_pin_generation.get();
            if observed_generation.get() != generation {
                observed_generation.set(generation);
                tries.set(0);
                last_target.set(None);
                stable_frames.set(0);
            }

            if tries.get() >= MAX_BOTTOM_PIN_TRIES {
                bottom_pin_active.set(false);
                return glib::ControlFlow::Break;
            }
            tries.set(tries.get() + 1);

            let adj = scroll.vadjustment();
            let target = (adj.upper() - adj.page_size()).max(adj.lower());
            let next_stable =
                next_stable_frame_count(last_target.get(), target, stable_frames.get());
            last_target.set(Some(target));
            stable_frames.set(next_stable);

            if scroll_value_changed(adj.value(), target) {
                programmatic.set(true);
                adj.set_value(target);
                programmatic.set(false);
            }

            if next_stable >= BOTTOM_STABLE_FRAMES {
                bottom_pin_active.set(false);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    pub(crate) fn reset_scroll_lock(&self) {
        self.user_scrolled_up.set(false);
    }
}

// ─── Virtual Scrolling ────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct ViewportState {
    pub(crate) first_visible: usize,
    pub(crate) last_visible: usize,
    pub(crate) total_height: i32,
}

pub(crate) struct WidgetPool {
    pub(crate) available: Vec<gtk::Box>,
    pub(crate) max_pool_size: usize,
}

impl WidgetPool {
    pub(crate) fn new() -> Self {
        Self {
            available: Vec::new(),
            max_pool_size: 20,
        }
    }

    pub(crate) fn acquire(&mut self) -> Option<gtk::Box> {
        self.available.pop()
    }

    pub(crate) fn release(&mut self, widget: gtk::Box) {
        if self.available.len() >= self.max_pool_size {
            return;
        }

        // Finished-block controllers capture the old block id and action
        // handles. Reusing the outer box without removing them causes stale
        // callbacks and stacks duplicate hover/right-click handlers on every
        // recycle. The new FinishedBlock installs a fresh controller set.
        let controllers = widget.observe_controllers();
        while let Some(controller) = controllers.item(0) {
            let Ok(controller) = controller.downcast::<gtk::EventController>() else {
                break;
            };
            widget.remove_controller(&controller);
        }

        self.available.push(widget);
    }
}

// ─── TermView ─────────────────────────────────────────────────────────────────

/// Shared lists of observer callbacks, keyed by the payload they receive.
pub(crate) type StrCallbacks = Rc<RefCell<Vec<Box<dyn Fn(&str)>>>>;
pub(crate) type IntCallbacks = Rc<RefCell<Vec<Box<dyn Fn(i32)>>>>;
pub(crate) type VoidCallbacks = Rc<RefCell<Vec<Box<dyn Fn()>>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_subpixel_scroll_churn() {
        assert!(!scroll_value_changed(100.0, 100.4));
        assert!(scroll_value_changed(100.0, 100.6));
    }

    #[test]
    fn stable_frame_count_resets_when_layout_moves() {
        assert_eq!(next_stable_frame_count(None, 100.0, 3), 0);
        assert_eq!(next_stable_frame_count(Some(100.0), 100.2, 2), 3);
        assert_eq!(next_stable_frame_count(Some(100.0), 101.0, 3), 0);
    }

    #[test]
    fn bottom_pin_coalesces_and_refreshes_overlapping_requests() {
        let active = Cell::new(false);
        let generation = Cell::new(0u64);

        assert!(request_bottom_pin(false, &active, &generation));
        assert!(active.get());
        assert_eq!(generation.get(), 1);

        assert!(!request_bottom_pin(false, &active, &generation));
        assert!(active.get());
        assert_eq!(generation.get(), 2);

        active.set(false);
        assert!(!request_bottom_pin(true, &active, &generation));
        assert!(!active.get());
        assert_eq!(generation.get(), 2);
    }
}
