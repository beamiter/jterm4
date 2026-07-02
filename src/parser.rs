//! OSC/CSI stream parser. Splits a raw PTY byte stream into semantic
//! `ParserEvent`s — passing through display bytes while extracting the OSC 133
//! shell-integration marks, OSC 52 clipboard, alt-screen toggles and APC
//! sequences that drive the block view. OSC 7/title sequences pass through to
//! VTE so its native cwd/title signals stay authoritative.

/// Which color slot an OSC 10/11/12/4 query asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorKind {
    /// OSC 10 — default foreground.
    Foreground,
    /// OSC 11 — default background.
    Background,
    /// OSC 12 — cursor color.
    Cursor,
    /// OSC 4;N — palette index N.
    Palette(u8),
}

/// Which terminal-capability handshake an app sent. The active VTE in block view
/// has no real PTY return path, so we synthesize a sensible "not supported"
/// reply ourselves to keep neovim/helix from blocking on a missing response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocolQuery {
    /// `CSI ? u` — kitty progressive-enhancement flag query.
    KittyQuery,
    /// `CSI ? 4 m` — XTQMODKEYS modifyOtherKeys query.
    ModifyOtherKeysQuery,
    /// `CSI c` / `CSI 0 c` — primary device attributes (DA1).
    PrimaryDeviceAttributes,
    /// `CSI > c` / `CSI > 0 c` — secondary device attributes (DA2). Different
    /// reply format from DA1 (`CSI > Pp ; Pv ; Pc c` vs `CSI ? ... c`).
    SecondaryDeviceAttributes,
    /// `CSI = c` / `CSI = 0 c` — tertiary device attributes (DA3).
    TertiaryDeviceAttributes,
    /// `CSI > q` — XTVERSION (xterm name/version request).
    XtVersion,
    /// `CSI 5 n` — DSR: report device status (reply `\e[0n` = OK).
    DeviceStatus,
    /// `CSI 6 n` — DSR: report cursor position (reply `\e[<row>;<col>R`).
    CursorPosition,
}

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
    /// OSC 7770 — rsh-specific: the remote shell announces its session ID at
    /// startup. The UI stores it on the tab's RemoteConn so subsequent
    /// reconnects pass `--session <id>` and rsh restores cwd/env/aliases.
    RemoteSessionId(String),
    /// CSI ? 1049 h — alt screen entered (vim, less, etc.)
    AltScreenEnter,
    /// CSI ? 1049 l — alt screen left.
    AltScreenLeave,
    /// OSC 52 — application set clipboard content.
    ClipboardSet(String),
    /// OSC 52 with `?` — app is asking for current clipboard content.
    /// We reply with an empty payload (`\e]52;c;\e\\`) so probers (tmux/vim)
    /// know we accept SET but don't expose clipboard contents to the shell.
    ClipboardQuery,
    /// APC sequence (ESC _) — Kitty graphics protocol or similar.
    ApcSequence(Vec<u8>),
    /// CSI ? <mode> h / l — DEC private mode change. Emitted in addition to
    /// pass-through so block_view can track reporting modes.
    DecsetMode { mode: u32, set: bool },
    /// OSC 10/11/12/4 with a `?` — app is asking the terminal what color it uses.
    /// The caller must write a `\e]<n>;rgb:RRRR/GGGG/BBBB\e\\` reply to the PTY.
    ColorQuery(ColorKind),
    /// App queried a keyboard/capability protocol. Caller should reply on the PTY
    /// with a canned "not supported" / level-0 response so the app falls back
    /// gracefully (otherwise neovim, helix, etc. hang waiting on the reply).
    KeyboardProtocolQuery(KeyboardProtocolQuery),
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
    /// Inside DCS (ESC P): collect until ST. Unlike `Ignore`, the bytes are
    /// rewrapped as `ESC P ... ESC \` and passed through to the active VTE so
    /// sixel graphics, DECRQSS replies, and tmux passthrough survive block mode.
    Dcs { buf: Vec<u8> },
    /// Saw ESC while in DCS — next byte should be '\' for ST.
    DcsEsc { payload: Vec<u8> },
    /// Inside PM (ESC ^) — consume until ST and discard.
    Ignore,
    /// Saw ESC while in PM — consume the ST final byte too.
    IgnoreEsc,
}

/// Which mouse-tracking mode the shell asked for. The active VTE in block-view
/// has no real PTY, so VTE never auto-generates mouse reports; the caller drives
/// reporting itself by reading this state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    None,
    /// `?9` — only button presses (no release).
    X10,
    /// `?1000` — button press + release.
    Normal,
    /// `?1002` — press/release + motion while a button is held.
    ButtonEvent,
    /// `?1003` — press/release + all motion.
    AnyEvent,
}

/// Wire format for mouse reports. Set by `?1006`, `?1015`, `?1005` (or default
/// xterm encoding if none enabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// Legacy `\e[M` + 3 bytes (button + 32, col + 32, row + 32).
    #[default]
    Default,
    /// `?1006` — SGR: `\e[<b;col;row;{M|m}`.
    Sgr,
    /// `?1015` — urxvt: `\e[b;col;row;M`.
    Urxvt,
    /// `?1005` — UTF-8 encoded coordinates.
    Utf8,
}

pub struct Parser {
    state: State,
    passthrough: Vec<u8>,
    config: ParserConfig,
    /// `?2004` — shell asked for paste content to be bracketed with `\e[200~`
    /// / `\e[201~`. The caller wraps its own `Paste` write when this is on.
    bracketed_paste: bool,
    /// Which mouse mode is currently active (highest-priority "h" wins).
    mouse_mode: MouseMode,
    /// Active mouse encoding flags. SGR/Urxvt/Utf8 are toggled independently; a
    /// later "h" replaces the encoding choice.
    mouse_encoding: MouseEncoding,
    /// `?1004` — shell asked for `\e[I` / `\e[O` on focus enter/leave.
    focus_events: bool,
}

#[derive(Clone, Copy)]
pub struct ParserConfig {
    pub mouse_reporting: bool,
    pub focus_reporting: bool,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            mouse_reporting: true,
            focus_reporting: true,
        }
    }
}

fn is_alt_screen_mode(params: &[u8]) -> bool {
    matches!(params, b"?47" | b"?1047" | b"?1049")
}

fn is_mouse_reporting_mode(params: &[u8]) -> bool {
    matches!(
        params,
        b"?9"
            | b"?1000"
            | b"?1001"
            | b"?1002"
            | b"?1003"
            | b"?1005"
            | b"?1006"
            | b"?1015"
            | b"?1016"
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
            bracketed_paste: false,
            mouse_mode: MouseMode::None,
            mouse_encoding: MouseEncoding::Default,
            focus_events: false,
        }
    }

    /// True while the shell has `?2004` enabled — callers should wrap pasted
    /// content with `\e[200~` / `\e[201~` before writing to the PTY.
    #[allow(dead_code)]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Currently active mouse-tracking mode, or `None` when reporting is off.
    #[allow(dead_code)]
    pub fn mouse_mode(&self) -> MouseMode {
        self.mouse_mode
    }

    /// Wire encoding the next mouse report should use.
    #[allow(dead_code)]
    pub fn mouse_encoding(&self) -> MouseEncoding {
        self.mouse_encoding
    }

    /// True while `?1004` is enabled — callers should emit `\e[I` on focus-in,
    /// `\e[O` on focus-out.
    #[allow(dead_code)]
    pub fn focus_events(&self) -> bool {
        self.focus_events
    }

    /// Apply each `?N` token from a `CSI ? Pm h/l` to the snooped state.
    /// `enable` = true for `h`, false for `l`. Unknown modes are ignored —
    /// they still pass through to the VTE.
    fn update_dec_private_modes(&mut self, params: &[u8], enable: bool) -> Vec<u32> {
        let mut modes = Vec::new();
        for token in params.split(|&c| c == b';') {
            // Each token may itself start with `?` if the shell sent
            // `CSI ?1;?2 h`; tolerate that.
            let token = token.strip_prefix(b"?").unwrap_or(token);
            let n: u32 = match std::str::from_utf8(token).ok().and_then(|s| s.parse().ok()) {
                Some(n) => n,
                None => continue,
            };
            modes.push(n);
            match n {
                2004 => self.bracketed_paste = enable,
                9 => {
                    self.mouse_mode = if enable {
                        MouseMode::X10
                    } else {
                        MouseMode::None
                    }
                }
                1000 => {
                    self.mouse_mode = if enable {
                        MouseMode::Normal
                    } else {
                        MouseMode::None
                    }
                }
                1002 => {
                    self.mouse_mode = if enable {
                        MouseMode::ButtonEvent
                    } else {
                        MouseMode::None
                    }
                }
                1003 => {
                    self.mouse_mode = if enable {
                        MouseMode::AnyEvent
                    } else {
                        MouseMode::None
                    }
                }
                1004 => self.focus_events = enable,
                1005 => {
                    self.mouse_encoding = if enable {
                        MouseEncoding::Utf8
                    } else {
                        MouseEncoding::Default
                    }
                }
                1006 => {
                    self.mouse_encoding = if enable {
                        MouseEncoding::Sgr
                    } else {
                        MouseEncoding::Default
                    }
                }
                1015 => {
                    self.mouse_encoding = if enable {
                        MouseEncoding::Urxvt
                    } else {
                        MouseEncoding::Default
                    }
                }
                _ => {}
            }
        }
        modes
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

        // Ground-state fast-path: bulk-copy runs of bytes until the next ESC.
        // The previous per-byte loop dominated cost on heavy text streams; ESC
        // is the only byte that exits Ground, so memchr lets us hop directly
        // to the next state transition.
        let mut i = 0usize;
        let len = data.len();
        while i < len {
            if matches!(self.state, State::Ground) {
                match memchr::memchr(0x1b, &data[i..]) {
                    Some(off) => {
                        if off > 0 {
                            self.passthrough.extend_from_slice(&data[i..i + off]);
                        }
                        i += off + 1;
                        self.state = State::Esc;
                        continue;
                    }
                    None => {
                        self.passthrough.extend_from_slice(&data[i..]);
                        break;
                    }
                }
            }

            let b = data[i];
            i += 1;
            match &mut self.state {
                State::Ground => unreachable!("handled by fast-path above"),

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
                    b'P' => {
                        self.state = State::Dcs { buf: Vec::new() };
                    }
                    b'^' => {
                        self.state = State::Ignore;
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
                        if (b == b'h' || b == b'l')
                            && params.first() == Some(&b'?')
                            && !is_alt_screen_mode(&params)
                        {
                            for mode in self.update_dec_private_modes(&params[1..], b == b'h') {
                                events.push(ParserEvent::DecsetMode {
                                    mode,
                                    set: b == b'h',
                                });
                            }
                        }
                        if b == b'h' && is_alt_screen_mode(&params) {
                            // Recognized alt-screen enter: drop the sequence bytes
                            // (never passed through) and emit the semantic event.
                            flush!();
                            events.push(ParserEvent::AltScreenEnter);
                        } else if b == b'l' && is_alt_screen_mode(&params) {
                            flush!();
                            events.push(ParserEvent::AltScreenLeave);
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
                            // Detect terminal-capability handshakes whose response
                            // the active VTE would write back through its own PTY
                            // (which is not connected). The caller synthesizes a
                            // canned reply on `ctx.pty` so neovim/helix/etc. don't
                            // hang waiting on it. The byte stream itself is still
                            // passed through so the VTE updates its internal state.
                            //
                            // `CSI ? u`                       — kitty keyboard query
                            // `CSI ? 4 m`                     — XTQMODKEYS query
                            // `CSI c`, `CSI 0 c`              — primary DA (DA1)
                            // `CSI > c`, `CSI > 0 c`          — secondary DA (DA2)
                            // `CSI = c`, `CSI = 0 c`          — tertiary DA (DA3)
                            // `CSI > q`                       — XTVERSION
                            // `CSI 5 n` / `CSI 6 n`           — DSR status / cursor pos
                            match (b, params.as_slice()) {
                                (b'u', b"?") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::KittyQuery,
                                    ));
                                }
                                (b'm', b"?4") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::ModifyOtherKeysQuery,
                                    ));
                                }
                                (b'c', b"") | (b'c', b"0") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::PrimaryDeviceAttributes,
                                    ));
                                }
                                (b'c', b">") | (b'c', b">0") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::SecondaryDeviceAttributes,
                                    ));
                                }
                                (b'c', b"=") | (b'c', b"=0") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::TertiaryDeviceAttributes,
                                    ));
                                }
                                (b'q', b">") | (b'q', b">0") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::XtVersion,
                                    ));
                                }
                                (b'n', b"5") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::DeviceStatus,
                                    ));
                                }
                                (b'n', b"6") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::CursorPosition,
                                    ));
                                }
                                _ => {}
                            }
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

                State::Osc { buf } => match b {
                    0x07 => {
                        let payload = std::mem::take(buf);
                        self.state = State::Ground;
                        flush!();
                        handle_osc(&payload, events);
                    }
                    0x1b => {
                        let payload = std::mem::take(buf);
                        self.state = State::OscEsc { payload };
                    }
                    _ => {
                        buf.push(b);
                    }
                },

                State::OscEsc { payload } => {
                    let payload = std::mem::take(payload);
                    self.state = State::Ground;
                    flush!();
                    handle_osc(&payload, events);
                    if b != b'\\' {
                        self.passthrough.push(b);
                    }
                }

                State::Apc { buf } => match b {
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
                },

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

                State::Dcs { buf } => match b {
                    0x07 => {
                        let payload = std::mem::take(buf);
                        self.state = State::Ground;
                        emit_dcs_passthrough(&payload, &mut self.passthrough);
                    }
                    0x1b => {
                        let payload = std::mem::take(buf);
                        self.state = State::DcsEsc { payload };
                    }
                    _ => {
                        buf.push(b);
                        // Bound runaway DCS (malformed stream) the same way CSI is bounded.
                        if buf.len() > 1 << 20 {
                            let payload = std::mem::take(buf);
                            self.state = State::Ground;
                            emit_dcs_passthrough(&payload, &mut self.passthrough);
                        }
                    }
                },

                State::DcsEsc { payload } => {
                    let payload = std::mem::take(payload);
                    self.state = State::Ground;
                    emit_dcs_passthrough(&payload, &mut self.passthrough);
                    if b == b'\\' {
                        // Consumed the ST terminator.
                    } else {
                        self.passthrough.push(b);
                    }
                }

                State::Ignore => {
                    if b == 0x07 {
                        self.state = State::Ground;
                    } else if b == 0x1b {
                        self.state = State::IgnoreEsc;
                    }
                }

                State::IgnoreEsc => {
                    if b != 0x1b {
                        self.state = State::Ground;
                    }
                }
            }
        }

        flush!();
    }
}

/// Rewrap a DCS payload as `ESC P ... ESC \` and append to the passthrough buffer
/// so the active VTE — which can interpret sixel, DECRQSS replies, tmux
/// passthrough, etc. — gets the original sequence verbatim.
fn emit_dcs_passthrough(payload: &[u8], passthrough: &mut Vec<u8>) {
    passthrough.reserve(payload.len() + 4);
    passthrough.push(0x1b);
    passthrough.push(b'P');
    passthrough.extend_from_slice(payload);
    passthrough.push(0x1b);
    passthrough.push(b'\\');
}

fn handle_osc(payload: &[u8], events: &mut Vec<ParserEvent>) {
    let s = match std::str::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => return,
    };

    // OSC 133 ; <mark> [; params...] — shell integration (FTCS).
    if let Some(rest) = s.strip_prefix("133;") {
        let mut fields = rest.split(';');
        match fields.next() {
            Some("A") => events.push(ParserEvent::PromptStart),
            Some("B") => events.push(ParserEvent::PromptEnd),
            Some("C") => events.push(ParserEvent::CommandStart),
            Some("D") => {
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

    // OSC 7770 ; <session-id> — rsh-specific session announce (see rsh osc.rs:107).
    if let Some(rest) = s.strip_prefix("7770;") {
        let id = rest.trim();
        if !id.is_empty() {
            events.push(ParserEvent::RemoteSessionId(id.to_string()));
        }
        return;
    }

    // OSC 7 (cwd), OSC 0 / 1 / 2 (title/icon), and everything else: pass through
    // unchanged. VTE consumes them natively and fires
    // current-directory-uri-notify / window-title-changed signals, which the
    // block_view subscribes to instead of re-parsing here.
    if s.starts_with("7;") {
        let mut bytes = Vec::with_capacity(payload.len() + 4);
        bytes.push(0x1b);
        bytes.push(b']');
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(b"\x1b\\");
        events.push(ParserEvent::Bytes(bytes));
        return;
    }

    // OSC 10 ; ? / OSC 11 ; ? / OSC 12 ; ?  — color queries (XParseColor reply).
    // The active VTE in block view has no return PTY, so the response we'd
    // expect VTE to emit never reaches the app. Emit a semantic event and let
    // the caller write a reply on the real PTY.
    for (prefix, kind) in [
        ("10;", ColorKind::Foreground),
        ("11;", ColorKind::Background),
        ("12;", ColorKind::Cursor),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if rest.starts_with('?') {
                events.push(ParserEvent::ColorQuery(kind));
                return;
            }
        }
    }

    // OSC 4 ; <idx> ; ? — palette color query.
    if let Some(rest) = s.strip_prefix("4;") {
        let mut it = rest.splitn(2, ';');
        if let (Some(idx_str), Some(value)) = (it.next(), it.next()) {
            if value.starts_with('?') {
                if let Ok(idx) = idx_str.parse::<u8>() {
                    events.push(ParserEvent::ColorQuery(ColorKind::Palette(idx)));
                    return;
                }
            }
        }
    }

    // OSC 52 ; <selection> ; <base64-data | ?> — clipboard set / query
    if let Some(rest) = s.strip_prefix("52;") {
        if let Some(data_start) = rest.find(';') {
            let b64_data = &rest[data_start + 1..];
            if b64_data == "?" {
                events.push(ParserEvent::ClipboardQuery);
            } else if let Ok(decoded) = base64_decode(b64_data.as_bytes()) {
                if let Ok(text) = String::from_utf8(decoded) {
                    events.push(ParserEvent::ClipboardSet(text));
                }
            }
        }
        return;
    }

    // All other OSC sequences: reconstruct and pass through.
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.push(0x1b);
    bytes.push(b']');
    bytes.extend_from_slice(payload);
    bytes.push(0x07);
    events.push(ParserEvent::Bytes(bytes));
}

fn base64_decode(input: &[u8]) -> Result<Vec<u8>, ()> {
    const TABLE: [u8; 256] = {
        let mut t = [0xFFu8; 256];
        let mut i = 0u8;
        loop {
            if i >= 26 {
                break;
            }
            t[(b'A' + i) as usize] = i;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 26 {
                break;
            }
            t[(b'a' + i) as usize] = 26 + i;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 10 {
                break;
            }
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
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[3", &mut events);
        p.feed(b"1m", &mut events);
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
    fn alt_screen_enter_leave_emitted_and_stripped() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1049h\x1b[?1049l", &mut events);
        assert!(matches!(events[0], ParserEvent::AltScreenEnter));
        assert!(matches!(events[1], ParserEvent::AltScreenLeave));
        assert!(collect_bytes(&events).is_empty());
    }

    #[test]
    fn dcs_is_passed_through_not_dropped() {
        // A DCS sixel sequence: ESC P q ... ESC \. The whole thing should
        // appear verbatim in the Bytes stream so the active VTE can render it.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"before\x1bPq#0;2;0;0;0!100~-\x1b\\after", &mut events);
        let bytes = collect_bytes(&events);
        // The plain "before" and "after" survive, and the DCS round-trips.
        assert!(bytes.windows(6).any(|w| w == b"before"));
        assert!(bytes.windows(5).any(|w| w == b"after"));
        assert!(bytes.windows(3).any(|w| w == b"\x1bPq"));
        assert!(bytes.windows(2).any(|w| w == b"\x1b\\"));
    }

    #[test]
    fn pm_st_does_not_leak_backslash() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"before\x1b^ignored\x1b\\after", &mut events);
        assert_eq!(collect_bytes(&events), b"beforeafter");
    }

    #[test]
    fn osc_color_queries_emit_events() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(
            b"\x1b]11;?\x07\x1b]10;?\x07\x1b]12;?\x1b\\\x1b]4;5;?\x07",
            &mut events,
        );
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::ColorQuery(k) => Some(*k),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                ColorKind::Background,
                ColorKind::Foreground,
                ColorKind::Cursor,
                ColorKind::Palette(5),
            ]
        );
    }

    #[test]
    fn keyboard_protocol_queries_emit_events() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        // kitty flag query, modifyOtherKeys query, primary & secondary DA.
        p.feed(b"\x1b[?u\x1b[?4m\x1b[c\x1b[>c", &mut events);
        let qs: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::KeyboardProtocolQuery(q) => Some(*q),
                _ => None,
            })
            .collect();
        assert_eq!(
            qs,
            vec![
                KeyboardProtocolQuery::KittyQuery,
                KeyboardProtocolQuery::ModifyOtherKeysQuery,
                KeyboardProtocolQuery::PrimaryDeviceAttributes,
                KeyboardProtocolQuery::SecondaryDeviceAttributes,
            ]
        );
    }

    #[test]
    fn da1_da2_da3_emit_distinct_events() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[0c\x1b[>0c\x1b[=0c", &mut events);
        let qs: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::KeyboardProtocolQuery(q) => Some(*q),
                _ => None,
            })
            .collect();
        assert_eq!(
            qs,
            vec![
                KeyboardProtocolQuery::PrimaryDeviceAttributes,
                KeyboardProtocolQuery::SecondaryDeviceAttributes,
                KeyboardProtocolQuery::TertiaryDeviceAttributes,
            ]
        );
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

    #[test]
    fn osc7_cwd_passes_through_to_vte() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b]7;file://host/home/me/dir\x07", &mut events);
        assert_eq!(
            collect_bytes(&events),
            b"\x1b]7;file://host/home/me/dir\x1b\\"
        );
    }

    #[test]
    fn mouse_reporting_dropped_when_disabled_but_event_emitted() {
        let mut p = Parser::with_config(ParserConfig {
            mouse_reporting: false,
            focus_reporting: true,
        });
        let mut events = Vec::new();
        p.feed(b"\x1b[?1000h", &mut events);
        assert!(collect_bytes(&events).is_empty());
        assert!(events.iter().any(|e| matches!(
            e,
            ParserEvent::DecsetMode {
                mode: 1000,
                set: true
            }
        )));
    }

    #[test]
    fn osc_7770_emits_remote_session_id() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b]7770;home-main\x1b\\", &mut events);
        let id = events.iter().find_map(|e| match e {
            ParserEvent::RemoteSessionId(s) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(id.as_deref(), Some("home-main"));
    }

    #[test]
    fn osc_7770_empty_payload_ignored() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b]7770;\x07", &mut events);
        assert!(events
            .iter()
            .all(|e| !matches!(e, ParserEvent::RemoteSessionId(_))));
    }
}
