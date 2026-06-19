/// Events emitted by the stream parser.
#[derive(Debug, Clone)]
pub enum ParserEvent {
    /// Raw bytes that should be displayed verbatim (ANSI codes stripped of OSC 133/7).
    Bytes(Vec<u8>),
    /// OSC 133 ;A — prompt about to render.
    PromptStart,
    /// OSC 133 ;B — prompt finished, waiting for user input.
    PromptEnd,
    /// OSC 133 ;C — user pressed Enter, command is executing.
    CommandStart,
    /// OSC 133 ;D;<code> — command finished with exit code.
    CommandEnd(i32),
    /// OSC 7 — shell reported new CWD.
    CwdUpdate(String),
    /// OSC 0 / OSC 2 — window title set by the application.
    TitleUpdate(String),
    /// CSI ? 1049 h — alt screen entered (vim, less, etc.)
    AltScreenEnter,
    /// CSI ? 1049 l — alt screen left.
    AltScreenLeave,
    /// OSC 52 — application set clipboard content.
    ClipboardSet(String),
    /// APC sequence (ESC _) — Kitty graphics protocol or similar.
    ApcSequence(Vec<u8>),
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    /// Saw ESC, waiting for next byte
    Esc,
    /// Inside CSI (ESC [): collecting parameter/intermediary bytes
    Csi { buf: Vec<u8> },
    /// Inside OSC (ESC ]): collecting bytes until ST (BEL or ESC \)
    Osc { buf: Vec<u8> },
    /// Just saw ESC while in OSC — next byte should be '\' for ST
    OscEsc { payload: Vec<u8> },
    /// Inside APC (ESC _): collecting bytes for Kitty graphics etc.
    Apc { buf: Vec<u8> },
    /// Saw ESC while in APC — next byte should be '\' for ST
    ApcEsc { payload: Vec<u8> },
    /// Inside DCS/PM — just consume until ST
    Ignore,
}

pub struct Parser {
    state: State,
    passthrough: Vec<u8>,
    config: ParserConfig,
    /// True while we have seen `CSI ?25 l` (cursor hidden) without a matching
    /// `CSI ?25 h`. Cleared on CommandStart (OSC 133 ;C) and on real ?1049h/l.
    cursor_hidden: bool,
    /// True if any printable byte has arrived since cursor was hidden. Gates
    /// the `[?25l → [2J` heuristic to consecutive control sequences only.
    printable_since_hide: bool,
    /// True once this command has been promoted to alt-screen (via the
    /// heuristic OR via a real ?1049h). Suppresses duplicate AltScreenEnter
    /// from a TUI that redraws via `[2J` every frame. Reset on CommandStart
    /// and on ?1049l.
    tui_promoted: bool,
    /// True after `CSI ?1 h` (DECCKM, application cursor keys). Together
    /// with `app_keypad` this is the standard "smkx" combo emitted by
    /// curses-like TUIs (vim, less, top, htop, mc, fzf). Cleared on
    /// `CSI ?1 l` and on CommandStart.
    app_cursor_keys: bool,
    /// True after `ESC =` (DECPAM, application keypad). Cleared on `ESC >`
    /// and on CommandStart.
    app_keypad: bool,
}

/// Runtime toggles for selectively swallowing reporting-enable sequences before
/// they reach the downstream consumer (VTE). When a toggle is `false`, the
/// matching `CSI ?…h`/`CSI ?…l` sequences are dropped from the byte stream so
/// the terminal never enters that reporting mode and apps never receive the
/// corresponding events.
#[derive(Clone, Copy)]
pub struct ParserConfig {
    pub mouse_reporting: bool,
    pub focus_reporting: bool,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self { mouse_reporting: true, focus_reporting: true }
    }
}

fn is_alt_screen_mode(params: &[u8]) -> bool {
    matches!(params, b"?47" | b"?1047" | b"?1049")
}

fn is_mouse_reporting_mode(params: &[u8]) -> bool {
    matches!(
        params,
        b"?9" | b"?1000" | b"?1001" | b"?1002" | b"?1003"
            | b"?1005" | b"?1006" | b"?1015" | b"?1016"
    )
}

fn is_focus_reporting_mode(params: &[u8]) -> bool {
    matches!(params, b"?1004")
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self::with_config(ParserConfig::default())
    }

    pub fn with_config(config: ParserConfig) -> Self {
        Parser {
            state: State::default(),
            passthrough: Vec::with_capacity(4096),
            config,
            cursor_hidden: false,
            printable_since_hide: false,
            tui_promoted: false,
            app_cursor_keys: false,
            app_keypad: false,
        }
    }

    /// Promote to alt-screen if smkx (app cursor keys + app keypad) just
    /// completed. Called after either flag is set; emits at most one
    /// synthetic AltScreenEnter per command. The caller is responsible for
    /// flushing prior passthrough first.
    fn maybe_promote_smkx(&mut self, events: &mut Vec<ParserEvent>) {
        if self.app_cursor_keys && self.app_keypad && !self.tui_promoted {
            // Order matters: passthrough may already contain the just-pushed
            // sequence bytes. Emit any bytes first so they reach VTE on the
            // main screen (harmless: smkx affects key encoding, not display),
            // then emit AltScreenEnter so block_view feeds [?1049h to VTE.
            if !self.passthrough.is_empty() {
                events.push(ParserEvent::Bytes(std::mem::take(&mut self.passthrough)));
            }
            events.push(ParserEvent::AltScreenEnter);
            self.tui_promoted = true;
        }
    }

    /// If `handle_osc` emitted a CommandStart event, reset the TUI-detection
    /// state so the next command starts fresh (the same parser instance lives
    /// for the entire shell session).
    fn maybe_reset_tui_state(&mut self, events: &[ParserEvent], pre_len: usize) {
        for ev in &events[pre_len..] {
            if matches!(ev, ParserEvent::CommandStart) {
                self.cursor_hidden = false;
                self.printable_since_hide = false;
                self.tui_promoted = false;
                self.app_cursor_keys = false;
                self.app_keypad = false;
            }
        }
    }

    pub fn feed(&mut self, data: &[u8], events: &mut Vec<ParserEvent>) {
        self.passthrough.clear();

        macro_rules! flush {
            () => {
                if !self.passthrough.is_empty() {
                    events.push(ParserEvent::Bytes(std::mem::take(&mut self.passthrough)));
                }
            };
        }

        for &b in data {
            match &mut self.state {
                State::Ground => match b {
                    0x1b => {
                        self.state = State::Esc;
                    }
                    _ => {
                        // While the cursor is hidden via [?25l, any printable
                        // byte (or whitespace) disqualifies a later [2J from
                        // being treated as a TUI signature — that's a spinner
                        // or similar, not a full-screen redraw.
                        if self.cursor_hidden && !self.printable_since_hide {
                            let printable = b == b'\t' || b == b'\n' || b == b'\r'
                                || (b >= 0x20 && b != 0x7f);
                            if printable {
                                self.printable_since_hide = true;
                            }
                        }
                        self.passthrough.push(b);
                    }
                },

                State::Esc => match b {
                    b'[' => {
                        // Do NOT emit "ESC[" yet. Buffer the whole CSI in state so a
                        // read boundary falling mid-sequence cannot split it across
                        // two Bytes events — downstream scanners (interactive-mode
                        // detection) rely on seeing each CSI whole.
                        self.state = State::Csi { buf: Vec::new() };
                    }
                    b']' => {
                        self.state = State::Osc { buf: Vec::new() };
                    }
                    b'_' => {
                        self.state = State::Apc { buf: Vec::new() };
                    }
                    b'P' | b'^' => {
                        self.state = State::Ignore;
                    }
                    b'=' => {
                        // DECPAM — application keypad mode. Half of the smkx
                        // pair; promote when the cursor-keys half also seen.
                        self.passthrough.push(0x1b);
                        self.passthrough.push(b);
                        self.state = State::Ground;
                        self.app_keypad = true;
                        self.maybe_promote_smkx(events);
                    }
                    b'>' => {
                        // DECPNM — normal keypad. Disarms the smkx half.
                        self.passthrough.push(0x1b);
                        self.passthrough.push(b);
                        self.state = State::Ground;
                        self.app_keypad = false;
                    }
                    _ => {
                        self.passthrough.push(0x1b);
                        self.passthrough.push(b);
                        self.state = State::Ground;
                    }
                },

                State::Csi { buf } => {
                    if (0x40..=0x7e).contains(&b) {
                        // Final byte of CSI sequence
                        let params = std::mem::take(buf);
                        self.state = State::Ground;
                        if b == b'h' && is_alt_screen_mode(&params) {
                            // Recognized alt-screen enter: drop the sequence bytes
                            // (never passed through) and emit the semantic event.
                            flush!();
                            events.push(ParserEvent::AltScreenEnter);
                            // Real alt-screen takes precedence: prevent the
                            // [?25l→[2J or smkx heuristics from re-firing
                            // inside this app.
                            self.tui_promoted = true;
                            self.cursor_hidden = false;
                            self.printable_since_hide = false;
                            self.app_cursor_keys = false;
                            self.app_keypad = false;
                        } else if b == b'l' && is_alt_screen_mode(&params) {
                            flush!();
                            events.push(ParserEvent::AltScreenLeave);
                            self.tui_promoted = false;
                            self.app_cursor_keys = false;
                            self.app_keypad = false;
                        } else if b == b'h' && params == b"?1" {
                            // DECCKM — application cursor keys (smkx half).
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                            self.passthrough.push(b);
                            self.app_cursor_keys = true;
                            self.maybe_promote_smkx(events);
                        } else if b == b'l' && params == b"?1" {
                            // DECCKM off — disarms the smkx half.
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                            self.passthrough.push(b);
                            self.app_cursor_keys = false;
                        } else if b == b'l' && params == b"?25" {
                            // Hide cursor — arm the main-screen-TUI heuristic.
                            // Pass through unchanged so VTE still hides cursor.
                            self.cursor_hidden = true;
                            self.printable_since_hide = false;
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                            self.passthrough.push(b);
                        } else if b == b'h' && params == b"?25" {
                            // Show cursor — disarm.
                            self.cursor_hidden = false;
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                            self.passthrough.push(b);
                        } else if b == b'J'
                            && (params.is_empty() || params == b"2")
                            && self.cursor_hidden
                            && !self.printable_since_hide
                            && !self.tui_promoted
                        {
                            // Main-screen TUI signature: cursor was hidden
                            // and no printable bytes since — `[2J` here is a
                            // full-screen redraw, not a shell `clear`.
                            // Promote to alt-screen so the live VTE/PTY get
                            // the full viewport.
                            //
                            // Order: flush prior bytes (they reach VTE on
                            // the *main* screen — harmless; usually just
                            // [?25l + cursor home), then emit AltScreenEnter
                            // (block_view feeds [?1049h to VTE), then push
                            // the [2J back into passthrough so it clears
                            // the alt buffer.
                            flush!();
                            events.push(ParserEvent::AltScreenEnter);
                            self.tui_promoted = true;
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                            self.passthrough.push(b);
                        } else if !self.config.mouse_reporting
                            && (b == b'h' || b == b'l')
                            && is_mouse_reporting_mode(&params)
                        {
                            // Drop: keep VTE out of mouse reporting mode.
                        } else if !self.config.focus_reporting
                            && (b == b'h' || b == b'l')
                            && is_focus_reporting_mode(&params)
                        {
                            // Drop: keep VTE out of focus reporting mode.
                        } else {
                            // Pass the complete sequence through as one contiguous run.
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                            self.passthrough.push(b);
                        }
                    } else {
                        buf.push(b);
                        // Guard against an unterminated CSI growing without bound
                        // (malformed stream). Dump what we have and recover.
                        if buf.len() > 4096 {
                            let params = std::mem::take(buf);
                            self.state = State::Ground;
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                        }
                    }
                }

                State::Osc { buf } => {
                    match b {
                        0x07 => {
                            let payload = std::mem::take(buf);
                            self.state = State::Ground;
                            flush!();
                            let pre_len = events.len();
                            handle_osc(&payload, events);
                            self.maybe_reset_tui_state(events, pre_len);
                        }
                        0x1b => {
                            let payload = std::mem::take(buf);
                            self.state = State::OscEsc { payload };
                        }
                        _ => {
                            buf.push(b);
                        }
                    }
                }

                State::OscEsc { payload } => {
                    let payload = std::mem::take(payload);
                    self.state = State::Ground;
                    flush!();
                    let pre_len = events.len();
                    handle_osc(&payload, events);
                    self.maybe_reset_tui_state(events, pre_len);
                    if b != b'\\' {
                        self.passthrough.push(b);
                    }
                }

                State::Apc { buf } => {
                    match b {
                        0x07 => {
                            let payload = std::mem::take(buf);
                            self.state = State::Ground;
                            flush!();
                            events.push(ParserEvent::ApcSequence(payload));
                        }
                        0x1b => {
                            let payload = std::mem::take(buf);
                            self.state = State::ApcEsc { payload };
                        }
                        _ => {
                            buf.push(b);
                        }
                    }
                }

                State::ApcEsc { payload } => {
                    let payload = std::mem::take(payload);
                    self.state = State::Ground;
                    if b == b'\\' {
                        flush!();
                        events.push(ParserEvent::ApcSequence(payload));
                    } else {
                        flush!();
                        events.push(ParserEvent::ApcSequence(payload));
                        self.passthrough.push(b);
                    }
                }

                State::Ignore => {
                    if b == 0x07 || b == 0x1b {
                        self.state = State::Ground;
                    }
                }
            }
        }

        flush!();
    }
}

fn handle_osc(payload: &[u8], events: &mut Vec<ParserEvent>) {
    let s = match std::str::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => return,
    };

    // OSC 133 ; <mark> [; params...] — shell integration (FTCS).
    // Real shells emit extra `;`-separated params, e.g. "A;cl=m;k=i" or
    // "D;1;aid=7", so match only the leading mark field and treat the rest
    // as parameters.
    if let Some(rest) = s.strip_prefix("133;") {
        let mut fields = rest.split(';');
        match fields.next() {
            Some("A") => events.push(ParserEvent::PromptStart),
            Some("B") => events.push(ParserEvent::PromptEnd),
            Some("C") => events.push(ParserEvent::CommandStart),
            Some("D") => {
                // Exit code is the first param after D (if any); ignore trailing
                // fields like aid=. A non-numeric/absent code means "unknown" → 0.
                let code = fields
                    .next()
                    .and_then(|f| f.parse::<i32>().ok())
                    .unwrap_or(0);
                events.push(ParserEvent::CommandEnd(code));
            }
            _ => {}
        }
        return;
    }

    // OSC 7 ; file://host/path — CWD update (path is percent-encoded per RFC 3986).
    if let Some(rest) = s.strip_prefix("7;") {
        let raw = if let Some(uri) = rest.strip_prefix("file://") {
            if let Some(idx) = uri.find('/') { &uri[idx..] } else { uri }
        } else {
            rest
        };
        let path = percent_decode(raw);
        if !path.is_empty() {
            events.push(ParserEvent::CwdUpdate(path));
        }
        return;
    }

    // OSC 52 ; <selection> ; <base64-data> — clipboard set
    if let Some(rest) = s.strip_prefix("52;") {
        if let Some(data_start) = rest.find(';') {
            let b64_data = &rest[data_start + 1..];
            if b64_data != "?" {
                if let Ok(decoded) = base64_decode(b64_data.as_bytes()) {
                    if let Ok(text) = String::from_utf8(decoded) {
                        events.push(ParserEvent::ClipboardSet(text));
                    }
                }
            }
        }
        return;
    }

    // OSC 0 ; <title> (icon + window title) and OSC 2 ; <title> (window title).
    // OSC 1 sets only the icon name, which we don't surface, so it's left to the
    // pass-through path below. Emitting a semantic event here lets the reader drop
    // its hand-rolled title byte-scan.
    if let Some(rest) = s.strip_prefix("0;").or_else(|| s.strip_prefix("2;")) {
        events.push(ParserEvent::TitleUpdate(rest.to_string()));
        return;
    }

    // All other OSC sequences: reconstruct and pass through
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.push(0x1b);
    bytes.push(b']');
    bytes.extend_from_slice(payload);
    bytes.push(0x07);
    events.push(ParserEvent::Bytes(bytes));
}

/// Percent-decode an OSC 7 path (e.g. "/home/me/My%20Docs" → "/home/me/My Docs").
/// Decoded bytes are interpreted as UTF-8; invalid sequences fall back to the
/// raw input unchanged.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

fn base64_decode(input: &[u8]) -> Result<Vec<u8>, ()> {
    const TABLE: [u8; 256] = {
        let mut t = [0xFFu8; 256];
        let mut i = 0u8;
        loop {
            if i >= 26 { break; }
            t[(b'A' + i) as usize] = i;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 26 { break; }
            t[(b'a' + i) as usize] = 26 + i;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 10 { break; }
            t[(b'0' + i) as usize] = 52 + i;
            i += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t
    };

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;

    for &b in input {
        if b == b'=' || b == b'\n' || b == b'\r' {
            continue;
        }
        let val = TABLE[b as usize];
        if val == 0xFF {
            return Err(());
        }
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenate all Bytes events into one buffer (what downstream sees).
    fn collect_bytes(events: &[ParserEvent]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in events {
            if let ParserEvent::Bytes(b) = e {
                out.extend_from_slice(b);
            }
        }
        out
    }

    #[test]
    fn csi_not_split_across_feeds() {
        // A CSI sequence arriving in two reads (split mid-sequence) must surface
        // as a single contiguous run in ONE Bytes event, so interactive-mode
        // scanners see each CSI whole.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[3", &mut events); // first half: no complete sequence yet
        p.feed(b"1m", &mut events); // second half: completes ESC[31m

        // The whole CSI lands in a single Bytes event, never fragmented.
        let bytes_events: Vec<&Vec<u8>> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::Bytes(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(bytes_events.len(), 1, "CSI must not be split into pieces");
        assert_eq!(bytes_events[0].as_slice(), b"\x1b[31m");
    }

    #[test]
    fn text_around_csi_passes_through() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"ab\x1b[1mcd", &mut events);
        assert_eq!(collect_bytes(&events), b"ab\x1b[1mcd");
    }

    #[test]
    fn alt_screen_enter_leave_emitted_and_stripped() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1049h\x1b[?1049l", &mut events);
        // Both semantic events fire and the raw mode bytes are NOT passed through.
        assert!(matches!(events[0], ParserEvent::AltScreenEnter));
        assert!(matches!(events[1], ParserEvent::AltScreenLeave));
        assert!(collect_bytes(&events).is_empty());
    }

    #[test]
    fn alt_screen_enter_split_across_feeds() {
        // The mode sequence split across reads still emits the semantic event
        // (parser state persists), and never leaks bytes downstream.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?10", &mut events);
        p.feed(b"49h", &mut events);
        assert!(events.iter().any(|e| matches!(e, ParserEvent::AltScreenEnter)));
        assert!(collect_bytes(&events).is_empty());
    }

    fn count_alt_screen_enters(events: &[ParserEvent]) -> usize {
        events.iter().filter(|e| matches!(e, ParserEvent::AltScreenEnter)).count()
    }

    #[test]
    fn top_signature_promotes_to_alt_screen() {
        // procps-ng top startup: hide cursor, home, full clear. Should promote.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?25l\x1b[H\x1b[2J", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 1, "expected one synthetic AltScreenEnter");
        // [?25l + [H reach VTE on the main screen (harmless), [2J reaches the alt buffer.
        let bytes = collect_bytes(&events);
        assert!(bytes.windows(6).any(|w| w == b"\x1b[?25l"));
        assert!(bytes.windows(3).any(|w| w == b"\x1b[H"));
        assert!(bytes.windows(3).any(|w| w == b"\x1b[J") || bytes.windows(4).any(|w| w == b"\x1b[2J"));
    }

    #[test]
    fn shell_clear_does_not_promote() {
        // Shell `clear` / prompt redraw — no [?25l. Must not trigger TUI mode.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[H\x1b[2J", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 0);
    }

    #[test]
    fn progress_bar_does_not_promote() {
        // Spinner: hide cursor, write printable bytes, then [2J would be a TUI
        // signal but our heuristic disqualifies it because text appeared.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?25lLoading...\x1b[2J", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 0);
    }

    #[test]
    fn tui_redraw_does_not_double_promote() {
        // Top redraws every frame via [2J. Only the first should promote.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?25l\x1b[2J", &mut events);
        p.feed(b"\x1b[H\x1b[2J\x1b[H\x1b[2J", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 1);
    }

    #[test]
    fn command_start_resets_state() {
        // After OSC 133 ;C (a new command begins), the [?25l state from the
        // previous command must not carry over.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?25l", &mut events);
        p.feed(b"\x1b]133;C\x07", &mut events);
        p.feed(b"\x1b[2J", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 0);
    }

    #[test]
    fn real_alt_screen_followed_by_2j_does_not_double_promote() {
        // A real ?1049h app may also emit [?25l + [2J. Only one AltScreenEnter.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1049h\x1b[?25l\x1b[2J", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 1);
    }

    #[test]
    fn smkx_promotes_to_alt_screen() {
        // less -FRX (and any smkx-using TUI without smcup): CSI ?1h then ESC =.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1h\x1b=", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 1, "smkx pair must promote");
    }

    #[test]
    fn smkx_promotes_in_either_order() {
        // ESC = arrives before CSI ?1h — still promote when both are seen.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b=\x1b[?1h", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 1);
    }

    #[test]
    fn smkx_only_one_signal_does_not_promote() {
        // CSI ?1h alone (without ESC =) is too weak — some apps flip cursor
        // keys without being TUIs.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1h", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 0);

        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b=", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 0);
    }

    #[test]
    fn smkx_after_real_alt_screen_does_not_double_promote() {
        // vim sends ?1049h then smkx — only one AltScreenEnter should fire.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1049h\x1b[?1h\x1b=", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 1);
    }

    #[test]
    fn smkx_resets_on_command_start() {
        // If [?1h leaks across commands somehow, CommandStart clears it so
        // the next command doesn't get an unwanted promotion.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1h", &mut events);
        p.feed(b"\x1b]133;C\x07", &mut events);
        p.feed(b"\x1b=", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 0);
    }

    #[test]
    fn smkx_disarmed_by_rmkx() {
        // [?1l then ESC > — exit signals — must NOT count toward promotion.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1l\x1b>", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 0);
    }

    #[test]
    fn cursor_show_disarms_heuristic() {
        // [?25l followed by [?25h must clear the hidden flag, so a later
        // [2J does not promote.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?25l\x1b[?25h\x1b[2J", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 0);
    }

    #[test]
    fn osc133_command_lifecycle() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b]133;A\x07\x1b]133;C\x07\x1b]133;D;0\x07", &mut events);
        let kinds: Vec<_> = events
            .iter()
            .map(|e| match e {
                ParserEvent::PromptStart => "A",
                ParserEvent::CommandStart => "C",
                ParserEvent::CommandEnd(_) => "D",
                _ => "?",
            })
            .collect();
        assert_eq!(kinds, vec!["A", "C", "D"]);
    }

    // ── Regression tests for alt-screen detection on real-world TUIs ────────
    //
    // These pin the byte streams we captured from running `top` and `less` so
    // a future parser change can't silently break the "first frame full-size"
    // behavior we depend on for `top`, `git log`, `man`, etc. If you change
    // the heuristic, you must keep these passing.

    #[test]
    fn top_startup_real_byte_stream_promotes_once() {
        // Captured via `script -q -c top` (procps-ng 3.3.17). The order is:
        //   smkx (?1h, =), hide cursor (?25l), home (H), full-clear (2J), then
        //   SGR / charset setup. This combines BOTH heuristic paths (smkx pair
        //   and [?25l]+[2J]). We must emit exactly one AltScreenEnter.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(
            b"\x1b[?1h\x1b=\x1b[?25l\x1b[H\x1b[2J\x1b(B\x1b[m\x1b[39;49m",
            &mut events,
        );
        assert_eq!(
            count_alt_screen_enters(&events),
            1,
            "real top startup must emit exactly one AltScreenEnter"
        );
    }

    #[test]
    fn less_startup_real_byte_stream_promotes_once() {
        // less without smcup (`-X` / `LESS=FRX` — what git log uses by default)
        // does not write ?1049h. Its startup is dominated by smkx: CSI ?1h
        // followed shortly by ESC =. That pair is our trigger.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1h\x1b=\x1b[?12;25h", &mut events);
        assert_eq!(
            count_alt_screen_enters(&events),
            1,
            "less -FRX startup must emit exactly one AltScreenEnter"
        );
    }

    #[test]
    fn top_then_command_end_then_top_both_promote() {
        // Two top sessions in the same shell. After the first one ends
        // (OSC 133 ;D), the second top must still promote — the heuristic
        // state must reset on CommandStart. This is the regression that
        // would silently break "running top twice in a row".
        let mut p = Parser::new();
        let mut events = Vec::new();
        // session 1: prompt → command start → top → exit
        p.feed(b"\x1b]133;A\x07\x1b]133;C\x07", &mut events);
        p.feed(b"\x1b[?1h\x1b=\x1b[?25l\x1b[H\x1b[2J", &mut events);
        p.feed(b"\x1b[?1049l\x1b]133;D;0\x07", &mut events);
        // session 2: prompt → command start → top again
        p.feed(b"\x1b]133;A\x07\x1b]133;C\x07", &mut events);
        p.feed(b"\x1b[?1h\x1b=\x1b[?25l\x1b[H\x1b[2J", &mut events);
        assert_eq!(
            count_alt_screen_enters(&events),
            2,
            "each top invocation must promote independently"
        );
    }

    #[test]
    fn starship_prompt_cursor_toggle_does_not_promote() {
        // Prompt frameworks (Starship, P10k) sometimes bracket their paint
        // with [?25l]…[?25h] to avoid cursor flicker. They do NOT clear the
        // screen, so no [2J. A later shell `clear` (which sends [H][2J) must
        // not be retroactively promoted by the long-since-shown cursor.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?25l\x1b[1;1H\x1b[Kuser@host $ \x1b[?25h", &mut events);
        p.feed(b"\x1b[H\x1b[2J", &mut events); // shell `clear` afterwards
        assert_eq!(count_alt_screen_enters(&events), 0);
    }

    #[test]
    fn alt_screen_leave_rearms_smkx_for_same_command() {
        // A program that briefly enters alt-screen (?1049h … ?1049l) and
        // then runs another smkx-using sub-pager in the same command must
        // still re-promote. ?1049l clears tui_promoted so the next smkx
        // pair fires AltScreenEnter again.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1049h", &mut events); // first promotion
        p.feed(b"\x1b[?1049l", &mut events); // leave clears tui_promoted
        p.feed(b"\x1b[?1h\x1b=", &mut events); // smkx pager → re-promote
        assert_eq!(count_alt_screen_enters(&events), 2);
    }

    #[test]
    fn smkx_split_across_feeds_still_promotes() {
        // Network/PTY chunking can split CSI ?1h and ESC = across reads.
        // The flags persist on the Parser, so the pair still triggers.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1h", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 0);
        p.feed(b"\x1b", &mut events);
        p.feed(b"=", &mut events);
        assert_eq!(count_alt_screen_enters(&events), 1);
    }
}
