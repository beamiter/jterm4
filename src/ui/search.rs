//! search — UiState methods extracted from ui (mechanical split, no logic changes)
use gtk4::glib;
use libadwaita as adw;
use adw::prelude::*;
use vte4::TerminalExt;

use super::*;

impl UiState {

    pub(crate) fn toggle_search(&self) {
        let visible = self.search_bar.is_search_mode();
        self.search_bar.set_search_mode(!visible);
        if !visible {
            self.search_entry.grab_focus();
        } else {
            // Clear search highlight when closing
            if let Some(term) = self.current_terminal() {
                term.search_set_regex(None::<&vte4::Regex>, 0);
            }
            self.focus_current_terminal();
        }
    }

    pub(crate) fn search_apply(&self) {
        let text = self.search_entry.text();
        if text.is_empty() {
            return;
        }

        // Detect regex pattern: /pattern/ syntax
        let text_str = text.as_str();
        let (query, use_regex) = if text_str.starts_with('/') && text_str.ends_with('/') && text_str.len() > 2 {
            (text_str[1..text_str.len() - 1].to_string(), true)
        } else {
            (text_str.to_string(), false)
        };

        // Try block search first (in block mode)
        if let Some(term_view) = self.current_term_view() {
            let filters = crate::block_view::BlockFilters {
                use_regex,
                ..Default::default()
            };
            let matches = term_view.search_blocks_with_filters(&query, &filters);
            if !matches.is_empty() {
                if let Some(first_match) = matches.first() {
                    term_view.scroll_to_block(*first_match);
                    return;
                }
            }
        }

        // Fall back to terminal regex search
        if let Some(term) = self.current_terminal() {
            let pattern = if use_regex {
                query
            } else {
                glib::Regex::escape_string(&text).to_string()
            };
            let regex = vte4::Regex::for_search(&pattern, pcre2_sys::PCRE2_CASELESS);
            if let Ok(regex) = regex {
                term.search_set_regex(Some(&regex), 0);
                term.search_set_wrap_around(true);
                term.search_find_next();
            }
        }
    }

    pub(crate) fn search_next(&self) {
        if let Some(term) = self.current_terminal() {
            term.search_find_next();
        }
    }

    pub(crate) fn search_prev(&self) {
        if let Some(term) = self.current_terminal() {
            term.search_find_previous();
        }
    }
}
