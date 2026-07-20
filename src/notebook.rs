//! Executable `.jtnb.md` notebooks for the native GTK4 application.
//!
//! A notebook is ordinary Markdown with fenced shell cells. Shell source is
//! executed in an isolated child process and never injected into a live terminal.
//! Explicit fences (`bash`, `sh`, `zsh`, `fish`, `pwsh`) select that interpreter;
//! `shell` and unlabeled fences use the caller-provided shell argv verbatim.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Maximum combined stdout/stderr retained for one cell run.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(40);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Code { lang: String, src: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fence {
    marker: u8,
    length: usize,
}

fn leading_indent(line: &str) -> Option<&str> {
    let stripped = line.trim_start_matches(' ');
    (line.len() - stripped.len() <= 3).then_some(stripped)
}

fn opening_fence(line: &str) -> Option<(Fence, &str)> {
    let stripped = leading_indent(line)?;
    let marker = *stripped.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let length = stripped
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    if length < 3 {
        return None;
    }
    let info = stripped[length..].trim();
    if marker == b'`' && info.as_bytes().contains(&b'`') {
        return None;
    }
    Some((Fence { marker, length }, info))
}

fn is_closing_fence(line: &str, opening: Fence) -> bool {
    let Some(stripped) = leading_indent(line) else {
        return false;
    };
    let marker_count = stripped
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == opening.marker)
        .count();
    marker_count >= opening.length && stripped[marker_count..].trim().is_empty()
}

fn push_text(segments: &mut Vec<Segment>, text: String) {
    if text.is_empty() {
        return;
    }
    if let Some(Segment::Text(previous)) = segments.last_mut() {
        previous.push_str(&text);
    } else {
        segments.push(Segment::Text(text));
    }
}

/// Split Markdown into prose and fenced code segments.
///
/// Both backtick and tilde fences are supported, including longer fences and
/// CommonMark's allowance of up to three leading spaces. An unfinished fence is
/// preserved as prose so a partially edited notebook never silently loses text.
pub fn parse_segments(input: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut lines = input.lines();

    while let Some(line) = lines.next() {
        let Some((fence, info)) = opening_fence(line) else {
            text.push_str(line);
            text.push('\n');
            continue;
        };

        push_text(&mut segments, std::mem::take(&mut text));
        let mut source = String::new();
        let mut closed = false;
        for inner in lines.by_ref() {
            if is_closing_fence(inner, fence) {
                closed = true;
                break;
            }
            source.push_str(inner);
            source.push('\n');
        }

        if closed {
            if source.ends_with('\n') {
                source.pop();
            }
            segments.push(Segment::Code {
                lang: info.to_owned(),
                src: source,
            });
        } else {
            text.push_str(line);
            text.push('\n');
            text.push_str(&source);
        }
    }

    push_text(&mut segments, text);
    segments
}

fn escape_pango(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn wrap_marker(text: &str, marker: &str, open: &str, close: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find(marker) else {
            rendered.push_str(rest);
            return rendered;
        };
        rendered.push_str(&rest[..start]);
        let after = &rest[start + marker.len()..];
        let Some(end) = after.find(marker) else {
            rendered.push_str(&rest[start..]);
            return rendered;
        };
        rendered.push_str(open);
        rendered.push_str(&after[..end]);
        rendered.push_str(close);
        rest = &after[end + marker.len()..];
    }
}

fn render_inline(text: &str) -> String {
    let escaped = escape_pango(text);
    let code = wrap_marker(&escaped, "`", "<tt>", "</tt>");
    let bold = wrap_marker(&code, "**", "<b>", "</b>");
    wrap_marker(&bold, "*", "<i>", "</i>")
}

/// Render the deliberately small Markdown subset used by notebook prose.
pub fn render_text_to_pango(text: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            rendered.push('\n');
            continue;
        }
        let (open, body, close) = if let Some(body) = line.strip_prefix("### ") {
            ("<span weight=\"bold\" size=\"large\">", body, "</span>")
        } else if let Some(body) = line.strip_prefix("## ") {
            ("<span weight=\"bold\" size=\"x-large\">", body, "</span>")
        } else if let Some(body) = line.strip_prefix("# ") {
            ("<span weight=\"bold\" size=\"xx-large\">", body, "</span>")
        } else {
            ("", line, "")
        };
        rendered.push_str(open);
        rendered.push_str(&render_inline(body));
        rendered.push_str(close);
        rendered.push('\n');
    }
    rendered
}

pub fn is_notebook_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jtnb.md"))
}

#[derive(Debug)]
pub enum NotebookError {
    InvalidExtension(PathBuf),
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for NotebookError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidExtension(path) => {
                write!(formatter, "not a .jtnb.md notebook: {}", path.display())
            }
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "could not read notebook {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for NotebookError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::InvalidExtension(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
struct CommandSpec {
    argv: Vec<String>,
    source: String,
    cwd: PathBuf,
}

fn language_name(info: &str) -> &str {
    info.split_whitespace().next().unwrap_or("")
}

fn shell_argv_for_info(info: &str, configured_shell: &[String]) -> Option<Vec<String>> {
    let language = language_name(info).to_ascii_lowercase();
    match language.as_str() {
        "" | "shell" => (!configured_shell.is_empty()).then(|| configured_shell.to_vec()),
        "bash" | "sh" | "zsh" | "fish" => Some(vec![language]),
        "pwsh" => Some(vec![
            "pwsh".to_owned(),
            "-NoLogo".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "-".to_owned(),
        ]),
        "powershell" => Some(vec![
            "powershell".to_owned(),
            "-NoLogo".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "-".to_owned(),
        ]),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CellOutcome {
    Exited(i32),
    Cancelled,
    Failed(String),
}

impl CellOutcome {
    fn failed(&self) -> bool {
        !matches!(self, Self::Exited(0))
    }
}

enum WorkerEvent {
    /// The child has been spawned; carries its process-group id. Emitted once,
    /// before any output, so an observer can read the group over the ordered
    /// channel without racing the worker resetting `pgid` to 0 on completion.
    Started(i32),
    Output(OutputStream, Vec<u8>),
    Done(CellOutcome),
}

struct CellHandle {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
    /// Kept independently of `Child`: the group can still contain descendants
    /// after the interpreter itself has exited and been reaped.
    pgid: Arc<AtomicI32>,
}

impl CellHandle {
    fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            pgid: Arc::new(AtomicI32::new(0)),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        signal_process_group(self.pgid.load(Ordering::SeqCst));
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                terminate_child_group(child);
            }
        }
    }
}

fn signal_process_group(pgid: i32) {
    #[cfg(unix)]
    if pgid > 0 {
        // SAFETY: a negative PID targets the process group created for this
        // cell. Failure is harmless when the group has already exited.
        unsafe {
            nix::libc::kill(-pgid, nix::libc::SIGKILL);
        }
    }
}

fn terminate_child_group(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            // Each notebook cell is placed in its own process group. Killing the
            // group prevents descendants such as `sleep` from surviving Stop or
            // dialog close after the shell itself exits.
            unsafe {
                nix::libc::kill(-pid, nix::libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

fn wait_for_shared_child(child: &Arc<Mutex<Option<Child>>>) -> std::io::Result<i32> {
    loop {
        let exit = {
            let mut guard = child
                .lock()
                .map_err(|_| std::io::Error::other("child handle mutex poisoned"))?;
            let process = guard
                .as_mut()
                .ok_or_else(|| std::io::Error::other("child handle missing before exit"))?;
            match process.try_wait()? {
                Some(status) => {
                    let code = status.code().unwrap_or(-1);
                    guard.take();
                    Some(code)
                }
                None => None,
            }
        };
        if let Some(code) = exit {
            return Ok(code);
        }
        std::thread::sleep(CHILD_POLL_INTERVAL);
    }
}

fn spawn_cell_worker(spec: CommandSpec, handle: &CellHandle) -> mpsc::Receiver<WorkerEvent> {
    // Bound queued output as well as the rendered buffers: a command that
    // writes faster than GTK can paint applies backpressure instead of growing
    // an unbounded cross-thread queue.
    let (sender, receiver) = mpsc::sync_channel(64);
    let child_slot = handle.child.clone();
    let cancelled = handle.cancelled.clone();
    let pgid = handle.pgid.clone();

    std::thread::spawn(move || {
        let host_bridge = crate::host::is_flatpak();
        let cwd_for_bridge = spec.cwd.to_string_lossy().into_owned();
        let executable_argv =
            crate::host::wrap_argv(&spec.argv, Some(cwd_for_bridge.as_str()), &[]);
        let Some((program, arguments)) = executable_argv.split_first() else {
            let _ = sender.send(WorkerEvent::Done(CellOutcome::Failed(
                "no shell executable configured".to_owned(),
            )));
            return;
        };

        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !host_bridge {
            command.current_dir(&spec.cwd);
        }
        #[cfg(unix)]
        command.process_group(0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(WorkerEvent::Done(CellOutcome::Failed(format!(
                    "spawn failed: {error}"
                ))));
                return;
            }
        };
        if let Ok(id) = i32::try_from(child.id()) {
            pgid.store(id, Ordering::SeqCst);
            let _ = sender.send(WorkerEvent::Started(id));
        }

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        match child_slot.lock() {
            Ok(mut guard) => {
                *guard = Some(child);
                if cancelled.load(Ordering::SeqCst) {
                    if let Some(child) = guard.as_mut() {
                        terminate_child_group(child);
                    }
                }
            }
            Err(_) => {
                terminate_child_group(&mut child);
                let _ = child.wait();
                let _ = sender.send(WorkerEvent::Done(CellOutcome::Failed(
                    "child handle mutex poisoned".to_owned(),
                )));
                return;
            }
        }

        let source = spec.source;
        let stdin_thread = stdin.map(|mut input| {
            std::thread::spawn(move || {
                let _ = input.write_all(source.as_bytes());
                if !source.ends_with('\n') {
                    let _ = input.write_all(b"\n");
                }
            })
        });

        let stdout_sender = sender.clone();
        let stdout_thread = stdout.map(|mut output| {
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    match output.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            if stdout_sender
                                .send(WorkerEvent::Output(
                                    OutputStream::Stdout,
                                    buffer[..count].to_vec(),
                                ))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        });

        let stderr_sender = sender.clone();
        let stderr_thread = stderr.map(|mut output| {
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    match output.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            if stderr_sender
                                .send(WorkerEvent::Output(
                                    OutputStream::Stderr,
                                    buffer[..count].to_vec(),
                                ))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        });

        let exit = wait_for_shared_child(&child_slot);
        // A shell can exit while a background child keeps its output pipes and
        // process group alive. End that group before joining the pipe readers,
        // both to bound completion time and to prevent orphaned notebook jobs.
        signal_process_group(pgid.load(Ordering::SeqCst));
        if let Some(thread) = stdin_thread {
            let _ = thread.join();
        }
        if let Some(thread) = stdout_thread {
            let _ = thread.join();
        }
        if let Some(thread) = stderr_thread {
            let _ = thread.join();
        }

        let outcome = match exit {
            Ok(_) if cancelled.load(Ordering::SeqCst) => CellOutcome::Cancelled,
            Ok(code) => CellOutcome::Exited(code),
            Err(error) => CellOutcome::Failed(format!("wait failed: {error}")),
        };
        pgid.store(0, Ordering::SeqCst);
        let _ = sender.send(WorkerEvent::Done(outcome));
    });

    receiver
}

struct OutputPane {
    root: gtk4::Box,
    buffer: gtk4::TextBuffer,
    scroll: gtk4::ScrolledWindow,
}

impl OutputPane {
    fn new(title: &str, is_error: bool) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 3);
        root.set_visible(false);

        let label = gtk4::Label::new(Some(title));
        label.set_xalign(0.0);
        label.add_css_class("dim-label");
        if is_error {
            label.add_css_class("error");
        }
        root.append(&label);

        let buffer = gtk4::TextBuffer::new(None);
        let view = gtk4::TextView::with_buffer(&buffer);
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk4::WrapMode::WordChar);
        view.add_css_class("notebook-output");
        let scroll = gtk4::ScrolledWindow::builder()
            .hexpand(true)
            .max_content_height(260)
            .child(&view)
            .build();
        scroll.set_propagate_natural_height(true);
        root.append(&scroll);

        Self {
            root,
            buffer,
            scroll,
        }
    }

    fn clear(&self) {
        self.buffer.set_text("");
        self.root.set_visible(false);
    }

    fn append(&self, text: &str) {
        self.root.set_visible(true);
        let mut end = self.buffer.end_iter();
        self.buffer.insert(&mut end, text);
        let adjustment = self.scroll.vadjustment();
        adjustment.set_value(adjustment.upper());
    }
}

type Completion = Box<dyn FnOnce(CellOutcome)>;

struct CellController {
    index: usize,
    frame: gtk4::Frame,
    command: Option<CommandSpec>,
    run_button: gtk4::Button,
    stop_button: gtk4::Button,
    stdout: OutputPane,
    stderr: OutputPane,
    status: gtk4::Label,
    active: RefCell<Option<Rc<CellHandle>>>,
    externally_locked: Cell<bool>,
}

impl CellController {
    fn new(
        index: usize,
        info: &str,
        source: &str,
        configured_shell: &[String],
        cwd: &Path,
    ) -> Rc<Self> {
        let argv = shell_argv_for_info(info, configured_shell);
        let command = argv.map(|argv| CommandSpec {
            argv,
            source: source.to_owned(),
            cwd: cwd.to_path_buf(),
        });

        let frame = gtk4::Frame::new(None);
        frame.add_css_class("card");
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        body.set_margin_top(8);
        body.set_margin_bottom(8);
        body.set_margin_start(8);
        body.set_margin_end(8);

        let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let language = language_name(info);
        let language_label = gtk4::Label::new(Some(if language.is_empty() {
            "shell"
        } else {
            language
        }));
        language_label.set_xalign(0.0);
        language_label.set_hexpand(true);
        language_label.add_css_class("dim-label");
        toolbar.append(&language_label);

        let copy_button = gtk4::Button::with_label("Copy");
        copy_button.add_css_class("flat");
        let source_for_copy = source.to_owned();
        copy_button.connect_clicked(move |_| {
            if let Some(display) = gtk4::gdk::Display::default() {
                display.clipboard().set_text(&source_for_copy);
            }
        });
        toolbar.append(&copy_button);

        let run_button = gtk4::Button::with_label("Run");
        let stop_button = gtk4::Button::with_label("Stop");
        stop_button.set_sensitive(false);
        if command.is_some() {
            run_button.add_css_class("suggested-action");
        } else {
            run_button.set_sensitive(false);
            run_button.set_tooltip_text(Some(
                "Only shell fences are executable; use bash, sh, zsh, fish, pwsh, shell, or no label",
            ));
        }
        toolbar.append(&run_button);
        toolbar.append(&stop_button);
        body.append(&toolbar);

        let source_buffer = gtk4::TextBuffer::new(None);
        source_buffer.set_text(source);
        let source_view = gtk4::TextView::with_buffer(&source_buffer);
        source_view.set_editable(false);
        source_view.set_cursor_visible(false);
        source_view.set_monospace(true);
        source_view.set_wrap_mode(gtk4::WrapMode::None);
        source_view.add_css_class("notebook-source");
        let source_scroll = gtk4::ScrolledWindow::builder()
            .hexpand(true)
            .max_content_height(220)
            .child(&source_view)
            .build();
        source_scroll.set_propagate_natural_height(true);
        body.append(&source_scroll);

        let stdout = OutputPane::new("stdout", false);
        body.append(&stdout.root);
        let stderr = OutputPane::new("stderr", true);
        body.append(&stderr.root);

        let status = gtk4::Label::new(None);
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_visible(false);
        body.append(&status);
        frame.set_child(Some(&body));

        let cell = Rc::new(Self {
            index,
            frame,
            command,
            run_button,
            stop_button,
            stdout,
            stderr,
            status,
            active: RefCell::new(None),
            externally_locked: Cell::new(false),
        });

        let weak = Rc::downgrade(&cell);
        cell.run_button.connect_clicked(move |_| {
            if let Some(cell) = weak.upgrade() {
                let _ = cell.run(None);
            }
        });
        let weak = Rc::downgrade(&cell);
        cell.stop_button.connect_clicked(move |_| {
            if let Some(cell) = weak.upgrade() {
                cell.cancel();
            }
        });
        cell
    }

    fn runnable(&self) -> bool {
        self.command.is_some()
    }

    fn is_running(&self) -> bool {
        self.active.borrow().is_some()
    }

    fn set_external_lock(&self, locked: bool) {
        self.externally_locked.set(locked);
        self.sync_buttons();
    }

    fn sync_buttons(&self) {
        let running = self.is_running();
        self.run_button
            .set_sensitive(self.runnable() && !running && !self.externally_locked.get());
        self.stop_button.set_sensitive(running);
    }

    fn cancel(&self) {
        if let Some(handle) = self.active.borrow().as_ref() {
            handle.cancel();
            self.status.set_text("Cancelling…");
            self.stop_button.set_sensitive(false);
        }
    }

    fn append_output(&self, stream: OutputStream, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        match stream {
            OutputStream::Stdout => self.stdout.append(&text),
            OutputStream::Stderr => self.stderr.append(&text),
        }
    }

    fn finish(&self, outcome: &CellOutcome) {
        self.status.remove_css_class("error");
        self.status.remove_css_class("warning");
        match outcome {
            CellOutcome::Exited(code) => {
                self.status.set_text(&format!("exit {code}"));
                if *code != 0 {
                    self.status.add_css_class("error");
                }
            }
            CellOutcome::Cancelled => {
                self.status.set_text("cancelled");
                self.status.add_css_class("warning");
            }
            CellOutcome::Failed(error) => {
                self.status.set_text(&format!("failed: {error}"));
                self.status.add_css_class("error");
            }
        }
        self.sync_buttons();
    }

    fn run(self: &Rc<Self>, completion: Option<Completion>) -> bool {
        let Some(command) = self.command.clone() else {
            return false;
        };
        if self.is_running() {
            return false;
        }

        self.stdout.clear();
        self.stderr.clear();
        self.status.set_visible(true);
        self.status.set_text("Running…");
        self.status.remove_css_class("error");
        self.status.remove_css_class("warning");

        let handle = Rc::new(CellHandle::new());
        *self.active.borrow_mut() = Some(handle.clone());
        self.sync_buttons();
        let receiver = spawn_cell_worker(command, &handle);
        let weak_cell = Rc::downgrade(self);
        let mut completion = completion;
        let mut bytes_seen = 0usize;
        let mut truncated = false;

        glib::timeout_add_local(OUTPUT_POLL_INTERVAL, move || {
            let Some(cell) = weak_cell.upgrade() else {
                handle.cancel();
                return glib::ControlFlow::Break;
            };

            loop {
                match receiver.try_recv() {
                    Ok(WorkerEvent::Started(_)) => {}
                    Ok(WorkerEvent::Output(stream, bytes)) => {
                        if bytes_seen >= MAX_OUTPUT_BYTES {
                            if !truncated {
                                truncated = true;
                                cell.stderr.append("\n[output truncated]\n");
                            }
                            continue;
                        }
                        let remaining = MAX_OUTPUT_BYTES - bytes_seen;
                        let count = bytes.len().min(remaining);
                        bytes_seen += count;
                        cell.append_output(stream, &bytes[..count]);
                    }
                    Ok(WorkerEvent::Done(outcome)) => {
                        let is_current = cell
                            .active
                            .borrow()
                            .as_ref()
                            .is_some_and(|active| Rc::ptr_eq(active, &handle));
                        if is_current {
                            cell.active.borrow_mut().take();
                        }
                        cell.finish(&outcome);
                        if let Some(callback) = completion.take() {
                            callback(outcome);
                        }
                        return glib::ControlFlow::Break;
                    }
                    Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        let outcome = CellOutcome::Failed("worker disconnected".to_owned());
                        cell.active.borrow_mut().take();
                        cell.finish(&outcome);
                        if let Some(callback) = completion.take() {
                            callback(outcome);
                        }
                        return glib::ControlFlow::Break;
                    }
                }
            }
        });
        true
    }
}

#[derive(Default)]
struct RunAllStats {
    total: usize,
    finished: usize,
    failed: usize,
}

struct NotebookRuntime {
    cells: Vec<Rc<CellController>>,
    queue: RefCell<VecDeque<usize>>,
    run_all_active: Cell<bool>,
    closed: Cell<bool>,
    stats: RefCell<RunAllStats>,
    run_all_button: gtk4::Button,
    stop_all_button: gtk4::Button,
    status: gtk4::Label,
}

impl NotebookRuntime {
    fn start_run_all(self: &Rc<Self>) {
        if self.closed.get() || self.run_all_active.get() {
            return;
        }
        if self.cells.iter().any(|cell| cell.is_running()) {
            self.status
                .set_text("Wait for individually running cells, or stop them first.");
            self.status.add_css_class("warning");
            self.status.set_visible(true);
            return;
        }

        let queue: VecDeque<usize> = self
            .cells
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| cell.runnable().then_some(index))
            .collect();
        if queue.is_empty() {
            self.status.set_text("No runnable shell cells.");
            self.status.set_visible(true);
            return;
        }

        *self.stats.borrow_mut() = RunAllStats {
            total: queue.len(),
            ..RunAllStats::default()
        };
        *self.queue.borrow_mut() = queue;
        self.run_all_active.set(true);
        self.run_all_button.set_sensitive(false);
        self.stop_all_button.set_sensitive(true);
        self.status.remove_css_class("error");
        self.status.remove_css_class("warning");
        self.status.set_visible(true);
        for cell in &self.cells {
            cell.set_external_lock(true);
        }
        self.run_next();
    }

    fn run_next(self: &Rc<Self>) {
        if !self.run_all_active.get() || self.closed.get() {
            return;
        }
        let Some(index) = self.queue.borrow_mut().pop_front() else {
            self.finish_run_all();
            return;
        };

        let stats = self.stats.borrow();
        self.status.set_text(&format!(
            "Running cell {} of {}…",
            stats.finished + 1,
            stats.total
        ));
        drop(stats);

        let cell = self.cells[index].clone();
        let weak_runtime: Weak<Self> = Rc::downgrade(self);
        if !cell.run(Some(Box::new(move |outcome| {
            if let Some(runtime) = weak_runtime.upgrade() {
                runtime.cell_finished(outcome);
            }
        }))) {
            self.cell_finished(CellOutcome::Failed(format!(
                "cell {} could not start",
                cell.index + 1
            )));
        }
    }

    fn cell_finished(self: &Rc<Self>, outcome: CellOutcome) {
        if !self.run_all_active.get() {
            return;
        }
        {
            let mut stats = self.stats.borrow_mut();
            stats.finished += 1;
            if outcome.failed() {
                stats.failed += 1;
            }
        }
        self.run_next();
    }

    fn finish_run_all(&self) {
        self.run_all_active.set(false);
        self.run_all_button.set_sensitive(true);
        self.stop_all_button.set_sensitive(false);
        for cell in &self.cells {
            cell.set_external_lock(false);
        }

        let stats = self.stats.borrow();
        self.status.remove_css_class("warning");
        if stats.failed == 0 {
            self.status
                .set_text(&format!("Run All finished: {} cell(s).", stats.finished));
            self.status.remove_css_class("error");
        } else {
            self.status.set_text(&format!(
                "Run All finished: {} cell(s), {} failed.",
                stats.finished, stats.failed
            ));
            self.status.add_css_class("error");
        }
    }

    fn stop_all(&self) {
        let was_run_all = self.run_all_active.replace(false);
        self.queue.borrow_mut().clear();
        for cell in &self.cells {
            cell.cancel();
            cell.set_external_lock(false);
        }
        self.run_all_button.set_sensitive(!self.closed.get());
        self.stop_all_button.set_sensitive(false);
        if was_run_all && !self.closed.get() {
            self.status.set_text("Run All cancelled.");
            self.status.add_css_class("warning");
        }
    }

    fn shutdown(&self) {
        self.closed.set(true);
        self.stop_all();
    }
}

/// Handle for a presented native GTK notebook dialog.
///
/// Dropping this Rust handle does not close the dialog; the presented GTK object
/// owns its UI lifetime. Closing the dialog always cancels and reaps active cells.
#[derive(Clone)]
pub struct NotebookDialog {
    dialog: adw::Dialog,
}

impl NotebookDialog {
    /// Read and present a `.jtnb.md` notebook.
    ///
    /// `shell_argv` is copied and used verbatim for `shell` or unlabeled cells.
    /// Explicit language fences select their named interpreter. `cwd`, when
    /// absent, defaults to the notebook file's directory.
    pub fn open(
        parent: &adw::ApplicationWindow,
        path: impl AsRef<Path>,
        shell_argv: &[String],
        cwd: Option<&Path>,
    ) -> Result<Self, NotebookError> {
        let path = path.as_ref().to_path_buf();
        if !is_notebook_path(&path) {
            return Err(NotebookError::InvalidExtension(path));
        }
        let contents = std::fs::read_to_string(&path).map_err(|source| NotebookError::Read {
            path: path.clone(),
            source,
        })?;
        let segments = parse_segments(&contents);
        let working_directory = cwd.map(Path::to_path_buf).unwrap_or_else(|| {
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        });

        let title = path
            .file_name()
            .map(|name| format!("Notebook: {}", name.to_string_lossy()))
            .unwrap_or_else(|| format!("Notebook: {}", path.display()));
        let dialog = adw::Dialog::builder()
            .title(&title)
            .content_width(900)
            .content_height(700)
            .build();

        let header = adw::HeaderBar::new();
        let stop_all_button = gtk4::Button::with_label("Stop All");
        stop_all_button.set_sensitive(false);
        let run_all_button = gtk4::Button::with_label("Run All");
        run_all_button.add_css_class("suggested-action");
        header.pack_end(&stop_all_button);
        header.pack_end(&run_all_button);

        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(16);
        content.set_margin_end(16);

        let run_all_status = gtk4::Label::new(None);
        run_all_status.set_xalign(0.0);
        run_all_status.add_css_class("dim-label");
        run_all_status.set_visible(false);
        content.append(&run_all_status);

        let mut cells = Vec::new();
        for segment in segments {
            match segment {
                Segment::Text(text) => {
                    let label = gtk4::Label::new(None);
                    label.set_use_markup(true);
                    label.set_markup(&render_text_to_pango(&text));
                    label.set_wrap(true);
                    label.set_xalign(0.0);
                    label.set_halign(gtk4::Align::Fill);
                    label.set_selectable(true);
                    content.append(&label);
                }
                Segment::Code { lang, src } => {
                    let cell = CellController::new(
                        cells.len(),
                        &lang,
                        &src,
                        shell_argv,
                        &working_directory,
                    );
                    content.append(&cell.frame);
                    cells.push(cell);
                }
            }
        }

        let shell_display = if shell_argv.is_empty() {
            "(none)".to_owned()
        } else {
            shell_argv.join(" ")
        };
        let footer = gtk4::Label::new(Some(&format!(
            "Cells run in isolated process groups with cwd {}. `shell` and unlabeled cells use: {}. Source is provided on stdin; active terminals are never modified.",
            working_directory.display(),
            shell_display
        )));
        footer.set_wrap(true);
        footer.set_xalign(0.0);
        footer.set_selectable(true);
        footer.add_css_class("dim-label");
        content.append(&footer);

        let runtime = Rc::new(NotebookRuntime {
            cells,
            queue: RefCell::new(VecDeque::new()),
            run_all_active: Cell::new(false),
            closed: Cell::new(false),
            stats: RefCell::new(RunAllStats::default()),
            run_all_button: run_all_button.clone(),
            stop_all_button: stop_all_button.clone(),
            status: run_all_status,
        });
        run_all_button.set_sensitive(runtime.cells.iter().any(|cell| cell.runnable()));

        let weak_runtime = Rc::downgrade(&runtime);
        run_all_button.connect_clicked(move |_| {
            if let Some(runtime) = weak_runtime.upgrade() {
                runtime.start_run_all();
            }
        });
        let weak_runtime = Rc::downgrade(&runtime);
        stop_all_button.connect_clicked(move |_| {
            if let Some(runtime) = weak_runtime.upgrade() {
                runtime.stop_all();
            }
        });
        dialog.connect_closed(move |_| runtime.shutdown());

        let scroll = gtk4::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scroll));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(parent));

        Ok(Self { dialog })
    }

    pub fn dialog(&self) -> &adw::Dialog {
        &self.dialog
    }

    pub fn close(&self) {
        self.dialog.force_close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_shell_fences() {
        let markdown = "Intro\n```bash\necho hi\n```\nMiddle\n```\nls\n```\nTail";
        let segments = parse_segments(markdown);
        assert_eq!(segments.len(), 5);
        assert!(matches!(segments[0], Segment::Text(_)));
        assert_eq!(
            segments[1],
            Segment::Code {
                lang: "bash".to_owned(),
                src: "echo hi".to_owned()
            }
        );
        assert_eq!(
            segments[3],
            Segment::Code {
                lang: "".to_owned(),
                src: "ls".to_owned()
            }
        );
    }

    #[test]
    fn supports_tildes_indent_and_long_fences() {
        let markdown = "  ~~~sh\necho tilde\n  ~~~\n````bash\necho ``` literal\n````\n";
        let segments = parse_segments(markdown);
        assert_eq!(segments.len(), 2);
        assert!(matches!(
            &segments[0],
            Segment::Code { lang, src } if lang == "sh" && src == "echo tilde"
        ));
        assert!(matches!(
            &segments[1],
            Segment::Code { lang, src } if lang == "bash" && src == "echo ``` literal"
        ));
    }

    #[test]
    fn unfinished_fence_remains_visible_as_text() {
        let segments = parse_segments("before\n```bash\necho incomplete\n");
        assert!(segments
            .iter()
            .all(|segment| matches!(segment, Segment::Text(_))));
        let joined = segments
            .into_iter()
            .map(|segment| match segment {
                Segment::Text(text) => text,
                Segment::Code { .. } => unreachable!(),
            })
            .collect::<String>();
        assert!(joined.contains("```bash"));
        assert!(joined.contains("echo incomplete"));
    }

    #[test]
    fn shell_fences_select_an_explicit_source() {
        let configured = vec!["/bin/zsh".to_owned(), "-l".to_owned()];
        assert_eq!(
            shell_argv_for_info("shell", &configured),
            Some(configured.clone())
        );
        assert_eq!(
            shell_argv_for_info("", &configured),
            Some(configured.clone())
        );
        assert_eq!(
            shell_argv_for_info("bash title=demo", &configured),
            Some(vec!["bash".to_owned()])
        );
        assert_eq!(shell_argv_for_info("python", &configured), None);
    }

    #[test]
    fn renders_safe_pango_markup() {
        let rendered = render_text_to_pango("# A & B\nUse **bold** and `x < y`");
        assert!(rendered.contains("A &amp; B</span>"));
        assert!(rendered.contains("<b>bold</b>"));
        assert!(rendered.contains("<tt>x &lt; y</tt>"));
    }

    #[test]
    fn notebook_extension_is_unambiguous() {
        assert!(is_notebook_path(Path::new("demo.jtnb.md")));
        assert!(!is_notebook_path(Path::new("demo.md")));
        assert!(!is_notebook_path(Path::new("demo.jtnb.md.bak")));
    }

    #[test]
    fn worker_keeps_stdout_and_stderr_separate() {
        let handle = CellHandle::new();
        let receiver = spawn_cell_worker(
            CommandSpec {
                argv: vec!["sh".to_owned()],
                source: "printf out; printf err >&2; exit 7".to_owned(),
                cwd: std::env::temp_dir(),
            },
            &handle,
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = loop {
            match receiver
                .recv_timeout(Duration::from_secs(3))
                .expect("worker event")
            {
                WorkerEvent::Started(_) => {}
                WorkerEvent::Output(OutputStream::Stdout, bytes) => stdout.extend(bytes),
                WorkerEvent::Output(OutputStream::Stderr, bytes) => stderr.extend(bytes),
                WorkerEvent::Done(outcome) => break outcome,
            }
        };
        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
        assert_eq!(outcome, CellOutcome::Exited(7));
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_kills_and_reaps_the_entire_process_group() {
        let handle = CellHandle::new();
        let receiver = spawn_cell_worker(
            CommandSpec {
                argv: vec!["sh".to_owned()],
                source: "sleep 30 & echo ready; wait".to_owned(),
                cwd: std::env::temp_dir(),
            },
            &handle,
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut stdout = Vec::new();
        while !stdout
            .windows(b"ready".len())
            .any(|chunk| chunk == b"ready")
        {
            assert!(std::time::Instant::now() < deadline, "cell did not start");
            match receiver
                .recv_timeout(Duration::from_millis(100))
                .expect("ready output")
            {
                WorkerEvent::Started(_) => {}
                WorkerEvent::Output(OutputStream::Stdout, bytes) => stdout.extend(bytes),
                WorkerEvent::Output(OutputStream::Stderr, _) => {}
                WorkerEvent::Done(outcome) => panic!("cell ended before cancellation: {outcome:?}"),
            }
        }
        let group = handle.pgid.load(Ordering::SeqCst);
        assert!(group > 0, "worker did not publish its process group");
        handle.cancel();

        let outcome = loop {
            match receiver
                .recv_timeout(Duration::from_secs(3))
                .expect("worker event")
            {
                WorkerEvent::Done(outcome) => break outcome,
                WorkerEvent::Started(_) | WorkerEvent::Output(_, _) => {}
            }
        };
        assert_eq!(outcome, CellOutcome::Cancelled);
        assert!(handle.child.lock().expect("child slot").is_none());

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            // Signal 0 checks group existence without modifying it.
            let exists = unsafe { nix::libc::kill(-group, 0) } == 0;
            if !exists {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a descendant survived notebook cancellation"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[cfg(unix)]
    fn root_exit_terminates_background_descendants_before_joining_readers() {
        let handle = CellHandle::new();
        let receiver = spawn_cell_worker(
            CommandSpec {
                argv: vec!["sh".to_owned()],
                // `sleep` inherits the pipes, so joining the reader threads
                // would hang until it exits unless the worker ends the group.
                source: "sleep 30 &".to_owned(),
                cwd: std::env::temp_dir(),
            },
            &handle,
        );

        // Read the group from the event stream, not by polling `pgid`: for a
        // shell that backgrounds a child and exits, the worker can run its whole
        // lifecycle — publish the group, signal it, then reset `pgid` to 0 — in
        // the gap before a cold poll first samples it, so polling races to a
        // spurious "did not publish" timeout. The Started event is ordered ahead
        // of Done on the channel and cannot be missed.
        let group = loop {
            match receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("worker must announce its process group")
            {
                WorkerEvent::Started(group) => break group,
                WorkerEvent::Output(_, _) => {}
                WorkerEvent::Done(outcome) => {
                    panic!("cell finished before announcing its group: {outcome:?}")
                }
            }
        };
        assert!(group > 0, "worker published an invalid process group");

        let outcome = loop {
            match receiver
                .recv_timeout(Duration::from_secs(3))
                .expect("worker must not hang on inherited output pipes")
            {
                WorkerEvent::Done(outcome) => break outcome,
                WorkerEvent::Started(_) | WorkerEvent::Output(_, _) => {}
            }
        };
        assert_eq!(outcome, CellOutcome::Exited(0));
        assert_eq!(handle.pgid.load(Ordering::SeqCst), 0);
        assert!(handle.child.lock().expect("child slot").is_none());

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while unsafe { nix::libc::kill(-group, 0) } == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "a background descendant survived normal cell completion"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
