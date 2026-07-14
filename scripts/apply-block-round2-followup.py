#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one occurrence, got {count}: {old[:100]!r}")
    file.write_text(text.replace(old, new, 1))


# Keep the sticky refresh source and history persistence tied to TermView's lifetime.
path = "src/block_view/mod.rs"
replace_once(
    path,
    """    resize_tick_id: RefCell<Option<gtk4::TickCallbackId>>,
    /// Tracks per-VTE selections so a drag that crosses block boundaries can be
""",
    """    resize_tick_id: RefCell<Option<gtk4::TickCallbackId>>,
    /// Periodic sticky-header refresh. Remove it explicitly on tab close so its
    /// GTK captures cannot retain a detached block tree.
    sticky_timer_id: RefCell<Option<glib::SourceId>>,
    /// Tracks per-VTE selections so a drag that crosses block boundaries can be
""",
)
replace_once(
    path,
    """impl Drop for TermView {
    fn drop(&mut self) {
        if let Some(id) = self.resize_tick_id.borrow_mut().take() {
            id.remove();
        }
    }
}
""",
    """impl Drop for TermView {
    fn drop(&mut self) {
        if let Err(err) = self.save_history() {
            log::warn!("save block history on close: {err}");
        }
        if let Some(id) = self.resize_tick_id.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = self.sticky_timer_id.borrow_mut().take() {
            id.remove();
        }
    }
}
""",
)
replace_once(
    path,
    """        {
            let sticky = sticky_bar.clone();
            let sticky_label = sticky_label.clone();
""",
    """        let sticky_timer_id = {
            let sticky = sticky_bar.clone();
            let sticky_label = sticky_label.clone();
""",
)
replace_once(
    path,
    """                glib::ControlFlow::Continue
            });
        }

        // ── VTE is used as a display-only widget (fed via feed() in alt-screen mode)
""",
    """                glib::ControlFlow::Continue
            })
        };

        // ── VTE is used as a display-only widget (fed via feed() in alt-screen mode)
""",
)
replace_once(
    path,
    """            resize_tick_id: RefCell::new(None),
            cross_selection,
""",
    """            resize_tick_id: RefCell::new(None),
            sticky_timer_id: RefCell::new(Some(sticky_timer_id)),
            cross_selection,
""",
)

# Make duration filtering explicit and regression-tested.
path = "src/block_view/find.rs"
replace_once(
    path,
    """fn find_match_block<'a>(
""",
    """fn duration_matches(duration: Option<u64>, filters: &BlockFilters) -> bool {
    let needs_duration =
        filters.min_duration_ms.is_some() || filters.max_duration_ms.is_some() || filters.slow_only;
    if !needs_duration {
        return true;
    }
    let Some(duration) = duration else {
        return false;
    };
    if filters.min_duration_ms.is_some_and(|min| duration < min) {
        return false;
    }
    if filters.max_duration_ms.is_some_and(|max| duration > max) {
        return false;
    }
    !filters.slow_only || duration >= filters.slow_threshold_ms
}

fn find_match_block<'a>(
""",
)
replace_once(
    path,
    """                // Duration predicates require a known duration. Background
                // output and legacy history with no timing metadata must not
                // leak into slow/min/max result sets.
                if filters.min_duration_ms.is_some()
                    || filters.max_duration_ms.is_some()
                    || filters.slow_only
                {
                    let Some(duration) = b.duration_ms else {
                        return false;
                    };
                    if filters.min_duration_ms.is_some_and(|min| duration < min) {
                        return false;
                    }
                    if filters.max_duration_ms.is_some_and(|max| duration > max) {
                        return false;
                    }
                    if filters.slow_only && duration < filters.slow_threshold_ms {
                        return false;
                    }
                }
""",
    """                if !duration_matches(b.duration_ms, filters) {
                    return false;
                }
""",
)
replace_once(
    path,
    """mod tests {
    use super::snippet;
""",
    """mod tests {
    use super::{duration_matches, snippet};
    use crate::block_view::BlockFilters;
""",
)
replace_once(
    path,
    """    #[test]
    fn snippet_passes_through_short_line() {
""",
    """    #[test]
    fn unknown_duration_does_not_match_duration_filters() {
        let filters = BlockFilters {
            slow_only: true,
            slow_threshold_ms: 1_000,
            ..Default::default()
        };
        assert!(!duration_matches(None, &filters));
    }

    #[test]
    fn duration_boundaries_are_inclusive() {
        let filters = BlockFilters {
            min_duration_ms: Some(500),
            max_duration_ms: Some(1_500),
            ..Default::default()
        };
        assert!(duration_matches(Some(500), &filters));
        assert!(duration_matches(Some(1_500), &filters));
        assert!(!duration_matches(Some(499), &filters));
        assert!(!duration_matches(Some(1_501), &filters));
    }

    #[test]
    fn duration_is_irrelevant_without_duration_predicates() {
        assert!(duration_matches(None, &BlockFilters::default()));
    }

    #[test]
    fn snippet_passes_through_short_line() {
""",
)
