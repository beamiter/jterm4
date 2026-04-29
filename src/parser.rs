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
    /// Inside DCS/PM/APC — just consume until ST
    Ignore { buf: Vec<u8> },
}

pub struct Parser {
    state: State,
}

fn is_alt_screen_mode(params: &[u8]) -> bool {
    matches!(params, b"?47" | b"?1047" | b"?1049")
}

impl Parser {
    pub fn new() -> Self {
        Parser { state: State::default() }
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<ParserEvent> {
        let mut events: Vec<ParserEvent> = Vec::new();
        let mut passthrough: Vec<u8> = Vec::new();

        macro_rules! flush {
            () => {
                if !passthrough.is_empty() {
                    events.push(ParserEvent::Bytes(std::mem::take(&mut passthrough)));
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
                        passthrough.push(b);
                    }
                },

                State::Esc => match b {
                    b'[' => {
                        // ESC [ — start CSI; pass ESC [ through so ANSI colors etc. work
                        passthrough.push(0x1b);
                        passthrough.push(b'[');
                        self.state = State::Csi { buf: Vec::new() };
                    }
                    b']' => {
                        // ESC ] — start OSC; do NOT pass through yet (strip markers)
                        self.state = State::Osc { buf: Vec::new() };
                    }
                    b'P' | b'^' | b'_' => {
                        // DCS / PM / APC — ignore until ST
                        self.state = State::Ignore { buf: Vec::new() };
                    }
                    _ => {
                        // Other ESC sequences: pass through as-is
                        passthrough.push(0x1b);
                        passthrough.push(b);
                        self.state = State::Ground;
                    }
                },

                State::Csi { buf } => {
                    if (0x40..=0x7e).contains(&b) {
                        // Final byte of CSI sequence
                        let params = std::mem::take(buf);
                        self.state = State::Ground;
                        passthrough.push(b);
                        if b == b'h' && is_alt_screen_mode(&params) {
                            let len = passthrough.len();
                            passthrough.truncate(len.saturating_sub(params.len() + 3));
                            flush!();
                            events.push(ParserEvent::AltScreenEnter);
                        } else if b == b'l' && is_alt_screen_mode(&params) {
                            let len = passthrough.len();
                            passthrough.truncate(len.saturating_sub(params.len() + 3));
                            flush!();
                            events.push(ParserEvent::AltScreenLeave);
                        }
                    } else {
                        passthrough.push(b);
                        buf.push(b);
                    }
                }

                State::Osc { buf } => {
                    match b {
                        0x07 => {
                            let payload = std::mem::take(buf);
                            self.state = State::Ground;
                            log::debug!("OSC terminated by BEL, payload length: {}", payload.len());
                            flush!();
                            handle_osc(&payload, &mut events);
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
                    log::debug!("OSC terminated by ESC\\, payload length: {}", payload.len());
                    flush!();
                    handle_osc(&payload, &mut events);
                    if b != b'\\' {
                        // Not ST — re-process this byte in Ground
                        passthrough.push(b);
                    }
                }

                State::Ignore { buf: _ } => {
                    // Consume until BEL or ESC
                    if b == 0x07 || b == 0x1b {
                        self.state = State::Ground;
                    }
                }
            }
        }

        flush!();
        events
    }
}

fn handle_osc(payload: &[u8], events: &mut Vec<ParserEvent>) {
    let s = match std::str::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => {
            log::debug!("OSC payload not UTF-8: {:?}", payload);
            return;
        }
    };

    log::debug!("OSC received: {:?}", s);

    // OSC 133 ; <mark> — shell integration
    if let Some(rest) = s.strip_prefix("133;") {
        log::debug!("OSC 133 matched, rest={:?}", rest);
        match rest {
            "A" => {
                log::debug!("Emitting PromptStart");
                events.push(ParserEvent::PromptStart);
            }
            "B" => {
                log::debug!("Emitting PromptEnd");
                events.push(ParserEvent::PromptEnd);
            }
            "C" => {
                log::debug!("Emitting CommandStart");
                events.push(ParserEvent::CommandStart);
            }
            _ if rest.starts_with("D;") => {
                let code = rest[2..].parse::<i32>().unwrap_or(0);
                log::debug!("Emitting CommandEnd({})", code);
                events.push(ParserEvent::CommandEnd(code));
            }
            "D" => {
                log::debug!("Emitting CommandEnd(0)");
                events.push(ParserEvent::CommandEnd(0));
            }
            _ => {
                log::debug!("OSC 133 unrecognized marker: {:?}", rest);
            }
        }
        return;
    }

    // OSC 7 ; file://host/path — CWD update
    if let Some(rest) = s.strip_prefix("7;") {
        // rest is a file:// URI or just a path
        let path = if let Some(uri) = rest.strip_prefix("file://") {
            // strip host component (up to first '/')
            if let Some(idx) = uri.find('/') { &uri[idx..] } else { uri }
        } else {
            rest
        };
        if !path.is_empty() {
            events.push(ParserEvent::CwdUpdate(path.to_string()));
        }
        return;
    }

    // All other OSC sequences (e.g. OSC 0 for window title, OSC 8 for hyperlinks):
    // reconstruct and pass through so the VTE fallback can handle them
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.push(0x1b);
    bytes.push(b']');
    bytes.extend_from_slice(payload);
    bytes.push(0x07);
    events.push(ParserEvent::Bytes(bytes));
}
