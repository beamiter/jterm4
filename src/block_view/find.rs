//! find — extracted from block_view (mechanical split, no logic changes)
//!
//! Find-within-blocks: VTE's native PCRE2 highlighter paints every hit inside
//! each finished block's command/output VTE; we only track which (block, surface)
//! each hit belongs to so Next/Prev can step the per-VTE search cursor across
//! block boundaries. Also hosts the metadata-only filter pass used by the
//! command palette's failed/slow toggles and by the debug dashboard counts.

use gtk4::glib;
use gtk4::prelude::*;
use vte4::TerminalExt;

use super::{contains_case_insensitive, select_finished_block, BlockFilters, TermView};

/// One hit from a find-within-blocks pass. With VTE-backed blocks the match
/// position lives inside the VTE itself (highlighted automatically by
/// `search_set_regex`); we only remember which (block, surface) it belongs
/// to so navigation can move the per-VTE search cursor to the right widget.
#[derive(Clone)]
pub(crate) struct FindMatch {
    pub(crate) block_id: u64,
    /// false = command VTE, true = output VTE.
    pub(crate) is_output: bool,
}

#[derive(Default)]
pub(crate) struct FindState {
    pub(crate) matches: Vec<FindMatch>,
    /// Index into `matches` of the currently focused hit.
    pub(crate) current: usize,
}

/// One result row from a cross-block ripgrep-style scan. Carries enough
/// context for a flat result list — block id (for jump), surface flag (so
/// the per-block VTE search cursor goes to the right widget), the 1-based
/// line number inside that surface, the line snippet itself (trimmed/
/// truncated for display), and a one-line cmd preview for context.
#[derive(Clone, Debug)]
pub struct CrossBlockHit {
    pub block_id: u64,
    pub is_output: bool,
    pub line_no: usize,
    pub line_text: String,
    pub cmd_preview: String,
}

/// Trim a line to a reasonable display width — the palette row is one
/// horizontal line so an unbounded long line (think bundled JSON) would
/// just blow out the dialog width. We truncate with a leading ellipsis if
/// the match isn't near the start, but for the MVP we just hard-cap.
fn snippet(line: &str) -> String {
    const CAP: usize = 240;
    if line.len() <= CAP {
        line.to_string()
    } else {
        let mut s = line[..CAP].to_string();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::snippet;

    #[test]
    fn snippet_passes_through_short_line() {
        assert_eq!(snippet("hello world"), "hello world");
    }

    #[test]
    fn snippet_truncates_long_line_with_ellipsis() {
        let long: String = "a".repeat(500);
        let out = snippet(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().filter(|&c| c == 'a').count(), 240);
    }
}

#[allow(dead_code)]
impl TermView {
    /// Search blocks for a query string (case-insensitive).
    /// Returns indices of matching blocks.
    pub fn search_blocks(&self, query: &str) -> Vec<usize> {
        self.search_blocks_with_filters(query, &BlockFilters::default())
    }

    /// Search blocks with optional filters
    pub fn search_blocks_with_filters(&self, query: &str, filters: &BlockFilters) -> Vec<usize> {
        let q = query.to_lowercase();
        let q_bytes = q.as_bytes();

        let re = if filters.use_regex && !query.is_empty() {
            regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .ok()
        } else {
            None
        };

        let results: Vec<usize> = self
            .block_data
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                let text_match = if q.is_empty() {
                    true
                } else if let Some(ref re) = re {
                    re.is_match(&b.prompt) || re.is_match(&b.cmd) || re.is_match(&b.output)
                } else {
                    contains_case_insensitive(b.prompt.as_bytes(), q_bytes)
                        || contains_case_insensitive(b.cmd.as_bytes(), q_bytes)
                        || contains_case_insensitive(b.output.as_bytes(), q_bytes)
                };

                if !text_match {
                    return false;
                }

                // Exit code filter
                if let Some(exit_code) = filters.exit_code {
                    if b.exit_code != exit_code {
                        return false;
                    }
                }

                // Failed only filter
                if filters.failed_only && b.exit_code == 0 {
                    return false;
                }

                // Duration filters
                if let Some(duration) = b.duration_ms {
                    if let Some(min_dur) = filters.min_duration_ms {
                        if duration < min_dur {
                            return false;
                        }
                    }
                    if let Some(max_dur) = filters.max_duration_ms {
                        if duration > max_dur {
                            return false;
                        }
                    }
                    if filters.slow_only && duration < filters.slow_threshold_ms {
                        return false;
                    }
                }

                true
            })
            .map(|(i, _)| i)
            .collect();

        results
    }

    /// Highlight every occurrence of `query` across the finished blocks and
    /// focus the first hit. Returns (current_1based, total); (0, 0) for no match.
    /// Mirrors Warp's FindWithinBlock highlight pass.
    pub fn find_in_blocks(&self, query: &str, use_regex: bool) -> (usize, usize) {
        self.clear_find();
        if query.is_empty() {
            return (0, 0);
        }
        let pattern = if use_regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        let re = match regex::RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(re) => re,
            Err(_) => return (0, 0),
        };

        // Compile the same pattern for VTE (PCRE2) so its native highlighter
        // paints every hit and its search cursor can step within each block.
        let vte_re = match vte4::Regex::for_search(
            &pattern,
            pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
        ) {
            Ok(r) => r,
            Err(_) => return (0, 0),
        };

        let mut matches: Vec<FindMatch> = Vec::new();
        {
            let finished = self.finished_blocks.borrow();
            for block in finished.iter() {
                let cmd_count = re.find_iter(&block.cmd_text).count();
                let out_count = block.with_stripped_output(|s| re.find_iter(s).count());
                if cmd_count > 0 {
                    block.command_vte.search_set_regex(Some(&vte_re), 0);
                    block.command_vte.search_set_wrap_around(true);
                    for _ in 0..cmd_count {
                        matches.push(FindMatch {
                            block_id: block.id,
                            is_output: false,
                        });
                    }
                }
                if out_count > 0 {
                    block.output_vte.search_set_regex(Some(&vte_re), 0);
                    block.output_vte.search_set_wrap_around(true);
                    for _ in 0..out_count {
                        matches.push(FindMatch {
                            block_id: block.id,
                            is_output: true,
                        });
                    }
                }
            }
        }

        if matches.is_empty() {
            return (0, 0);
        }
        let total = matches.len();
        {
            let mut st = self.find_state.borrow_mut();
            st.matches = matches;
            st.current = 0;
        }
        self.focus_current_match();
        self.scroll_to_current_match();
        (1, total)
    }

    /// Step to the next match (wrapping). Returns (current_1based, total).
    pub fn find_next(&self) -> (usize, usize) {
        self.step_find(1)
    }

    /// Step to the previous match (wrapping). Returns (current_1based, total).
    pub fn find_prev(&self) -> (usize, usize) {
        self.step_find(-1)
    }

    fn step_find(&self, delta: isize) -> (usize, usize) {
        let (cur, total) = {
            let st = self.find_state.borrow();
            (st.current, st.matches.len())
        };
        if total == 0 {
            return (0, 0);
        }
        let next = ((cur as isize + delta).rem_euclid(total as isize)) as usize;
        self.find_state.borrow_mut().current = next;
        self.focus_current_match_step(delta);
        self.scroll_to_current_match();
        (next + 1, total)
    }

    /// Move the VTE search cursor on the block backing the current match.
    /// Used after the find_state index is updated; `delta` direction tells
    /// VTE which way to step its internal cursor.
    fn focus_current_match_step(&self, delta: isize) {
        let finished = self.finished_blocks.borrow();
        let st = self.find_state.borrow();
        let Some(fm) = st.matches.get(st.current) else {
            return;
        };
        let Some(block) = finished.iter().find(|b| b.id == fm.block_id) else {
            return;
        };
        let vte = if fm.is_output {
            &block.output_vte
        } else {
            &block.command_vte
        };
        if delta >= 0 {
            vte.search_find_next();
        } else {
            vte.search_find_previous();
        }
    }

    /// Move VTE's search cursor to the very first match of the current pass.
    fn focus_current_match(&self) {
        let finished = self.finished_blocks.borrow();
        let st = self.find_state.borrow();
        let Some(fm) = st.matches.get(st.current) else {
            return;
        };
        let Some(block) = finished.iter().find(|b| b.id == fm.block_id) else {
            return;
        };
        let vte = if fm.is_output {
            &block.output_vte
        } else {
            &block.command_vte
        };
        vte.search_find_next();
    }

    fn scroll_to_current_match(&self) {
        let finished = self.finished_blocks.borrow();
        let st = self.find_state.borrow();
        let Some(fm) = st.matches.get(st.current) else {
            return;
        };
        let Some(block) = finished.iter().find(|b| b.id == fm.block_id) else {
            return;
        };
        let widget = block.widget().clone();
        let scroll = self.block_scroll.clone();
        glib::idle_add_local_once(move || {
            if let Some(point) =
                widget.compute_point(&scroll, &gtk4::graphene::Point::new(0.0, 0.0))
            {
                let adj = scroll.vadjustment();
                let target = (point.y() as f64) - adj.page_size() / 3.0;
                adj.set_value(target.max(0.0));
            }
        });
    }

    /// Cross-block ripgrep-style flat-result scan over cached stripped output
    /// + command text. Caller passes a literal substring (case-insensitive)
    /// when `is_regex == false`, else a regex. Returns at most `max_hits`
    /// hits in block-list order; each hit carries enough context (line
    /// number + the raw line + cmd preview) to drive a palette UI that lets
    /// the user pick one and jump to it.
    ///
    /// Errors only on invalid regex; an empty pattern returns `Ok(vec![])`
    /// so the caller can clear results without a special branch.
    pub fn cross_block_search(
        &self,
        pattern: &str,
        is_regex: bool,
        max_hits: usize,
    ) -> Result<Vec<CrossBlockHit>, String> {
        if pattern.is_empty() {
            return Ok(Vec::new());
        }
        let compiled_pattern = if is_regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let re = regex::RegexBuilder::new(&compiled_pattern)
            .case_insensitive(true)
            .multi_line(true)
            .build()
            .map_err(|e| format!("{e}"))?;

        let finished = self.finished_blocks.borrow();
        let mut hits: Vec<CrossBlockHit> = Vec::new();

        for block in finished.iter() {
            if hits.len() >= max_hits {
                break;
            }
            let cmd_preview = block
                .cmd_text
                .lines()
                .next()
                .unwrap_or(&block.cmd_text)
                .to_string();

            // Cmd surface — usually 1 line, but multiline commands exist.
            for (ln_idx, line) in block.cmd_text.lines().enumerate() {
                if hits.len() >= max_hits {
                    break;
                }
                if re.is_match(line) {
                    hits.push(CrossBlockHit {
                        block_id: block.id,
                        is_output: false,
                        line_no: ln_idx + 1,
                        line_text: snippet(line),
                        cmd_preview: cmd_preview.clone(),
                    });
                }
            }

            // Output surface — uses the cached ANSI-stripped view.
            block.with_stripped_output(|s| {
                for (ln_idx, line) in s.lines().enumerate() {
                    if hits.len() >= max_hits {
                        break;
                    }
                    if re.is_match(line) {
                        hits.push(CrossBlockHit {
                            block_id: block.id,
                            is_output: true,
                            line_no: ln_idx + 1,
                            line_text: snippet(line),
                            cmd_preview: cmd_preview.clone(),
                        });
                    }
                }
            });
        }
        Ok(hits)
    }

    /// Scroll the named block into view (by stable id, not list index).
    /// Returns `false` if the id is unknown — likely evicted by the
    /// `max_blocks` cap or deleted via the per-block menu.
    pub fn scroll_to_block_id(&self, block_id: u64) -> bool {
        let finished = self.finished_blocks.borrow();
        let Some(block) = finished.iter().find(|b| b.id == block_id) else {
            return false;
        };
        select_finished_block(&finished, &self.selected_block_id, Some(block_id));
        block.widget().grab_focus();
        let adj = self.block_scroll.vadjustment();
        if let Some(value) = block
            .widget()
            .compute_point(&self.block_scroll, &gtk4::graphene::Point::new(0.0, 0.0))
        {
            adj.set_value(value.y() as f64);
        }
        true
    }

    /// Light up the chosen block's command/output VTE with a PCRE2 search
    /// for `pattern` and advance its internal search cursor to the first
    /// hit. Other blocks keep whatever highlight state they had — this is
    /// the "jump to this hit" companion for `cross_block_search`. Returns
    /// `false` when the id is unknown or the pattern can't compile.
    pub fn focus_match_in_block(
        &self,
        block_id: u64,
        pattern: &str,
        is_regex: bool,
        is_output: bool,
    ) -> bool {
        if pattern.is_empty() {
            return false;
        }
        let compiled = if is_regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let Ok(vte_re) = vte4::Regex::for_search(
            &compiled,
            pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
        ) else {
            return false;
        };
        let finished = self.finished_blocks.borrow();
        let Some(block) = finished.iter().find(|b| b.id == block_id) else {
            return false;
        };
        let vte = if is_output {
            &block.output_vte
        } else {
            &block.command_vte
        };
        vte.search_set_regex(Some(&vte_re), 0);
        vte.search_set_wrap_around(true);
        vte.search_find_next();
        true
    }

    /// Remove all find highlights and reset the find cursor (call on close).
    pub fn clear_find(&self) {
        {
            let finished = self.finished_blocks.borrow();
            for block in finished.iter() {
                block.command_vte.search_set_regex(None::<&vte4::Regex>, 0);
                block.output_vte.search_set_regex(None::<&vte4::Regex>, 0);
            }
        }
        let mut st = self.find_state.borrow_mut();
        st.matches.clear();
        st.current = 0;
    }

    /// Get only failed blocks (exit_code != 0)
    pub fn get_failed_blocks(&self) -> Vec<usize> {
        let filters = BlockFilters {
            failed_only: true,
            ..Default::default()
        };
        self.search_blocks_with_filters("", &filters)
    }

    /// Get only slow blocks (duration > threshold)
    pub fn get_slow_blocks(&self, threshold_ms: u64) -> Vec<usize> {
        let filters = BlockFilters {
            slow_only: true,
            slow_threshold_ms: threshold_ms,
            ..Default::default()
        };
        self.search_blocks_with_filters("", &filters)
    }
}
