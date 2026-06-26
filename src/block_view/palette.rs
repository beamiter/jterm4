//! palette — extracted from block_view (mechanical split, no logic changes)
//!
//! Fuzzy-searchable command-history popover (Ctrl+P): pop up a `Popover` over the
//! block scroller, take a most-recent-first deduped command list, score each entry
//! with a subsequence fuzzy match, and on selection clear the live shell line and
//! type the chosen command (without executing) so the user can edit before Enter.

use gtk4::prelude::*;
use gtk4::{glib, Orientation, ScrolledWindow};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::Terminal;

use crate::pty::OwnedPty;

/// Subsequence fuzzy match: returns `Some(score)` if every char of `query`
/// appears in `text` in order (case-insensitive), else `None`. Lower score is a
/// better match (penalizes a late first match and gaps between matched chars).
pub(crate) fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score: i64 = 0;
    let mut last: i64 = -1;
    for (ti, ch) in text.to_lowercase().chars().enumerate() {
        if qi < q.len() && ch == q[qi] {
            score += if last < 0 {
                ti as i64
            } else {
                ti as i64 - last - 1
            };
            last = ti as i64;
            qi += 1;
        }
    }
    if qi == q.len() {
        Some(score)
    } else {
        None
    }
}

/// A command-history entry for the palette, carrying the outcome metadata the
/// failed/slow filters need (the plain command list can't express those).
pub(crate) struct PaletteEntry {
    pub(crate) cmd: String,
    pub(crate) failed: bool,
    pub(crate) slow: bool,
}

/// A run taking longer than this is flagged "slow" for the palette filter.
pub(crate) const PALETTE_SLOW_MS: u64 = 2000;

pub(crate) fn show_command_palette(
    parent: &ScrolledWindow,
    entries: Vec<PaletteEntry>,
    pty: Rc<OwnedPty>,
    typed_cmd: Rc<RefCell<String>>,
    live_vte: Terminal,
) {
    let popover = gtk4::Popover::new();
    popover.set_parent(parent);
    popover.set_has_arrow(false);
    popover.set_autohide(true);
    popover.add_css_class("command-palette");
    popover.set_position(gtk4::PositionType::Bottom);
    let pw = parent.width().max(1);
    popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(pw / 2, 0, 1, 1)));

    let vbox = gtk4::Box::new(Orientation::Vertical, 6);
    vbox.set_size_request(540, -1);

    let entry = gtk4::SearchEntry::new();
    entry.set_placeholder_text(Some("Search command history…"));
    vbox.append(&entry);

    // Outcome filters: restrict the list to failed-only / slow-only runs. The
    // backend metadata (exit code, duration) rides along on each PaletteEntry.
    let filter_row = gtk4::Box::new(Orientation::Horizontal, 6);
    let failed_toggle = gtk4::ToggleButton::with_label("Failed");
    failed_toggle.set_tooltip_text(Some("Show only commands that exited non-zero"));
    failed_toggle.add_css_class("flat");
    let slow_toggle = gtk4::ToggleButton::with_label("Slow");
    slow_toggle.set_tooltip_text(Some("Show only commands slower than 2s"));
    slow_toggle.add_css_class("flat");
    filter_row.append(&failed_toggle);
    filter_row.append(&slow_toggle);
    vbox.append(&filter_row);

    let failed_only = Rc::new(Cell::new(false));
    let slow_only = Rc::new(Cell::new(false));

    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::Single);
    list.add_css_class("command-palette-list");

    let scroller = ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroller.set_min_content_height(300);
    scroller.set_max_content_height(300);
    scroller.set_child(Some(&list));
    vbox.append(&scroller);
    popover.set_child(Some(&vbox));

    let entries = Rc::new(entries);
    let filtered: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let populate = {
        let list = list.clone();
        let entries = entries.clone();
        let filtered = filtered.clone();
        let failed_only = failed_only.clone();
        let slow_only = slow_only.clone();
        move |query: &str| {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let want_failed = failed_only.get();
            let want_slow = slow_only.get();
            let mut scored: Vec<(i64, &str)> = entries
                .iter()
                .filter(|e| (!want_failed || e.failed) && (!want_slow || e.slow))
                .filter_map(|e| fuzzy_score(query, &e.cmd).map(|s| (s, e.cmd.as_str())))
                .collect();
            // Stable sort keeps recency (input order) as the tiebreak.
            scored.sort_by_key(|(s, _)| *s);
            let mut keep = Vec::with_capacity(scored.len());
            for (_, c) in scored {
                let row_label = gtk4::Label::new(Some(c));
                row_label.set_halign(gtk4::Align::Start);
                row_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                row_label.add_css_class("command-palette-row");
                let row = gtk4::ListBoxRow::new();
                row.set_child(Some(&row_label));
                list.append(&row);
                keep.push(c.to_string());
            }
            *filtered.borrow_mut() = keep;
            if let Some(first) = list.row_at_index(0) {
                list.select_row(Some(&first));
            }
        }
    };
    populate("");

    // Toggling a filter re-runs the current query through the new predicate.
    {
        let populate = populate.clone();
        let entry = entry.clone();
        let failed_only = failed_only.clone();
        failed_toggle.connect_toggled(move |btn| {
            failed_only.set(btn.is_active());
            populate(entry.text().as_str());
        });
    }
    {
        let populate = populate.clone();
        let entry = entry.clone();
        let slow_only = slow_only.clone();
        slow_toggle.connect_toggled(move |btn| {
            slow_only.set(btn.is_active());
            populate(entry.text().as_str());
        });
    }

    let choose: Rc<dyn Fn()> = {
        let list = list.clone();
        let filtered = filtered.clone();
        let popover = popover.clone();
        let scroll = parent.clone();
        Rc::new(move || {
            let idx = list.selected_row().map(|r| r.index()).unwrap_or(-1);
            if idx >= 0 {
                if let Some(cmd) = filtered.borrow().get(idx as usize) {
                    pty.write_bytes(b"\x15");
                    pty.write_bytes(cmd.as_bytes());
                    typed_cmd.borrow_mut().clear();
                }
            }
            popover.popdown();
            // Dismissing the popover returns focus to the live VTE, which makes the
            // ScrolledWindow scroll to reveal the holder's *top* (jumping up into
            // history). Re-pin to the bottom so the user lands back on the prompt
            // with the recalled command.
            let scroll = scroll.clone();
            let live_vte = live_vte.clone();
            glib::idle_add_local_once(move || {
                live_vte.grab_focus();
                let adj = scroll.vadjustment();
                adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
            });
        })
    };

    {
        let populate = populate.clone();
        entry.connect_search_changed(move |e| populate(e.text().as_str()));
    }

    {
        let choose = choose.clone();
        list.connect_row_activated(move |list, row| {
            list.select_row(Some(row));
            choose();
        });
    }

    {
        let list = list.clone();
        let popover = popover.clone();
        let choose = choose.clone();
        let key = gtk4::EventControllerKey::new();
        key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, keyval, _, _| {
            use gtk4::gdk::Key;
            let n_rows = {
                let mut n = 0;
                while list.row_at_index(n).is_some() {
                    n += 1;
                }
                n
            };
            let cur = list.selected_row().map(|r| r.index()).unwrap_or(-1);
            match keyval {
                Key::Up => {
                    if n_rows > 0 {
                        let next = if cur <= 0 { n_rows - 1 } else { cur - 1 };
                        if let Some(r) = list.row_at_index(next) {
                            list.select_row(Some(&r));
                        }
                    }
                    glib::Propagation::Stop
                }
                Key::Down => {
                    if n_rows > 0 {
                        let next = if cur < 0 || cur >= n_rows - 1 {
                            0
                        } else {
                            cur + 1
                        };
                        if let Some(r) = list.row_at_index(next) {
                            list.select_row(Some(&r));
                        }
                    }
                    glib::Propagation::Stop
                }
                Key::Return | Key::KP_Enter => {
                    choose();
                    glib::Propagation::Stop
                }
                Key::Escape => {
                    popover.popdown();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        entry.add_controller(key);
    }

    // A Popover with an explicit parent must be unparented when dismissed or it
    // leaks (and warns at teardown).
    popover.connect_closed(|p| p.unparent());

    popover.popup();
    entry.grab_focus();
}
