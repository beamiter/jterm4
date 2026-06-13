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
}

fn is_alt_screen_mode(params: &[u8]) -> bool {
    matches!(params, b"?47" | b"?1047" | b"?1049")
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Parser {
            state: State::default(),
            passthrough: Vec::with_capacity(4096),
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
                        self.passthrough.push(b);
                    }
                },

                State::Esc => match b {
                    b'[' => {
                        self.passthrough.push(0x1b);
                        self.passthrough.push(b'[');
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
                        self.passthrough.push(b);
                        if b == b'h' && is_alt_screen_mode(&params) {
                            let len = self.passthrough.len();
                            self.passthrough.truncate(len.saturating_sub(params.len() + 3));
                            flush!();
                            events.push(ParserEvent::AltScreenEnter);
                        } else if b == b'l' && is_alt_screen_mode(&params) {
                            let len = self.passthrough.len();
                            self.passthrough.truncate(len.saturating_sub(params.len() + 3));
                            flush!();
                            events.push(ParserEvent::AltScreenLeave);
                        }
                    } else {
                        self.passthrough.push(b);
                        buf.push(b);
                    }
                }

                State::Osc { buf } => {
                    match b {
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
                    }
                }

                State::OscEsc { payload } => {
                    let payload = std::mem::take(payload);
                    self.state = State::Ground;
                    flush!();
                    handle_osc(&payload, events);
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
