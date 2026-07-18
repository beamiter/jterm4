//! alt — VTE builder + small parser helpers for the live terminal.
//!
//! jterm4 aligns with Warp's alt-screen model: when an alt-screen app
//! (top/vim/htop/...) sends `?47h`, `?1047h`, or `?1049h`, the live VTE receives
//! that exact DEC mode and renders full-viewport. The matching leave sequence
//! discards the alt-screen frame; the block keeps only command metadata.
//! No frame-merge / pager-snapshot path runs, matching Warp.
use crate::config::Config;
use crate::terminal::apply_terminal_theme;
use gtk4::glib;
use gtk4::prelude::*;
use vte4::TerminalExt;
use vte4::{CursorBlinkMode, CursorShape, Format, Terminal};

/// Give dense block output the same breathing room as jterm1. Patched
/// monospace fonts often paint close to VTE's default cell boundary.
pub(crate) const BLOCK_CELL_HEIGHT_SCALE: f64 = 1.12;

// ─── Mouse Reporting Mode ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum MouseReportingMode {
    /// No mouse reporting (CSI ?1000l, etc.)
    #[default]
    None,
    /// Basic click reporting (CSI ?1000h)
    Click,
    /// Button press/release/drag (CSI ?1002h)
    Button,
    /// All mouse motion (CSI ?1003h)
    Motion,
    /// SGR-style reporting (CSI ?1006h) - modern format
    Sgr,
}

/// Encode a wheel-scroll event as a mouse-reporting byte sequence appropriate
/// for `mode`. Returns `None` if the mode has no wheel reporting (e.g. `None`,
/// or a mode where wheel deltas don't translate).
///
/// `delta_y` follows the GTK convention (negative = up, positive = down).
/// `col` / `row` are 1-based cell coordinates under the pointer; if you don't
/// have them, pass 1/1 — pagers (less/vim) look at the button code, not the
/// coordinate.
///
/// VTE 4 normally encodes wheel events itself, but only when it owns the PTY;
/// jterm4's live VTE is fed by our own reader so we synthesize the bytes here.
pub(crate) fn encode_mouse_wheel(
    mode: MouseReportingMode,
    delta_y: f64,
    col: i64,
    row: i64,
) -> Option<Vec<u8>> {
    if delta_y == 0.0 {
        return None;
    }
    // Buttons per xterm: 64 = wheel up, 65 = wheel down.
    let button: u32 = if delta_y < 0.0 { 64 } else { 65 };
    let c = col.max(1);
    let r = row.max(1);
    match mode {
        MouseReportingMode::None => None,
        MouseReportingMode::Sgr => Some(format!("\x1b[<{};{};{}M", button, c, r).into_bytes()),
        // X10-style modes encode each field as `value + 32` in a single byte.
        // Wheel reporting requires at least Button-event tracking (1002), but
        // xterm's de-facto behavior also forwards wheel under plain Click
        // (1000), so we emit for any non-None, non-SGR mode.
        MouseReportingMode::Click | MouseReportingMode::Button | MouseReportingMode::Motion => {
            // Clamp to the legacy 223-column limit (255 - 32).
            let cb = (button + 32).min(255) as u8;
            let cc = (c as u32 + 32).min(255) as u8;
            let cr = (r as u32 + 32).min(255) as u8;
            Some(vec![0x1b, b'[', b'M', cb, cc, cr])
        }
    }
}

/// Convert VTE's adjustment extent into the number of rows retained by a
/// finished snapshot. Negative `lower` represents wrapped/overflow rows in
/// scrollback; `upper - lower` covers both that scrollback and the visible grid.
fn finished_buffer_rows_from_adjustment(lower: f64, upper: f64, visible_rows: i64) -> i64 {
    let visible_rows = visible_rows.max(1);
    if !lower.is_finite() || !upper.is_finite() || upper <= lower {
        return visible_rows;
    }
    let span = (upper - lower).ceil();
    if span >= i64::MAX as f64 {
        i64::MAX
    } else {
        (span as i64).max(visible_rows)
    }
}

/// Count visual rows in VTE's plain-text buffer export.
///
/// VTE preserves hard line breaks in `text_format`, but joins soft-wrapped rows
/// back into one logical line. Re-apply the terminal width so a wrapped last
/// line cannot be clipped when the provisional grid is resized.
fn rendered_text_rows(text: &str, cols: i64) -> i64 {
    use unicode_width::UnicodeWidthChar;

    let text = text.trim_end_matches(['\n', '\r', ' ', '\t', '\0']);
    if text.is_empty() {
        return 0;
    }

    let cols = cols.max(1) as usize;
    text.split('\n')
        .map(|line| {
            let mut width = 0usize;
            for ch in line.trim_end_matches('\r').chars() {
                width += match ch {
                    '\t' => 8 - (width % 8),
                    _ => UnicodeWidthChar::width(ch).unwrap_or(0),
                };
            }
            width.max(1).div_ceil(cols) as i64
        })
        .sum()
}

/// Resolve actual occupied rows from VTE's rendered text and final cursor.
///
/// Unlike the old adjustment-only calculation, this may shrink below
/// `visible_rows`: the provisional grid itself contributes blank rows to the
/// adjustment extent, so treating that extent as content made every
/// overestimate permanent.
fn finished_content_rows(
    rendered_text: Option<&str>,
    cols: i64,
    cursor_row: i64,
    lower: f64,
    upper: f64,
    visible_rows: i64,
) -> i64 {
    let fallback = finished_buffer_rows_from_adjustment(lower, upper, visible_rows);
    let lower_row = if lower.is_finite() {
        lower.floor().clamp(i64::MIN as f64, i64::MAX as f64) as i64
    } else {
        0
    };
    let cursor_rows = cursor_row
        .saturating_sub(lower_row)
        .saturating_add(1)
        .max(1);
    let text_rows = rendered_text
        .map(|text| rendered_text_rows(text, cols))
        .unwrap_or(fallback);
    cursor_rows.max(text_rows).clamp(1, fallback.max(1))
}

/// Fit a read-only finished VTE to the rows it actually rendered.
///
/// Measuring VTE's text/cursor state covers ANSI cursor movement, erases, tabs,
/// combining/wide glyphs, CR redraws, and automatic wrapping. The resize is
/// intentionally bidirectional: estimates are only capture capacity, never a
/// minimum visible height.
pub(crate) fn fit_finished_terminal_to_content(terminal: &Terminal) {
    let visible_rows = terminal.row_count().max(1);
    let cols = terminal.column_count().max(1);
    let (_, cursor_row) = terminal.cursor_position();
    let rendered = terminal
        .text_format(Format::Text)
        .map(|text| text.to_string());
    let rows = terminal
        .vadjustment()
        .map(|adj| {
            finished_content_rows(
                rendered.as_deref(),
                cols,
                cursor_row,
                adj.lower(),
                adj.upper(),
                visible_rows,
            )
        })
        .unwrap_or_else(|| {
            rendered
                .as_deref()
                .map(|text| rendered_text_rows(text, cols).max(1))
                .unwrap_or(visible_rows)
        });

    if rows != visible_rows {
        terminal.set_size(cols, rows);
    }
    let cell_height = (terminal.char_height() as i32).max(1);
    let rows_i32 = rows.clamp(1, i32::MAX as i64) as i32;
    terminal.set_height_request(rows_i32.saturating_mul(cell_height));
    if let Some(adj) = terminal.vadjustment() {
        adj.set_value(adj.lower());
    }
}

/// VTE updates its buffer asynchronously after `feed()`. Two idle passes fold
/// all retained rows into the finished card, then a final pass pins the snapshot
/// to its first row.
pub(crate) fn settle_finished_terminal_after_feed(terminal: &Terminal) {
    let terminal = terminal.clone();
    glib::idle_add_local_once(move || {
        fit_finished_terminal_to_content(&terminal);
        let terminal = terminal.clone();
        glib::idle_add_local_once(move || {
            fit_finished_terminal_to_content(&terminal);
            let terminal = terminal.clone();
            glib::idle_add_local_once(move || {
                if let Some(adj) = terminal.vadjustment() {
                    adj.set_value(adj.lower());
                }
            });
        });
    });
}

/// Keep a deliberately capped snapshot anchored at its first retained row.
/// VTE applies `feed()` asynchronously, so one immediate adjustment write can
/// be overwritten by the final buffer layout. Reassert the top over two idles.
pub(crate) fn settle_finished_terminal_at_top(terminal: &Terminal) {
    let terminal = terminal.clone();
    glib::idle_add_local_once(move || {
        if let Some(adj) = terminal.vadjustment() {
            adj.set_value(adj.lower());
        }
        let terminal = terminal.clone();
        glib::idle_add_local_once(move || {
            if let Some(adj) = terminal.vadjustment() {
                adj.set_value(adj.lower());
            }
        });
    });
}

// ─── VTE builder ─────────────────────────────────────────────────────────────

/// The single persistent live VTE for block mode. It keeps `input_enabled(true)`
/// so the VTE translates keypresses into terminal byte sequences and emits them
/// via its `commit` signal (which we forward to our PTY). It also owns IME
/// natively, so there is no separate IMMulticontext to fight for fcitx/ibus focus.
pub(crate) fn create_active_terminal(config: &Config) -> Terminal {
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .name("term_name")
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        // Keep the live surface behavior-identical to the regular VTE mode.
        // In particular, respect the desktop cursor-blink preference instead
        // of forcing a different policy only in block mode.
        .cursor_blink_mode(CursorBlinkMode::System)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .cell_height_scale(BLOCK_CELL_HEIGHT_SCALE)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();
    terminal.set_mouse_autohide(true);
    // Backspace must emit ASCII DEL (0x7f), not BS (0x08). Our PTY isn't VTE-owned,
    // so VTE's Auto binding can't read the tty erase char and falls back to 0x08,
    // which readline-style line editors (incl. rsh) ignore — making Backspace dead.
    terminal.set_backspace_binding(vte4::EraseBinding::AsciiDelete);
    apply_terminal_theme(&terminal, config);
    // Match the regular VTE surface: links are detectable while the command is
    // still running, not only after its output becomes a finished block.
    if let Ok(regex) = vte4::Regex::for_match(
        r"[a-z]+://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    ) {
        terminal.match_add_regex(&regex, 0);
    }
    terminal
}

/// Apply the common terminal theme to a snapshot VTE, then restore its hidden
/// cursor. Keeping this separate avoids theme changes turning a finished block's
/// inert caret visible again.
pub(crate) fn apply_snapshot_theme_to_vte(terminal: &Terminal, config: &Config) {
    apply_terminal_theme(terminal, config);
    let mut transparent = config.background;
    transparent.set_alpha(0.0);
    terminal.set_color_cursor(Some(&transparent));
}

/// A read-only PTY-less VTE used as the renderer for a single finished block.
/// Input is disabled; cursor is hidden (block widget shows completed output, not
/// a live prompt). Most completed outputs expand to their full retained buffer
/// and participate in the outer block history. Only exceptionally large outputs
/// stay capped at `viewport_cap` and keep private scrollback as a safety valve.
pub(crate) fn create_finished_terminal(
    config: &Config,
    cols: i64,
    output_rows: i64,
    viewport_cap: i64,
    expand_to_buffer: bool,
) -> Terminal {
    let visible_rows = output_rows.min(viewport_cap).max(1);
    // Estimates can be low for cursor movement, CR redraws, tabs, wide glyphs,
    // and wrapping. Keep temporary capture capacity until the post-feed settle
    // pass folds VTE's actual buffer rows into the card.
    let capture_rows = output_rows
        .max(viewport_cap)
        .max(config.truncation_threshold_lines as i64)
        .max(4096)
        .clamp(1, u32::MAX as i64) as u32;
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(false)
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(false)
        .scrollback_lines(capture_rows)
        .cursor_blink_mode(CursorBlinkMode::Off)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .cell_height_scale(BLOCK_CELL_HEIGHT_SCALE)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .scroll_on_output(false)
        .scroll_on_keystroke(false)
        .build();
    terminal.set_mouse_autohide(true);
    apply_snapshot_theme_to_vte(&terminal, config);
    terminal.set_size(cols.max(1), visible_rows);

    if expand_to_buffer {
        let expanded = std::cell::Cell::new(false);
        terminal.connect_map(move |terminal| {
            if expanded.replace(true) {
                return;
            }
            settle_finished_terminal_after_feed(terminal);
        });
    }

    if let Ok(regex) = vte4::Regex::for_match(
        r"[a-z]+://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    ) {
        terminal.match_add_regex(&regex, 0);
    }
    terminal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_wheel_up_encodes_button_64() {
        // delta_y < 0 → wheel up → button 64 (xterm convention).
        let seq = encode_mouse_wheel(MouseReportingMode::Sgr, -1.0, 10, 5).unwrap();
        assert_eq!(seq, b"\x1b[<64;10;5M");
    }

    #[test]
    fn sgr_wheel_down_encodes_button_65() {
        let seq = encode_mouse_wheel(MouseReportingMode::Sgr, 1.0, 1, 1).unwrap();
        assert_eq!(seq, b"\x1b[<65;1;1M");
    }

    #[test]
    fn x10_wheel_up_uses_value_plus_32() {
        // Legacy mode: each field encoded as byte = value + 32.
        let seq = encode_mouse_wheel(MouseReportingMode::Button, -1.0, 1, 1).unwrap();
        assert_eq!(seq, vec![0x1b, b'[', b'M', 64 + 32, 1 + 32, 1 + 32]);
    }

    #[test]
    fn none_mode_returns_no_bytes() {
        assert!(encode_mouse_wheel(MouseReportingMode::None, -1.0, 1, 1).is_none());
    }

    #[test]
    fn zero_delta_returns_no_bytes() {
        // Spurious 0 delta from GTK shouldn't paginate the app.
        assert!(encode_mouse_wheel(MouseReportingMode::Sgr, 0.0, 1, 1).is_none());
    }

    #[test]
    fn finished_buffer_rows_include_wrapped_scrollback() {
        assert_eq!(finished_buffer_rows_from_adjustment(-7.0, 5.0, 5), 12);
    }

    #[test]
    fn finished_buffer_rows_never_shrink_the_provisional_grid() {
        assert_eq!(finished_buffer_rows_from_adjustment(0.0, 1.0, 5), 5);
        assert_eq!(finished_buffer_rows_from_adjustment(f64::NAN, 1.0, 3), 3);
    }

    #[test]
    fn measured_content_can_shrink_an_overestimated_grid() {
        assert_eq!(
            finished_content_rows(Some("alpha\nbeta\n\n\n"), 80, 1, 0.0, 60.0, 60,),
            2
        );
        assert_eq!(finished_content_rows(Some(""), 80, 0, 0.0, 60.0, 60), 1);
    }

    #[test]
    fn measured_content_keeps_rows_below_an_upward_moved_cursor() {
        assert_eq!(
            finished_content_rows(Some("top\nmiddle\nbottom"), 80, 0, 0.0, 40.0, 40,),
            3
        );
    }

    #[test]
    fn measured_content_reapplies_soft_wrapping() {
        assert_eq!(rendered_text_rows("123456789", 4), 3);
        assert_eq!(rendered_text_rows("12345678", 4), 2);
        assert_eq!(rendered_text_rows("界界界", 4), 2);
        assert_eq!(rendered_text_rows("\tX", 4), 3);
    }

    #[test]
    fn measured_content_never_exceeds_retained_vte_buffer() {
        assert_eq!(
            finished_content_rows(Some("alpha"), 80, 999, 0.0, 60.0, 60),
            60
        );
    }

    #[test]
    fn cursor_preserves_intentional_trailing_blank_rows() {
        assert_eq!(
            finished_content_rows(Some("alpha\n\n"), 80, 2, 0.0, 20.0, 20),
            3
        );
    }
}
