//! alt — VTE builder + small parser helpers for the live terminal.
//!
//! jterm4 aligns with Warp's alt-screen model: when an alt-screen app
//! (top/vim/htop/...) sends `?1049h`, the live VTE switches to its alt buffer
//! and renders full-viewport; when it sends `?1049l`, the alt-screen content
//! is **discarded** — the active block keeps only the command name + exit code.
//! No frame-merge / pager-snapshot path runs, matching Warp.
use crate::config::Config;
use gtk4::gdk::RGBA;
use gtk4::pango::FontDescription;
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

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
}

// ─── VTE builder ─────────────────────────────────────────────────────────────

/// Apply colors + font + font scale from `config` onto an existing Terminal.
/// Single source of truth for VTE theming so the live VTE and read-only
/// finished-block VTEs stay visually identical.
pub(crate) fn apply_theme_to_vte(terminal: &Terminal, config: &Config) {
    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(
        Some(&config.foreground),
        Some(&config.background),
        &palette_refs,
    );
    terminal.set_color_bold(None);
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));
    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));
    terminal.set_font_scale(config.default_font_scale);
}

/// The single persistent live VTE for block mode. It keeps `input_enabled(true)`
/// so the VTE translates keypresses into terminal byte sequences and emits them
/// via its `commit` signal (which we forward to our PTY). It also owns IME
/// natively, so there is no separate IMMulticontext to fight for fcitx/ibus focus.
pub(crate) fn create_active_terminal(config: &Config) -> Terminal {
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        .cursor_blink_mode(CursorBlinkMode::On)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();
    terminal.set_mouse_autohide(true);
    // Backspace must emit ASCII DEL (0x7f), not BS (0x08). Our PTY isn't VTE-owned,
    // so VTE's Auto binding can't read the tty erase char and falls back to 0x08,
    // which readline-style line editors (incl. rsh) ignore — making Backspace dead.
    terminal.set_backspace_binding(vte4::EraseBinding::AsciiDelete);
    apply_theme_to_vte(&terminal, config);
    terminal
}

/// A read-only PTY-less VTE used as the renderer for a single finished block.
/// Input is disabled; cursor is hidden (block widget shows completed output, not
/// a live prompt). `output_rows` sizes the widget to exactly the captured row
/// count up to `viewport_cap`; anything beyond goes into the widget's own
/// scrollback so the user can scroll within a long block (e.g. `git log`).
pub(crate) fn create_finished_terminal(
    config: &Config,
    cols: i64,
    output_rows: i64,
    viewport_cap: i64,
) -> Terminal {
    let visible_rows = output_rows.min(viewport_cap).max(1);
    let scrollback = output_rows.max(visible_rows) as u32;
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(false)
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(false)
        .scrollback_lines(scrollback)
        .cursor_blink_mode(CursorBlinkMode::Off)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        // Finished blocks are fed once; the view should anchor at the TOP of
        // the captured output so the user reads it head-down (e.g. `git log`'s
        // first `commit <hash>` line stays visible). With the VTE default
        // (`scroll-on-output = true`) the post-feed cursor at end snaps the
        // view to the bottom, hiding the first rows in scrollback whenever
        // output_rows > viewport_cap — which is exactly the wide-terminal
        // case where there's room to show them.
        .scroll_on_output(false)
        .scroll_on_keystroke(false)
        .build();
    terminal.set_mouse_autohide(true);
    apply_theme_to_vte(&terminal, config);
    // Hide the read-only block's cursor — the completed output should not show a
    // blinking caret at the end of the last line.
    let mut transparent = config.background;
    transparent.set_alpha(0.0);
    terminal.set_color_cursor(Some(&transparent));
    terminal.set_size(cols.max(1), visible_rows);
    // URL detection — mirror the live-VTE pattern at src/terminal.rs:52-56.
    if let Ok(regex) = vte4::Regex::for_match(
        r"[a-z]+://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    ) {
        terminal.match_add_regex(&regex, 0);
    }
    terminal
}
