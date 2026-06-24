//! Parser regressions: byte-stream → event-stream invariants that we've
//! actually had to debug. Each test pins one previously-broken (or
//! easily-broken) behavior so a future refactor can't silently regress it.
//!
//! Lives in `tests/` (not `src/parser.rs#tests`) because it asserts at the
//! crate's public-API surface; in-file tests cover lower-level state
//! transitions.

mod common;

use jterm4::parser::{Parser, ParserConfig, ParserEvent};

fn feed_all(p: &mut Parser, data: &[u8]) -> Vec<ParserEvent> {
    let mut events = Vec::new();
    p.feed(data, &mut events);
    events
}

fn collect_bytes(events: &[ParserEvent]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in events {
        if let ParserEvent::Bytes(b) = e {
            out.extend_from_slice(b);
        }
    }
    out
}

/// OSC 133 ; A ; cl=m ; k=i — real shells (bash-preexec, starship, fish)
/// emit extra `;`-separated params after the mark. The parser must read only
/// the first field; anything after is shell-private and ignored.
#[test]
fn osc133_a_with_extra_params_still_recognised() {
    common::setup_test_env();
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"\x1b]133;A;cl=m;k=i\x07");
    assert!(
        events.iter().any(|e| matches!(e, ParserEvent::PromptStart)),
        "PromptStart must fire even with extra params: {events:?}"
    );
}

/// OSC 133 ; D ; <code> ; aid=N — exit-code prefix followed by Warp-style
/// `aid=` field. The code must be parsed from the field immediately after D;
/// trailing fields must NOT corrupt it.
#[test]
fn osc133_d_with_trailing_aid_field_parses_exit_code() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"\x1b]133;D;42;aid=7\x07");
    let code = events.iter().find_map(|e| match e {
        ParserEvent::CommandEnd(c) => Some(*c),
        _ => None,
    });
    assert_eq!(code, Some(42));
}

/// OSC 133 ; D with no code — some shells emit a bare `D` to signal "done,
/// unknown exit." Must default to 0 instead of skipping the event.
#[test]
fn osc133_d_bare_emits_zero() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"\x1b]133;D\x07");
    let code = events.iter().find_map(|e| match e {
        ParserEvent::CommandEnd(c) => Some(*c),
        _ => None,
    });
    assert_eq!(code, Some(0), "missing exit code defaults to 0");
}

/// OSC 133 ; D ; <non-numeric> — defensive: a malformed exit code must NOT
/// drop the CommandEnd event (otherwise the block never finalises and the
/// active cell stays "running" forever).
#[test]
fn osc133_d_non_numeric_code_defaults_to_zero() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"\x1b]133;D;bogus\x07");
    let code = events.iter().find_map(|e| match e {
        ParserEvent::CommandEnd(c) => Some(*c),
        _ => None,
    });
    assert_eq!(code, Some(0));
}

/// OSC split across feed boundaries — parser state must persist across the
/// read boundary or the whole sequence leaks into Bytes and the OSC event
/// never fires.
#[test]
fn osc133_d_split_across_feeds_still_fires() {
    let mut p = Parser::new();
    let mut events = Vec::new();
    p.feed(b"\x1b]13", &mut events);
    p.feed(b"3;D;1\x07", &mut events);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ParserEvent::CommandEnd(1))),
        "split OSC 133 must still resolve to CommandEnd(1): {events:?}"
    );
    assert!(
        collect_bytes(&events).is_empty(),
        "split OSC bytes must not leak as passthrough"
    );
}

/// OSC terminated by `ESC \` (proper ST) instead of BEL — the spec allows
/// both. Many TUIs (vim, less) emit ST-form. Parser must accept both.
#[test]
fn osc_terminated_by_st_recognised() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"\x1b]133;A\x1b\\");
    assert!(events.iter().any(|e| matches!(e, ParserEvent::PromptStart)));
    assert!(
        collect_bytes(&events).is_empty(),
        "ST `ESC \\` must be consumed, not leaked: {events:?}"
    );
}

/// OSC 52 ; c ; <base64> — application-set clipboard. We don't have a VTE
/// hook for this so the parser must decode it and emit ClipboardSet.
#[test]
fn osc52_clipboard_set_decodes_base64() {
    let mut p = Parser::new();
    // "hello" base64 -> aGVsbG8=
    let events = feed_all(&mut p, b"\x1b]52;c;aGVsbG8=\x07");
    let payload = events.iter().find_map(|e| match e {
        ParserEvent::ClipboardSet(s) => Some(s.clone()),
        _ => None,
    });
    assert_eq!(payload.as_deref(), Some("hello"));
}

/// OSC 52 ; c ; ? — query, NOT a set. Must NOT fire ClipboardSet (would
/// otherwise corrupt the user's actual clipboard with the literal "?").
#[test]
fn osc52_clipboard_query_does_not_set() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"\x1b]52;c;?\x07");
    let any_set = events
        .iter()
        .any(|e| matches!(e, ParserEvent::ClipboardSet(_)));
    assert!(!any_set, "OSC 52 query `?` must not fire ClipboardSet");
}

/// DCS / PM sequences ignored cleanly. vim emits dozens of XTGETTCAP
/// queries `\eP+q<hex>\e\\` per startup; the closing `\` of the ST was
/// previously leaking into the Bytes stream (one stray `\` per query,
/// hundreds across a vim session).
#[test]
fn dcs_consumed_including_trailing_st_byte() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"hi\x1bP+q544e\x1b\\bye");
    assert_eq!(
        collect_bytes(&events),
        b"hibye",
        "DCS body AND its `ESC \\` ST must both be consumed: {events:?}"
    );
}

/// APC (ESC _) — Kitty graphics & similar. Must surface as an ApcSequence
/// event with the raw payload, not as text bytes or a passthrough leak.
#[test]
fn apc_kitty_graphics_emitted_as_event() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"\x1b_Gf=32,s=10\x1b\\");
    let payload = events.iter().find_map(|e| match e {
        ParserEvent::ApcSequence(b) => Some(b.clone()),
        _ => None,
    });
    assert_eq!(payload.as_deref(), Some(&b"Gf=32,s=10"[..]));
    assert!(collect_bytes(&events).is_empty());
}

/// Decset chaining — `CSI ?1000;1006h` enables both modes in one sequence.
/// Each mode number must surface as its own DecsetMode event.
#[test]
fn decset_chained_modes_emit_each() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"\x1b[?1000;1006h");
    let modes: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            ParserEvent::DecsetMode { mode, set: true } => Some(*mode),
            _ => None,
        })
        .collect();
    assert_eq!(modes, vec![1000, 1006]);
}

/// Focus reporting (`CSI ?1004h`) must be dropped from the byte stream when
/// the config disables it. Otherwise VTE sends focus-in/out CSI replies on
/// every alt-tab and apps see spurious events.
#[test]
fn focus_reporting_dropped_when_disabled() {
    let mut p = Parser::with_config(ParserConfig {
        mouse_reporting: true,
        focus_reporting: false,
    });
    let events = feed_all(&mut p, b"before\x1b[?1004hafter");
    assert_eq!(
        collect_bytes(&events),
        b"beforeafter",
        "focus-reporting enable sequence must not pass through: {events:?}"
    );
}

/// Mouse reporting passes through when the config allows it (mirrors VTE's
/// default behaviour, so apps like htop / vim work).
#[test]
fn mouse_reporting_passes_through_when_enabled() {
    let mut p = Parser::with_config(ParserConfig {
        mouse_reporting: true,
        focus_reporting: true,
    });
    let events = feed_all(&mut p, b"\x1b[?1000h");
    assert_eq!(collect_bytes(&events), b"\x1b[?1000h");
}

/// CRLF and bare CR pass through verbatim — parser must NOT silently
/// normalise. The block_view layer (and the PTY's own termios) own
/// canonical-mode rewrites; if the parser touches \r the per-block output
/// reflow gets out of sync with what VTE renders.
#[test]
fn crlf_and_bare_cr_pass_through_unchanged() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"a\r\nb\rc");
    assert_eq!(collect_bytes(&events), b"a\r\nb\rc");
}

/// Each `feed()` call surfaces one Bytes event for its contiguous run.
/// Lets the reader coalesce multiple PTY reads into one feed (commit
/// 326a210) without changing downstream event count.
#[test]
fn single_feed_call_emits_at_most_one_bytes_event_per_run() {
    let mut p = Parser::new();
    let events = feed_all(&mut p, b"hello world no escapes");
    let n_bytes = events
        .iter()
        .filter(|e| matches!(e, ParserEvent::Bytes(_)))
        .count();
    assert_eq!(n_bytes, 1);
    assert_eq!(collect_bytes(&events), b"hello world no escapes");
}

/// Multiple feeds with no escapes each produce their own Bytes event
/// (parser does NOT cross-feed coalesce). The reader's coalescer sits
/// upstream and is responsible for batching reads before they reach feed().
#[test]
fn parser_does_not_coalesce_across_feed_boundaries() {
    let mut p = Parser::new();
    let mut events = Vec::new();
    p.feed(b"first", &mut events);
    p.feed(b"second", &mut events);
    let n_bytes = events
        .iter()
        .filter(|e| matches!(e, ParserEvent::Bytes(_)))
        .count();
    assert_eq!(n_bytes, 2, "events: {events:?}");
}

/// PAGER neutralization smoke: long colored output from git log (PAGER=cat
/// in block mode) is just bytes + SGR + LF — must round-trip with no event
/// other than Bytes (and no swallowed escapes).
#[test]
fn pager_neutralized_colored_output_round_trips() {
    let mut p = Parser::new();
    let input = b"\x1b[33mcommit abcdef0123456789\x1b[0m\n\
                  Author: someone <a@b.com>\n\
                  \n    body line\n";
    let events = feed_all(&mut p, input);
    assert_eq!(collect_bytes(&events), input);
    // No semantic events for plain colored text.
    assert!(events
        .iter()
        .all(|e| matches!(e, ParserEvent::Bytes(_))));
}

/// The actual sequence a real block ends with: OSC 133;C, the command's
/// own output (with SGR), OSC 133;D;<code>. Asserts all three event
/// markers fire in order around an output run, so the block finalize path
/// has the (start, bytes..., end) triple it needs to construct a finished
/// block.
#[test]
fn full_command_lifecycle_round_trip() {
    let mut p = Parser::new();
    let events = feed_all(
        &mut p,
        b"\x1b]133;A\x07prompt$ \x1b]133;B\x07ls\n\
          \x1b]133;C\x07\x1b[34mDocuments\x1b[0m\nfile.txt\n\
          \x1b]133;D;0\x07",
    );

    let marks: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            ParserEvent::PromptStart => Some("A"),
            ParserEvent::PromptEnd => Some("B"),
            ParserEvent::CommandStart => Some("C"),
            ParserEvent::CommandEnd(_) => Some("D"),
            _ => None,
        })
        .collect();
    assert_eq!(marks, vec!["A", "B", "C", "D"]);

    // The bytes between markers reach downstream untouched.
    let body = collect_bytes(&events);
    assert!(body.starts_with(b"prompt$ ls\n"));
    assert!(body.windows(b"file.txt".len()).any(|w| w == b"file.txt"));
}
