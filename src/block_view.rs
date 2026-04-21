/// TermView — block-based terminal widget.
///
/// Layout:
///   root (gtk4::Box, Vertical)
///     ├── block_scroll (ScrolledWindow)  — shown in block mode
///     │   └── block_list (gtk4::Box, Vertical)
///     │       ├── finished blocks …
///     │       └── active_block (gtk4::Box, Vertical)
///     │           ├── prompt_row (gtk4::Box, Horizontal)
///     │           │   └── prompt_label
///     │           ├── cmd_row (gtk4::Box, Horizontal)
///     │           │   └── cmd_label
///     │           └── live_view (gtk4::TextView) — live output
///     └── vte_box (gtk4::Box)            — shown in alt-screen mode
///         └── vte4::Terminal + Scrollbar
use gtk4::gdk::RGBA;
use gtk4::pango::FontDescription;
use gtk4::{glib, Orientation, ScrolledWindow, WrapMode};
use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

use crate::config::Config;
use crate::parser::{Parser, ParserEvent};
use crate::pty::OwnedPty;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn rgba_to_hex(c: &RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8,
    )
}

fn dim_rgba(c: &RGBA, alpha: f32) -> RGBA {
    RGBA::new(c.red(), c.green(), c.blue(), alpha)
}

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    // CSI sequence: skip until final byte 0x40..0x7e
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() { i += 1; }
                }
                b']' => {
                    // OSC sequence: skip until BEL or ST
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == 0x07 { i += 1; }
                }
                _ => {
                    // Other ESC sequence: skip ESC + one byte
                    i += 2;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ─── FinishedBlock ────────────────────────────────────────────────────────────

struct FinishedBlock {
    widget: gtk4::Box,
}

impl FinishedBlock {
    fn new(prompt: &str, cmd: &str, output: &str, exit_code: i32, _config: &Config) -> Self {

        // Outer frame
        let outer = gtk4::Box::new(Orientation::Vertical, 0);
        outer.add_css_class("block-finished");
        outer.set_margin_bottom(6);

        // Header row: prompt • command [exit badge]
        let header = gtk4::Box::new(Orientation::Horizontal, 8);
        header.add_css_class("block-header");
        header.set_margin_start(8);
        header.set_margin_end(8);
        header.set_margin_top(4);
        header.set_margin_bottom(4);

        let prompt_label = gtk4::Label::new(Some(prompt));
        prompt_label.add_css_class("block-prompt");
        prompt_label.set_xalign(0.0);
        prompt_label.set_selectable(true);

        let sep = gtk4::Label::new(Some("❯"));
        sep.add_css_class("block-chevron");

        let cmd_label = gtk4::Label::new(Some(if cmd.is_empty() { "(empty)" } else { cmd }));
        cmd_label.add_css_class("block-cmd");
        cmd_label.set_xalign(0.0);
        cmd_label.set_hexpand(true);
        cmd_label.set_selectable(true);

        header.append(&prompt_label);
        header.append(&sep);
        header.append(&cmd_label);

        if exit_code != 0 {
            let badge = gtk4::Label::new(Some(&format!(" {exit_code} ")));
            badge.add_css_class("block-exit-bad");
            header.append(&badge);
        }

        outer.append(&header);

        // Output area (only if there is output)
        if !output.is_empty() {
            let tv = gtk4::TextView::new();
            tv.set_editable(false);
            tv.set_cursor_visible(false);
            tv.set_wrap_mode(WrapMode::Char);
            tv.set_monospace(true);
            tv.set_margin_start(12);
            tv.set_margin_end(8);
            tv.set_margin_bottom(6);
            tv.add_css_class("block-output");
            let buf = tv.buffer();
            buf.set_text(output);
            outer.append(&tv);
        }

        // Separator line
        let sep_box = gtk4::Separator::new(Orientation::Horizontal);
        outer.append(&sep_box);

        FinishedBlock { widget: outer }
    }

    fn widget(&self) -> &gtk4::Box {
        &self.widget
    }
}

// ─── ActiveBlock ──────────────────────────────────────────────────────────────

struct ActiveBlock {
    widget: gtk4::Box,
    prompt_label: gtk4::Label,
    cmd_label: gtk4::Label,
    output_buf: gtk4::TextBuffer,
}

impl ActiveBlock {
    fn new() -> Self {
        let widget = gtk4::Box::new(Orientation::Vertical, 0);
        widget.add_css_class("block-active");
        widget.set_margin_bottom(2);

        // Prompt row
        let prompt_row = gtk4::Box::new(Orientation::Horizontal, 8);
        prompt_row.set_margin_start(8);
        prompt_row.set_margin_top(6);
        prompt_row.set_margin_bottom(2);

        let prompt_label = gtk4::Label::new(Some(""));
        prompt_label.add_css_class("block-prompt");
        prompt_label.set_xalign(0.0);
        prompt_row.append(&prompt_label);

        // Command row
        let cmd_row = gtk4::Box::new(Orientation::Horizontal, 8);
        cmd_row.set_margin_start(8);
        cmd_row.set_margin_bottom(4);

        let chevron = gtk4::Label::new(Some("❯"));
        chevron.add_css_class("block-chevron-active");
        let cmd_label = gtk4::Label::new(Some(""));
        cmd_label.add_css_class("block-cmd-active");
        cmd_label.set_xalign(0.0);
        cmd_label.set_hexpand(true);
        cmd_row.append(&chevron);
        cmd_row.append(&cmd_label);

        // Live output
        let tv = gtk4::TextView::new();
        tv.set_editable(false);
        tv.set_cursor_visible(false);
        tv.set_wrap_mode(WrapMode::Char);
        tv.set_monospace(true);
        tv.set_margin_start(12);
        tv.set_margin_end(8);
        tv.set_margin_bottom(6);
        tv.add_css_class("block-output");
        let output_buf = tv.buffer();

        widget.append(&prompt_row);
        widget.append(&cmd_row);
        widget.append(&tv);

        ActiveBlock { widget, prompt_label, cmd_label, output_buf }
    }

    fn set_prompt(&self, text: &str) {
        self.prompt_label.set_text(text);
    }

    fn set_cmd(&self, text: &str) {
        self.cmd_label.set_text(text);
    }

    fn append_output(&self, text: &str) {
        let mut end = self.output_buf.end_iter();
        self.output_buf.insert(&mut end, text);
    }

    fn output_text(&self) -> String {
        self.output_buf.text(
            &self.output_buf.start_iter(),
            &self.output_buf.end_iter(),
            false,
        ).to_string()
    }

    fn widget(&self) -> &gtk4::Box {
        &self.widget
    }
}

// ─── TermView state machine ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum BlockState {
    /// Waiting for first PromptStart or any bytes
    Idle,
    /// Between PromptStart and PromptEnd — collecting prompt text
    CollectingPrompt,
    /// Between PromptEnd and CommandStart — user is typing
    AwaitingCommand,
    /// Between CommandStart and CommandEnd — collecting output
    CollectingOutput,
    /// Inside full-screen app (vim/less/etc.)
    AltScreen,
}

// ─── TermView ─────────────────────────────────────────────────────────────────

pub struct TermView {
    root: gtk4::Box,
    block_scroll: ScrolledWindow,
    block_list: gtk4::Box,
    vte_box: gtk4::Box,
    vte: Terminal,
    active: Rc<RefCell<ActiveBlock>>,
    bstate: Rc<Cell<BlockState>>,
    prompt_buf: Rc<RefCell<String>>,
    cmd_buf: Rc<RefCell<String>>,
    pty: Rc<OwnedPty>,
    cwd_callbacks: Rc<RefCell<Vec<Box<dyn Fn(&str)>>>>,
    exited_callbacks: Rc<RefCell<Vec<Box<dyn Fn(i32)>>>>,
    config: Config,
}

impl TermView {
    pub fn new(config: &Config, shell_argv: &[String], cwd: Option<&str>) -> Self {
        // ── Build widget tree ──────────────────────────────────────────────
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.add_css_class("term-view-root");

        // Block list inside a scrolled window
        let block_list = gtk4::Box::new(Orientation::Vertical, 0);
        block_list.set_vexpand(true);

        let block_scroll = ScrolledWindow::new();
        block_scroll.set_hexpand(true);
        block_scroll.set_vexpand(true);
        block_scroll.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Automatic);
        block_scroll.set_child(Some(&block_list));

        // Active block always at bottom
        let active = Rc::new(RefCell::new(ActiveBlock::new()));
        block_list.append(active.borrow().widget());

        // VTE fallback for alt-screen mode
        let vte = build_vte(config);
        let vte_scrollbar = gtk4::Scrollbar::new(
            Orientation::Vertical,
            vte.vadjustment().as_ref(),
        );
        let vte_box = gtk4::Box::new(Orientation::Horizontal, 0);
        vte_box.set_hexpand(true);
        vte_box.set_vexpand(true);
        vte_box.append(&vte);
        vte_box.append(&vte_scrollbar);
        vte_box.set_visible(false); // hidden until alt-screen

        root.append(&block_scroll);
        root.append(&vte_box);

        // ── PTY ───────────────────────────────────────────────────────────
        let argv: Vec<&str> = shell_argv.iter().map(|s| s.as_str()).collect();
        let pty = Rc::new(
            OwnedPty::spawn(&argv, cwd, &[]).expect("PTY spawn failed"),
        );

        // ── Register CSS ──────────────────────────────────────────────────
        install_block_css(config);

        // ── Shared state ──────────────────────────────────────────────────
        let bstate = Rc::new(Cell::new(BlockState::Idle));
        let prompt_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cmd_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cwd_callbacks: Rc<RefCell<Vec<Box<dyn Fn(&str)>>>> = Rc::new(RefCell::new(vec![]));
        let exited_callbacks: Rc<RefCell<Vec<Box<dyn Fn(i32)>>>> = Rc::new(RefCell::new(vec![]));

        // ── Wire PTY → parser → block events ─────────────────────────────
        {
            let active_rc = active.clone();
            let bstate_rc = bstate.clone();
            let prompt_buf_rc = prompt_buf.clone();
            let cmd_buf_rc = cmd_buf.clone();
            let block_list_rc = block_list.clone();
            let block_scroll_rc = block_scroll.clone();
            let vte_for_alt = vte.clone();
            let vte_box_rc = vte_box.clone();
            let pty_for_resize = pty.clone();
            let cwd_cbs = cwd_callbacks.clone();
            let exited_cbs = exited_callbacks.clone();
            let config_for_cb = config.clone();
            let parser = Rc::new(RefCell::new(Parser::new()));

            pty.start_reader(
                move |data: Vec<u8>| {
                    log::debug!("PTY data: {} bytes, state={:?}", data.len(), bstate_rc.get());
                    if data.len() < 512 {
                        log::debug!("PTY hex: {:02x?}", &data);
                    }
                    let events = parser.borrow_mut().feed(&data);

                    for event in &events {
                        let state = bstate_rc.get();
                        log::debug!("ParserEvent: {:?} (state={:?})", event, state);
                        match event {
                            ParserEvent::Bytes(bytes) => {
                                let text = String::from_utf8_lossy(bytes).to_string();
                                match state {
                                    BlockState::CollectingPrompt => {
                                        prompt_buf_rc.borrow_mut().push_str(&text);
                                        // strip trailing whitespace/newlines and ANSI codes from prompt display
                                        let clean = strip_ansi(&text).trim_end().to_string();
                                        if !clean.is_empty() {
                                            active_rc.borrow().set_prompt(&clean);
                                        }
                                    }
                                    BlockState::AwaitingCommand => {
                                        // Shell's line editor sends the full line (prompt + input) with each keystroke.
                                        // When the prompt appears at the start, it's a fresh redraw - replace buffer.
                                        // Otherwise it's continuation - append to buffer.
                                        let stripped = strip_ansi(&text);

                                        let prompt_text = strip_ansi(&prompt_buf_rc.borrow());
                                        let prompt_clean = prompt_text.trim();

                                        // If this chunk starts with the prompt, it's a fresh redraw - replace buffer
                                        if !prompt_clean.is_empty() && stripped.starts_with(prompt_clean) {
                                            *cmd_buf_rc.borrow_mut() = stripped;
                                        } else {
                                            // No prompt at start means this is continuation input
                                            cmd_buf_rc.borrow_mut().push_str(&stripped);
                                        }

                                        // Now extract the command from the buffer
                                        let current_buf = cmd_buf_rc.borrow().clone();

                                        // Strip the prompt prefix to get just the command
                                        let cmd = if !prompt_clean.is_empty() {
                                            if let Some(after_prompt) = current_buf.strip_prefix(prompt_clean) {
                                                after_prompt.trim_start()
                                            } else if let Some(pos) = current_buf.find(prompt_clean) {
                                                current_buf[pos + prompt_clean.len()..].trim_start()
                                            } else {
                                                &current_buf
                                            }
                                        } else {
                                            &current_buf
                                        };

                                        let display = cmd.trim_end_matches('\n').trim_end();
                                        active_rc.borrow().set_cmd(display);
                                    }
                                    BlockState::CollectingOutput => {
                                        let clean = strip_ansi(&text);
                                        active_rc.borrow().append_output(&clean);
                                        // Auto-scroll to bottom
                                        let adj = block_scroll_rc.vadjustment();
                                        adj.set_value(adj.upper() - adj.page_size());
                                    }
                                    BlockState::AltScreen => {
                                        // Feed raw bytes directly to VTE
                                        vte_for_alt.feed(bytes);
                                    }
                                    BlockState::Idle => {
                                        // Bytes before first prompt — ignore (pre-prompt noise)
                                    }
                                }
                            }

                            ParserEvent::PromptStart => {
                                bstate_rc.set(BlockState::CollectingPrompt);
                                prompt_buf_rc.borrow_mut().clear();
                            }

                            ParserEvent::PromptEnd => {
                                bstate_rc.set(BlockState::AwaitingCommand);
                                cmd_buf_rc.borrow_mut().clear();
                                active_rc.borrow().set_cmd("");
                            }

                            ParserEvent::CommandStart => {
                                bstate_rc.set(BlockState::CollectingOutput);
                            }

                            ParserEvent::CommandEnd(code) => {
                                // Freeze the active block into a finished block
                                let prompt = strip_ansi(&prompt_buf_rc.borrow()).trim().to_string();

                                // Extract command from line-editor buffer
                                // Since we now clean cmd_buf in AwaitingCommand when \r is seen,
                                // cmd_buf should already contain only the current line.
                                let stripped_cmd_buf = strip_ansi(&cmd_buf_rc.borrow());
                                // Still handle \r just in case
                                let last_line = if let Some(idx) = stripped_cmd_buf.rfind('\r') {
                                    &stripped_cmd_buf[idx+1..]
                                } else {
                                    &stripped_cmd_buf
                                };
                                let cmd = if let Some(cmd_part) = last_line.strip_prefix(prompt.trim()) {
                                    cmd_part.trim().to_string()
                                } else {
                                    last_line.trim().to_string()
                                };
                                let output = active_rc.borrow().output_text();
                                let output_trimmed = output.trim_end().to_string();

                                let finished = FinishedBlock::new(
                                    &prompt, &cmd, &output_trimmed, *code, &config_for_cb,
                                );

                                // Insert before the active block (which is always last)
                                let active_widget = active_rc.borrow().widget().clone().upcast::<gtk4::Widget>();
                                finished.widget().insert_before(&block_list_rc, Some(&active_widget));

                                // Reset active block for next command
                                active_rc.borrow().set_prompt("");
                                active_rc.borrow().set_cmd("");
                                active_rc.borrow().output_buf.set_text("");

                                bstate_rc.set(BlockState::Idle);
                            }

                            ParserEvent::CwdUpdate(path) => {
                                for cb in cwd_cbs.borrow().iter() {
                                    cb(&path);
                                }
                            }

                            ParserEvent::AltScreenEnter => {
                                bstate_rc.set(BlockState::AltScreen);
                                // Hide block view and expand VTE to fill all space
                                block_scroll_rc.set_visible(false);
                                block_scroll_rc.set_vexpand(false);
                                vte_box_rc.set_vexpand(true);
                                vte_box_rc.set_visible(true);

                                // Resize PTY to match VTE widget size
                                let pty_resize = pty_for_resize.clone();
                                let vte_for_resize = vte_for_alt.clone();
                                glib::idle_add_local_once(move || {
                                    let width = vte_for_resize.allocated_width() as i64;
                                    let height = vte_for_resize.allocated_height() as i64;
                                    if width > 0 && height > 0 {
                                        let char_width = vte_for_resize.char_width();
                                        let char_height = vte_for_resize.char_height();
                                        if char_width > 0 && char_height > 0 {
                                            let cols = (width / char_width) as u16;
                                            let rows = (height / char_height) as u16;
                                            log::debug!("Resizing PTY to {}x{} (widget {}x{}, char {}x{})",
                                                cols, rows, width, height, char_width, char_height);
                                            pty_resize.resize(cols, rows);
                                        }
                                    }
                                });

                                vte_for_alt.grab_focus();
                            }

                            ParserEvent::AltScreenLeave => {
                                vte_box_rc.set_visible(false);
                                vte_box_rc.set_vexpand(false);
                                block_scroll_rc.set_vexpand(true);
                                block_scroll_rc.set_visible(true);
                                bstate_rc.set(BlockState::Idle);
                                // Reset active block ready for next prompt
                                active_rc.borrow().set_prompt("");
                                active_rc.borrow().set_cmd("");
                            }
                        }
                    }
                },
                move |exit_code| {
                    log::debug!("Shell exited with code {}", exit_code);
                    for cb in exited_cbs.borrow().iter() {
                        cb(exit_code);
                    }
                },
            );
        }

        // ── VTE is used as a display-only widget (fed via feed() in alt-screen mode)
        //    so we do NOT attach it to the PTY. Our reader thread handles all I/O.

        // ── Keyboard input → PTY ──────────────────────────────────────────
        {
            let pty_for_key = pty.clone();
            let vte_box_for_key = vte_box.clone();
            let bstate_for_key = bstate.clone();
            let key_ctrl = gtk4::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);

            key_ctrl.connect_key_pressed(move |_, keyval, _keycode, modifiers| {
                // All keyboard input goes through here to the PTY.
                // VTE has no PTY attached — it's display-only (fed via feed()).
                // Main app's key_controller on the window (also Capture phase) runs first
                // and will intercept keybindings before we get here.
                let ctrl = modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
                let bytes: Option<Vec<u8>> = match keyval {
                    v if v == gtk4::gdk::Key::Return || v == gtk4::gdk::Key::KP_Enter => {
                        Some(b"\r".to_vec())
                    }
                    v if v == gtk4::gdk::Key::BackSpace => Some(b"\x7f".to_vec()),
                    v if v == gtk4::gdk::Key::Tab => Some(b"\t".to_vec()),
                    v if v == gtk4::gdk::Key::Escape => Some(b"\x1b".to_vec()),
                    v if v == gtk4::gdk::Key::Up => Some(b"\x1b[A".to_vec()),
                    v if v == gtk4::gdk::Key::Down => Some(b"\x1b[B".to_vec()),
                    v if v == gtk4::gdk::Key::Right => Some(b"\x1b[C".to_vec()),
                    v if v == gtk4::gdk::Key::Left => Some(b"\x1b[D".to_vec()),
                    v if v == gtk4::gdk::Key::Home => Some(b"\x1b[H".to_vec()),
                    v if v == gtk4::gdk::Key::End => Some(b"\x1b[F".to_vec()),
                    v if v == gtk4::gdk::Key::Delete => Some(b"\x1b[3~".to_vec()),
                    v if v == gtk4::gdk::Key::Insert => Some(b"\x1b[2~".to_vec()),
                    v if v == gtk4::gdk::Key::Page_Up => Some(b"\x1b[5~".to_vec()),
                    v if v == gtk4::gdk::Key::Page_Down => Some(b"\x1b[6~".to_vec()),
                    v if v == gtk4::gdk::Key::F1 => Some(b"\x1bOP".to_vec()),
                    v if v == gtk4::gdk::Key::F2 => Some(b"\x1bOQ".to_vec()),
                    v if v == gtk4::gdk::Key::F3 => Some(b"\x1bOR".to_vec()),
                    v if v == gtk4::gdk::Key::F4 => Some(b"\x1bOS".to_vec()),
                    v if v == gtk4::gdk::Key::F5 => Some(b"\x1b[15~".to_vec()),
                    v if v == gtk4::gdk::Key::F6 => Some(b"\x1b[17~".to_vec()),
                    v if v == gtk4::gdk::Key::F7 => Some(b"\x1b[18~".to_vec()),
                    v if v == gtk4::gdk::Key::F8 => Some(b"\x1b[19~".to_vec()),
                    v if v == gtk4::gdk::Key::F9 => Some(b"\x1b[20~".to_vec()),
                    v if v == gtk4::gdk::Key::F10 => Some(b"\x1b[21~".to_vec()),
                    v if v == gtk4::gdk::Key::F11 => Some(b"\x1b[23~".to_vec()),
                    v if v == gtk4::gdk::Key::F12 => Some(b"\x1b[24~".to_vec()),
                    v if ctrl => {
                        if let Some(ch) = v.to_unicode() {
                            let ctrl_byte = (ch as u8).wrapping_sub(b'`');
                            if ctrl_byte < 32 {
                                Some(vec![ctrl_byte])
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    v => {
                        if let Some(ch) = v.to_unicode() {
                            let mut buf = [0u8; 4];
                            Some(ch.encode_utf8(&mut buf).as_bytes().to_vec())
                        } else {
                            None
                        }
                    }
                };
                if let Some(data) = bytes {
                    pty_for_key.write_bytes(&data);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            root.add_controller(key_ctrl);
            root.set_focusable(true);
        }

        TermView {
            root,
            block_scroll,
            block_list,
            vte_box,
            vte,
            active,
            bstate,
            prompt_buf,
            cmd_buf,
            pty,
            cwd_callbacks,
            exited_callbacks,
            config: config.clone(),
        }
    }

    /// Root GTK widget to embed in the notebook page.
    pub fn widget(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }

    /// Send key bytes into the PTY (user input).
    pub fn write_input(&self, data: &[u8]) {
        self.pty.write_bytes(data);
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) {
        self.pty.resize(cols, rows);
    }

    /// Kill the child process.
    pub fn kill(&self) {
        self.pty.kill();
    }

    pub fn pid_i32(&self) -> i32 {
        self.pty.pid_i32()
    }

    pub fn vte(&self) -> &Terminal {
        &self.vte
    }

    pub fn grab_focus(&self) {
        if self.vte_box.is_visible() {
            self.vte.grab_focus();
        } else {
            self.root.grab_focus();
        }
    }

    pub fn connect_cwd_changed<F: Fn(&str) + 'static>(&self, f: F) {
        self.cwd_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_exited<F: Fn(i32) + 'static>(&self, f: F) {
        self.exited_callbacks.borrow_mut().push(Box::new(f));
    }

    /// Apply updated theme colors to the block widgets.
    pub fn apply_theme(&self) {
        install_block_css(&self.config);
    }
}

// ─── VTE builder ─────────────────────────────────────────────────────────────

fn build_vte(config: &Config) -> Terminal {
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        .cursor_blink_mode(CursorBlinkMode::Off)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();
    terminal.set_mouse_autohide(true);
    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(Some(&config.foreground), Some(&config.background), &palette_refs);
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));
    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));
    terminal
}

// ─── CSS ──────────────────────────────────────────────────────────────────────

fn install_block_css(config: &Config) {
    let fg = &config.foreground;
    let bg = &config.background;
    let bg_hex = rgba_to_hex(bg);
    let fg_hex = rgba_to_hex(fg);
    // Slightly lighter bg for header
    let header_bg = format!(
        "rgba({},{},{},0.08)",
        (fg.red() * 255.0) as u8,
        (fg.green() * 255.0) as u8,
        (fg.blue() * 255.0) as u8,
    );
    let dim_fg = format!(
        "rgba({},{},{},0.55)",
        (fg.red() * 255.0) as u8,
        (fg.green() * 255.0) as u8,
        (fg.blue() * 255.0) as u8,
    );
    // Accent color for active chevron (use palette color 2 = green-ish)
    let accent = rgba_to_hex(&config.palette[2]);

    let fg_r = (fg.red() * 255.0) as u8;
    let fg_g = (fg.green() * 255.0) as u8;
    let fg_b = (fg.blue() * 255.0) as u8;
    let css = format!(
        r#"
        .block-finished {{
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.12);
            border-radius: 6px;
            margin: 4px 6px;
            background-color: {bg_hex};
        }}
        .block-active {{
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.25);
            border-left: 3px solid {accent};
            border-radius: 6px;
            margin: 4px 6px;
            background-color: {bg_hex};
        }}
        .block-header {{
            background-color: {header_bg};
            border-radius: 5px 5px 0 0;
        }}
        .block-prompt {{
            color: {dim_fg};
            font-size: 0.82em;
        }}
        .block-chevron {{
            color: {dim_fg};
            font-weight: bold;
        }}
        .block-chevron-active {{
            color: {accent};
            font-weight: bold;
        }}
        .block-cmd {{
            color: {fg_hex};
            font-family: monospace;
        }}
        .block-cmd-active {{
            color: {fg_hex};
            font-family: monospace;
            font-weight: bold;
        }}
        .block-exit-bad {{
            color: #ff5555;
            background-color: rgba(255,85,85,0.18);
            border-radius: 3px;
            font-size: 0.8em;
        }}
        .block-output {{
            background-color: {bg_hex};
            color: {fg_hex};
            font-family: monospace;
        }}
        "#,
    );

    let provider = gtk4::CssProvider::new();
    provider.load_from_data(&css);
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().unwrap(),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
