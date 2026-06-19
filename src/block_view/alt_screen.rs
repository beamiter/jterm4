//! alt — VTE builder + small parser helpers for the live terminal.
//!
//! jterm4 aligns with Warp's alt-screen model: when an alt-screen app
//! (top/vim/htop/...) sends `?1049h`, the live VTE switches to its alt buffer
//! and renders full-viewport; when it sends `?1049l`, the alt-screen content
//! is **discarded** — the active block keeps only the command name + exit code.
//! No frame-merge / pager-snapshot path runs, matching Warp.
use gtk4::gdk::RGBA;
use gtk4::pango::FontDescription;
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};
use crate::config::Config;

// ─── Mouse Reporting Mode ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Default)]
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

/// True if the byte stream contains a *real* bell (BEL, 0x07) — i.e. one that is
/// NOT acting as the string terminator of an OSC sequence (`ESC ] … BEL`). A
/// naive `bytes.contains(&7)` fires spuriously on every OSC 0/2 title update that
/// uses the BEL terminator form.
pub(crate) fn contains_bell(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            // OSC: skip to its terminator (BEL or ESC \), consuming the BEL.
            i += 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    i += 1;
                    break;
                }
                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if bytes[i] == 0x07 {
            return true;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod contains_bell_tests {
    use super::contains_bell;

    #[test]
    fn detects_real_bell() {
        assert!(contains_bell(b"abc\x07def"));
        assert!(contains_bell(b"\x07"));
    }

    #[test]
    fn no_bell_in_plain_text() {
        assert!(!contains_bell(b"hello world"));
        assert!(!contains_bell(b""));
    }

    #[test]
    fn ignores_bel_terminating_osc() {
        // OSC title set: ESC ] 0 ; title BEL — the trailing BEL is a string
        // terminator, not an audible bell, so it must not count.
        assert!(!contains_bell(b"\x1b]0;my title\x07"));
        assert!(!contains_bell(b"before\x1b]0;t\x07after"));
    }

    #[test]
    fn osc_terminated_by_st_then_real_bell() {
        // OSC closed with ESC \ (ST), followed by a genuine bell afterwards.
        assert!(contains_bell(b"\x1b]0;t\x1b\\\x07"));
    }

    #[test]
    fn real_bell_before_osc() {
        assert!(contains_bell(b"\x07\x1b]0;t\x07"));
    }
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
    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(
        Some(&config.foreground),
        Some(&config.background),
        &palette_refs,
    );
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));
    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));
    terminal
}
