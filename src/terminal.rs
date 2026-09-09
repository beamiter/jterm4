use adw::prelude::*;
use gtk4::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk4::gdk::ModifierType;
use gtk4::gdk::RGBA;
use gtk4::gio::{self, Cancellable};
use gtk4::glib::translate::IntoGlib;
use gtk4::glib::SpawnFlags;
use gtk4::pango::FontDescription;
use gtk4::GestureClick;
use gtk4::{glib, Entry, Label, Orientation, Paned};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::Arc;
use vte4::{CursorBlinkMode, CursorShape, PtyFlags, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

use crate::config::Config;
use crate::process::{ChildLifecycle, EscalationPolicy, ReapOwner};

/// Shutdown ladder for a terminal pane's shell.
///
/// Both backends give their child a session of its own — `OwnedPty` calls
/// `setsid` between fork and exec, and VTE does the same in its PTY child
/// setup — so a pane owns everything in that session, not merely a process
/// group. Shell job control puts each foreground command in its own group, so
/// only the session sweep reaches a stubborn background job on pane close.
pub(crate) const TERMINAL_ESCALATION: EscalationPolicy = EscalationPolicy::SESSION_DRAIN;

/// Where a pane's child lifecycle is parked on its live VTE widget.
///
/// The lifecycle, not a bare pid, is what travels with the widget: it carries
/// the reap owner (VTE's glib child watch for a conventional pane, this
/// process for a Block pane), so widget-tree teardown paths never have to
/// infer ownership from the widget type — and can never signal a pid that has
/// already been reaped and recycled.
const CHILD_LIFECYCLE_DATA_KEY: &str = "child-lifecycle";

/// Attach a pane's child lifecycle to the live VTE that displays it.
pub(crate) fn set_terminal_child_lifecycle(terminal: &Terminal, lifecycle: Arc<ChildLifecycle>) {
    unsafe {
        terminal.set_data::<Arc<ChildLifecycle>>(CHILD_LIFECYCLE_DATA_KEY, lifecycle);
    }
}

/// The child lifecycle of the shell shown by `terminal`, if one was attached.
pub(crate) fn terminal_child_lifecycle(terminal: &Terminal) -> Option<Arc<ChildLifecycle>> {
    unsafe {
        terminal
            .data::<Arc<ChildLifecycle>>(CHILD_LIFECYCLE_DATA_KEY)
            .map(|lifecycle| Arc::clone(lifecycle.as_ref()))
    }
}

/// The shell pid behind `terminal`, for `/proc` probes and logging only.
pub(crate) fn terminal_child_pid(terminal: &Terminal) -> Option<i32> {
    terminal_child_lifecycle(terminal).map(|lifecycle| lifecycle.pid())
}

fn belongs_to_selected_notebook_page(terminal: &Terminal) -> bool {
    // GTK4 Notebook keeps its pages inside an internal GtkStack, so walking
    // plain `parent()` links never lands on a widget that equals `nth_page`.
    // Resolve the Notebook ancestor first, then test page membership downward.
    let widget = terminal.clone().upcast::<gtk4::Widget>();
    let Some(notebook) = widget
        .ancestor(gtk4::Notebook::static_type())
        .and_downcast::<gtk4::Notebook>()
    else {
        return true;
    };
    notebook
        .current_page()
        .and_then(|page| notebook.nth_page(Some(page)))
        .is_some_and(|selected| widget == selected || widget.is_ancestor(&selected))
}

/// Focus a mapped terminal now, then once more after the current container
/// transition.
///
/// Notebook activation owns its page- and generation-scoped frame retries in
/// `main.rs`. This generic fallback is deliberately gated by `is_mapped()`, so
/// a page hidden by a later Ctrl+PageUp/PageDown cannot steal focus back while
/// dialogs and freshly reparented panes can still restore focus after closing.
/// Never logically focus an unmapped VTE: doing so can prevent its IM context
/// from receiving a real focus-in when the widget is first mapped.
pub(crate) fn focus_terminal(terminal: &Terminal) {
    let mapped = terminal.is_mapped();
    let on_page = belongs_to_selected_notebook_page(terminal);
    if mapped && on_page {
        let grabbed = terminal.grab_focus();
        log::debug!(
            "focus_terminal: grab_focus -> {grabbed}, has_focus={}",
            terminal.has_focus()
        );
    } else {
        log::debug!("focus_terminal: skipped immediate grab (mapped={mapped} on_page={on_page})");
    }
    let deferred = terminal.clone();
    glib::idle_add_local_once(move || {
        if deferred.is_mapped() && belongs_to_selected_notebook_page(&deferred) {
            let grabbed = deferred.grab_focus();
            log::debug!(
                "focus_terminal(deferred): grab_focus -> {grabbed}, has_focus={}",
                deferred.has_focus()
            );
        }
    });
}

/// Apply the visual profile shared by regular VTE mode, block mode's live
/// surface, and block snapshots. Keeping this in one place prevents a runtime
/// theme change from making the two terminal modes drift apart.
/// One Pango parse per distinct font string, shared by every VTE.
///
/// Building the description re-parses the same configured string for the live
/// surface and for both read-only VTEs of every finished card. The value
/// changes only when the config does, so a one-entry cache is enough — keyed
/// on the icon fallback too, since that is the other half of what gets built.
fn with_font_description<R>(config: &Config, f: impl FnOnce(&FontDescription) -> R) -> R {
    thread_local! {
        static CACHED: std::cell::RefCell<Option<(String, Option<String>, FontDescription)>> =
            const { std::cell::RefCell::new(None) };
    }
    let icon_family = crate::font::icon_family(config);
    CACHED.with(|cached| {
        let mut cached = cached.borrow_mut();
        let current = cached.as_ref().is_some_and(|(desc, icon, _)| {
            desc == &config.font_desc && icon.as_deref() == icon_family
        });
        if !current {
            *cached = Some((
                config.font_desc.clone(),
                icon_family.map(str::to_owned),
                crate::font::font_description(&config.font_desc, icon_family),
            ));
        }
        let (_, _, parsed) = cached.as_ref().expect("populated above");
        f(parsed)
    })
}

pub(crate) fn apply_terminal_theme(terminal: &Terminal, config: &Config) {
    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(
        Some(&config.foreground),
        Some(&config.background),
        &palette_refs,
    );
    terminal.set_color_bold(None);
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));
    with_font_description(config, |font_desc| {
        terminal.set_font(Some(font_desc));
    });
    terminal.set_font_scale(config.default_font_scale);
}

pub(crate) fn create_terminal(config: &Config) -> Terminal {
    let font_scale = config.default_font_scale;
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .name("term_name")
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        .cursor_blink_mode(CursorBlinkMode::System)
        .cursor_shape(CursorShape::Block)
        .font_scale(font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();

    terminal.set_mouse_autohide(true);
    // Match the canonical xterm/VTE erase sequences explicitly instead of
    // inheriting distro- or profile-dependent defaults.
    terminal.set_backspace_binding(vte4::EraseBinding::AsciiDelete);
    terminal.set_delete_binding(vte4::EraseBinding::DeleteSequence);

    apply_terminal_theme(&terminal, config);

    // Set regex for hyperlinks
    let regex_pattern = vte4::Regex::for_match(
        r"https?://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    );
    terminal.match_add_regex(&regex_pattern.unwrap(), 0);

    terminal
}

// ─── VteTerminalView ──────────────────────────────────────────────────────

/// Shared lists of observer callbacks, keyed by the payload they receive.
type StrCallbacks = Rc<RefCell<Vec<Box<dyn Fn(&str)>>>>;
type IntCallbacks = Rc<RefCell<Vec<Box<dyn Fn(i32)>>>>;
type VoidCallbacks = Rc<RefCell<Vec<Box<dyn Fn()>>>>;

#[allow(dead_code)]
pub struct VteTerminalView {
    root: gtk4::Box,
    /// Status strip above the grid, shown only while this pane's tab is split.
    pane_header: crate::ui::PaneHeader,
    terminal: Terminal,
    config: Rc<RefCell<Config>>,
    cwd_callbacks: StrCallbacks,
    exited_callbacks: IntCallbacks,
    bell_callbacks: VoidCallbacks,
    title_callbacks: StrCallbacks,
    activity_callbacks: VoidCallbacks,
}

#[allow(dead_code)]
impl VteTerminalView {
    pub fn new(
        config: Rc<RefCell<Config>>,
        shell_argv: &[String],
        working_directory: Option<&str>,
        session_id: Option<&str>,
        initial_commands: &[String],
        env_extra: Vec<(String, String)>,
        on_launched: Option<Box<dyn FnOnce()>>,
    ) -> Self {
        // Create Terminal widget
        let terminal = create_terminal(&config.borrow());

        // Wrap with scrollbar, then stack the pane header above it. The outer
        // box is the pane's leaf widget, so the split tree still sees exactly
        // one widget per pane.
        let content = wrap_with_scrollbar(&terminal);
        let pane_header = crate::ui::PaneHeader::new();
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.add_css_class("vte-view-root");
        root.append(pane_header.widget());
        root.append(&content);

        let cwd_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn(&str)>>::new()));
        let exited_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn(i32)>>::new()));
        let bell_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn()>>::new()));
        let title_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn(&str)>>::new()));
        let activity_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn()>>::new()));

        // Listen for OSC 7 (CWD changes)
        let cwd_callbacks_clone = cwd_callbacks.clone();
        let terminal_for_cwd = terminal.clone();
        terminal.connect_current_directory_uri_notify(move |_| {
            if let Some(uri) = terminal_for_cwd.current_directory_uri() {
                let file = gio::File::for_uri(uri.as_str());
                if let Some(path) = file
                    .path()
                    .map(|p| p.to_string_lossy().to_string())
                    .filter(|s| !s.is_empty())
                {
                    let path = jterm_core::review_input::safe_inline_display(&path, 4 * 1024);
                    for callback in cwd_callbacks_clone.borrow().iter() {
                        callback(&path);
                    }
                }
            }
        });

        // Listen for child-exited signal. VTE hands over the raw `waitpid`
        // status its glib child watch observed; normalize it to the family's
        // exit-code convention once, here, so the lifecycle and every observer
        // see the same number a Block pane reports. Recording it retires the
        // pid: after this point the lifecycle refuses to signal a number VTE
        // has already released for reuse.
        let exited_callbacks_clone = exited_callbacks.clone();
        terminal.connect_child_exited(move |term, status| {
            let Some(code) = crate::process::exit_code_from_wait_status(status) else {
                // A stop or continue report: the child is still alive.
                return;
            };
            if let Some(lifecycle) = terminal_child_lifecycle(term) {
                lifecycle.note_foreign_exit(code);
            }
            for callback in exited_callbacks_clone.borrow().iter() {
                callback(code);
            }
        });

        // Listen for bell signal
        let bell_callbacks_clone = bell_callbacks.clone();
        terminal.connect_bell(move |_term| {
            for callback in bell_callbacks_clone.borrow().iter() {
                callback();
            }
        });

        // Listen for window-title-changed signal
        let title_callbacks_clone = title_callbacks.clone();
        let terminal_for_title = terminal.clone();
        terminal.connect_window_title_changed(move |_term| {
            if let Some(title) = terminal_for_title.window_title() {
                let title_str = jterm_core::review_input::safe_inline_display(&title, 512);
                if !title_str.is_empty() {
                    for callback in title_callbacks_clone.borrow().iter() {
                        callback(&title_str);
                    }
                }
            }
        });

        // Listen for contents-changed signal (activity)
        let activity_callbacks_clone = activity_callbacks.clone();
        terminal.connect_contents_changed(move |_term| {
            for callback in activity_callbacks_clone.borrow().iter() {
                callback();
            }
        });

        // Spawn shell
        spawn_shell(
            &terminal,
            shell_argv,
            working_directory,
            session_id,
            initial_commands,
            env_extra,
            on_launched,
        );

        VteTerminalView {
            root,
            pane_header,
            terminal,
            config,
            cwd_callbacks,
            exited_callbacks,
            bell_callbacks,
            title_callbacks,
            activity_callbacks,
        }
    }

    pub(crate) fn pane_header(&self) -> &crate::ui::PaneHeader {
        &self.pane_header
    }

    pub fn widget(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }

    pub fn vte(&self) -> &Terminal {
        &self.terminal
    }

    pub fn connect_cwd_changed<F>(&self, callback: F)
    where
        F: Fn(&str) + 'static,
    {
        self.cwd_callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn connect_exited<F>(&self, callback: F)
    where
        F: Fn(i32) + 'static,
    {
        self.exited_callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn grab_focus(&self) {
        focus_terminal(&self.terminal);
    }

    pub fn copy_to_clipboard(&self) {
        self.terminal.copy_clipboard_format(vte4::Format::Text);
    }

    pub fn paste_from_clipboard(&self) {
        self.terminal.paste_clipboard();
    }

    pub fn connect_bell<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        self.bell_callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn connect_title_changed<F>(&self, callback: F)
    where
        F: Fn(&str) + 'static,
    {
        self.title_callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn connect_activity<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        self.activity_callbacks
            .borrow_mut()
            .push(Box::new(callback));
    }

    /// Takes the configured string, not a parsed description: the surface gets
    /// an icon fallback appended that the user's font string must not carry.
    pub fn set_font(&self, desc: &str) {
        let font_desc = crate::font::terminal_font_description(desc, &self.config.borrow());
        self.terminal.set_font(Some(&font_desc));
    }

    pub fn set_font_scale(&self, scale: f64) {
        self.terminal.set_font_scale(scale);
    }

    pub fn apply_theme(&self) {
        let config = self.config.borrow();
        apply_terminal_theme(&self.terminal, &config);
    }

    pub fn write_input(&self, data: &[u8]) {
        self.terminal.feed_child(data);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(pty) = self.terminal.pty() {
            let _ = pty.set_size(rows as i32, cols as i32);
        }
    }

    /// Tear this pane's shell down through the shared escalation ladder.
    ///
    /// Idempotent by construction: the second call — an explicit pane close
    /// followed by the widget's own teardown — finds a termination already in
    /// flight and does nothing.
    pub fn kill(&self) {
        if let Some(lifecycle) = terminal_child_lifecycle(&self.terminal) {
            lifecycle.terminate(TERMINAL_ESCALATION);
        }
    }

    pub fn pid_i32(&self) -> i32 {
        terminal_child_pid(&self.terminal).unwrap_or(0)
    }

    /// Borrow the VTE-managed master-side PTY descriptor for process probes.
    pub fn pty_fd_i32(&self) -> i32 {
        self.terminal
            .pty()
            .map(|pty| pty.fd().as_raw_fd())
            .unwrap_or(-1)
    }
}

/// Wrap a terminal in an hbox with a scrollbar on the right side.
pub(crate) fn wrap_with_scrollbar(terminal: &Terminal) -> gtk4::Box {
    let hbox = gtk4::Box::new(Orientation::Horizontal, 0);
    hbox.set_hexpand(true);
    hbox.set_vexpand(true);
    hbox.add_css_class("terminal-box");
    let scrollbar = gtk4::Scrollbar::new(Orientation::Vertical, terminal.vadjustment().as_ref());
    hbox.append(terminal);
    hbox.append(&scrollbar);
    hbox
}

/// If the widget is a terminal inside a scrollbar wrapper box, return the wrapper box.
pub(crate) fn scrollbar_wrapper_of(term_widget: &gtk4::Widget) -> Option<gtk4::Box> {
    let parent = term_widget.parent()?;
    let bx = parent.clone().downcast::<gtk4::Box>().ok()?;
    if bx.has_css_class("terminal-box") {
        Some(bx)
    } else {
        None
    }
}

pub(crate) fn terminal_working_directory(terminal: &Terminal) -> Option<String> {
    // Prefer OSC 7 reported directory
    if let Some(uri) = terminal.current_directory_uri() {
        let file = gio::File::for_uri(uri.as_str());
        if let Some(path) = file
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(path);
        }
    }
    // Fallback: read /proc/<pid>/cwd
    crate::process::process_cwd(terminal_child_pid(terminal)?)
}

/// Commands typed into a new shell on its behalf, one PTY line each.
///
/// Configuration retains its historical comma-separated syntax, but it is
/// parsed once at the application boundary. Session restore instead constructs
/// exactly one safely quoted command from a persisted argv. Downstream terminal
/// backends therefore never reinterpret a restored command's commas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InitialCommands(Vec<String>);

impl InitialCommands {
    pub(crate) fn from_config(configured: Option<&str>) -> Self {
        const MAX_INITIAL_COMMANDS: usize = 128;
        let Some(configured) = configured.filter(|value| {
            value.len() <= jterm_core::review_input::MAX_REVIEW_INPUT_BYTES
                && !value.chars().any(char::is_control)
                && !jterm_core::review_input::contains_visual_spoofing(value)
        }) else {
            return Self::default();
        };
        let mut commands = Vec::new();
        for command in configured
            .split(", ")
            .map(str::trim)
            .filter(|command| !command.is_empty())
        {
            if commands.len() == MAX_INITIAL_COMMANDS
                || jterm_core::review_input::validate(command).is_err()
            {
                log::warn!(
                    "Skipping startup commands because the configuration exceeds the safe execution contract"
                );
                return Self::default();
            }
            commands.push(command.to_string());
        }
        Self(commands)
    }

    /// Quote a restored argv (e.g. `["ssh", "host"]`) into exactly one command
    /// for the configured interactive shell. Unsafe argvs and unknown shell
    /// grammars skip replay instead of risking changed argument boundaries.
    pub(crate) fn from_restored_argv(argv: Option<&[String]>, shell_argv: &[String]) -> Self {
        // Structured argv preserves boundaries, but it does not make an
        // arbitrary command trustworthy. Re-run the narrow persistence
        // classifier at the execution boundary so a modified snapshot cannot
        // turn `sh -c ...` (or any other local command) into startup code.
        let classified = argv.and_then(crate::process::match_restorable_command_bounded);
        let command = classified
            .as_deref()
            .and_then(|argv| crate::process::shell_quote_argv_for(argv, shell_argv))
            .filter(|command| {
                command.len() <= crate::process::MAX_RESTORABLE_QUOTED_COMMAND_BYTES_LOCAL
            });
        if argv.is_some() && command.is_none() {
            log::warn!(
                "Skipping session command replay because its argv is not a recognized restorable command, is unsafe, or the configured shell grammar is unsupported"
            );
        }
        Self(command.into_iter().collect())
    }

    pub(crate) fn as_slice(&self) -> &[String] {
        &self.0
    }
}

pub(crate) fn spawn_shell(
    terminal: &Terminal,
    argv_owned: &[String],
    working_directory: Option<&str>,
    session_id: Option<&str>,
    initial_commands: &[String],
    env_extra: Vec<(String, String)>,
    on_launched: Option<Box<dyn FnOnce()>>,
) {
    // Append --session <id> to argv when restoring a session (only for jsh)
    let mut argv_vec: Vec<String> = argv_owned.to_vec();
    if let Some(sid) =
        session_id.filter(|sid| jterm_core::execution_journal::is_valid_jsh_session_id(sid))
    {
        let is_jsh = argv_vec
            .first()
            .and_then(|s| std::path::Path::new(s).file_name())
            .and_then(|f| f.to_str())
            .map(|name| name == "jsh")
            .unwrap_or(false);

        if is_jsh {
            argv_vec.push("--session".to_string());
            argv_vec.push(sid.to_string());
        }
    }
    // Extra `KEY=value` pairs overlaid on the frozen child environment. Empty
    // for ordinary shells; task terminals use it to pin validation children to
    // no-rc startup files. The pairs must reach both a native child (through
    // the frozen envv) and a flatpak-spawn host wrapper (through wrap_argv).
    let env_extra_pairs: Vec<(&str, &str)> = env_extra
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let home = std::env::var("HOME").ok();
    let requested_working_directory = working_directory.or(home.as_deref());
    // The terminal identity must reach both a native child and a shell launched
    // by `flatpak-spawn --host`; the latter needs it encoded in the wrapper argv,
    // which `wrap_argv` now builds from the same shared policy.
    let argv_vec = crate::host::wrap_argv(&argv_vec, requested_working_directory, &env_extra_pairs);
    let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();

    // The child gets the complete environment frozen at launch (see
    // `capture_inherited_environment` in `app::run`), so variables written
    // after startup — CLI's `FORGE_*`, the input-method `GTK_*` rewrites —
    // never reach the shell. `VTE_SPAWN_NO_PARENT_ENVV` stops VTE from merging
    // the live, toolkit-mutated process environment back in; without it the
    // frozen block would be pointless. The flag and the frozen block must stay
    // paired: with the pre-freeze identity-only fallback envv the flag would
    // strip the whole parent environment.
    let child_identity = jterm_core::child_env::ChildEnv::from_identity();
    let (envv_owned, frozen) =
        match jterm_core::child_env::vte_envv_from_captured(&child_identity, &env_extra_pairs) {
            Ok(envv) => (envv, true),
            Err(err) => {
                match jterm_core::child_env::envp_from_captured(&child_identity, &env_extra_pairs) {
                    Ok(block) => {
                        // The frozen block exists but `vte_envv_from_captured`
                        // rejected it: some inherited name or value is not
                        // UTF-8, which the `&str`-based VTE envv cannot carry.
                        // Rebuild the same block from the frozen capture with
                        // those entries dropped (a non-UTF-8 name drops the
                        // whole entry) and keep the pairing invariant — the
                        // flag must still suppress the live, mutated parent
                        // environment instead of merging it back in.
                        let envv: Vec<String> = block
                            .into_iter()
                            .filter_map(|entry| String::from_utf8(entry.into_bytes()).ok())
                            .collect();
                        log::warn!(
                            "non-UTF-8 entries scrubbed from the frozen launch environment: {err}"
                        );
                        (envv, true)
                    }
                    Err(err) => {
                        // Unreachable in the app (`app::run` captures before GTK
                        // starts); keep the pre-freeze identity-only behavior
                        // rather than failing the spawn.
                        log::warn!(
                            "no frozen launch environment; VTE inherits the live one: {err}"
                        );
                        (
                            jterm_core::child_env::vte_envv(&child_identity, &env_extra_pairs),
                            false,
                        )
                    }
                }
            }
        };
    let envv: Vec<&str> = envv_owned.iter().map(String::as_str).collect();
    let envv: &[&str] = &envv;
    let mut spawn_flags = SpawnFlags::SEARCH_PATH;
    if frozen {
        spawn_flags |=
            SpawnFlags::from_bits_retain(jterm_core::child_env::VTE_SPAWN_NO_PARENT_ENVV_BITS);
    }
    let cancellable: Option<&Cancellable> = None;
    let spawn_working_directory = if crate::host::is_flatpak() {
        None
    } else {
        requested_working_directory
    };
    let terminal_for_pid = terminal.clone();

    // If initial commands are provided, send them after the shell starts.
    let init_cmds: Vec<String> = initial_commands.to_vec();
    let terminal_for_init = terminal.clone();

    terminal.spawn_async(
        PtyFlags::DEFAULT,
        spawn_working_directory,
        &argv,
        envv,
        spawn_flags,
        || {},
        -1,
        cancellable,
        move |res| {
            log::debug!("spawn_async: {res:?}");
            // The spawn resolved one way or the other: the child (on success)
            // has already entered its working directory through the path it
            // was handed, so launch-time pins may be released now. Runs on
            // failure too — the pin must not leak on a failed spawn.
            if let Some(on_launched) = on_launched {
                on_launched();
            }
            if let Ok(pid) = res {
                let pid_i32: i32 = pid.into_glib();
                // VTE spawned this child through glib, and glib's child watch
                // is what calls `waitpid` for it — hence `Foreign`. The
                // lifecycle only ever signals it (through a pidfd), and learns
                // the status from `child-exited`; reaping here would consume
                // the status VTE is waiting for and free the pid behind its
                // back.
                match ChildLifecycle::new(pid_i32, ReapOwner::Foreign) {
                    Ok(lifecycle) => set_terminal_child_lifecycle(&terminal_for_pid, lifecycle),
                    Err(error) => {
                        log::warn!("Cannot manage the lifecycle of VTE child {pid_i32}: {error}")
                    }
                }
            }
            // Feed initial commands after the shell has fully initialized.
            // We delay to ensure the shell has entered raw mode; sending \r
            // too early would hit the kernel's cooked-mode icrnl translation
            // (turning \r into \n), which raw-mode shells don't treat as Enter.
            // Each pre-parsed command is fed as exactly one line; splitting
            // here would let a restored command's own bytes change boundaries.
            if !init_cmds.is_empty() {
                let cmds = init_cmds.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                    for line in &cmds {
                        let text = format!("{line}\r");
                        terminal_for_init.feed_child(text.as_bytes());
                    }
                });
            }
        },
    );
}

pub(crate) fn open_uri(uri: &str) {
    if !jterm_core::link::is_openable_url(uri) {
        log::warn!("Refused to open an unsafe or unsupported terminal hyperlink");
        return;
    }
    if let Err(err) = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>) {
        log::warn!(
            "Failed to open URI {}: {err}",
            jterm_core::review_input::safe_inline_display(uri, 2 * 1024)
        );
    }
}

pub(crate) fn show_rename_dialog(
    window: &adw::ApplicationWindow,
    label: &Label,
    custom_title: Rc<Cell<bool>>,
) {
    let dialog = adw::AlertDialog::new(Some("Rename tab"), None);
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);

    let entry = Entry::new();
    entry.set_text(&label.text());
    entry.set_activates_default(true);
    dialog.set_extra_child(Some(&entry));

    let label_clone = label.clone();
    let custom_title_clone = custom_title.clone();
    let value = entry.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response == "rename" {
            let text = value.text();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let title = jterm_core::review_input::safe_inline_display(trimmed, 512);
                label_clone.set_text(&title);
                custom_title_clone.set(true);
            }
        }
    });

    dialog.present(Some(window));
}

pub(crate) fn show_rename_dialog_with_strip(
    window: &adw::ApplicationWindow,
    label: &Label,
    strip_label: &Label,
    custom_title: Rc<Cell<bool>>,
    private_title: Rc<Cell<bool>>,
) {
    let dialog = adw::AlertDialog::new(Some("Rename tab"), None);
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);

    let entry = Entry::new();
    entry.set_text(&label.text());
    entry.set_activates_default(true);
    dialog.set_extra_child(Some(&entry));

    let label_clone = label.clone();
    let strip_label_clone = strip_label.clone();
    let custom_title_clone = custom_title.clone();
    let private_title_clone = private_title.clone();
    let value = entry.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response == "rename" {
            let text = value.text();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let title = jterm_core::review_input::safe_inline_display(trimmed, 512);
                label_clone.set_text(&title);
                if !private_title_clone.get() {
                    strip_label_clone.set_text(&title);
                }
                custom_title_clone.set(true);
            }
        }
    });

    dialog.present(Some(window));
}

pub(crate) fn default_tab_title(tab_index_1based: u32, working_directory: Option<&str>) -> String {
    let mut resolved_dir = working_directory
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    // If no directory is known (e.g. first launch), default to HOME so the tab has a meaningful title.
    if resolved_dir.is_none() {
        resolved_dir = std::env::var("HOME").ok();
    }

    let Some(dir) = resolved_dir.as_deref() else {
        return format!("Terminal {tab_index_1based}");
    };

    // Normalize trailing slashes.
    let mut normalized = dir.trim_end_matches('/');
    if normalized.is_empty() {
        normalized = "/";
    }

    // Shorten $HOME to ~.
    let home = std::env::var("HOME").ok();
    let display_dir = if let Some(home) = home.as_deref() {
        if normalized == home {
            "~".to_string()
        } else if let Some(rest) = normalized.strip_prefix(home) {
            if rest.starts_with('/') {
                format!("~{rest}")
            } else {
                normalized.to_string()
            }
        } else {
            normalized.to_string()
        }
    } else {
        normalized.to_string()
    };
    let display_dir = jterm_core::review_input::safe_inline_display(&display_dir, 4 * 1024);

    if display_dir == "/" || display_dir == "~" {
        return display_dir;
    }

    // Fish-like prompt_pwd: abbreviate intermediate components, keep the last component.
    // Example: /usr/local/bin -> /u/l/bin, ~/projects/rust-project/jwm -> ~/p/r/jwm
    fn shorten_component(component: &str) -> String {
        if component.is_empty() {
            return String::new();
        }
        if component == "." || component == ".." {
            return component.to_string();
        }

        let mut chars = component.chars();
        let first = chars.next().unwrap();
        if first == '.' {
            // Better readability for dot-dirs: ".config" -> ".c".
            if let Some(second) = chars.next() {
                let mut out = String::new();
                out.push(first);
                out.push(second);
                out
            } else {
                ".".to_string()
            }
        } else {
            first.to_string()
        }
    }

    let (prefix, rest) = if let Some(r) = display_dir.strip_prefix("~/") {
        ("~/", r)
    } else if let Some(r) = display_dir.strip_prefix('/') {
        ("/", r)
    } else {
        ("", display_dir.as_str())
    };

    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 1 {
        return jterm_core::review_input::safe_inline_display(&format!("{prefix}{rest}"), 512);
    }

    let mut out_parts: Vec<String> = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            out_parts.push((*part).to_string());
        } else {
            out_parts.push(shorten_component(part));
        }
    }

    jterm_core::review_input::safe_inline_display(&format!("{prefix}{}", out_parts.join("/")), 512)
}

pub(crate) fn setup_terminal_click_handler(terminal: &Terminal) {
    // Use a click gesture in Capture phase to intercept Ctrl+Click before VTE sees it
    // For normal clicks, let them pass through to VTE for text selection
    let click_controller = GestureClick::new();
    click_controller.set_button(GDK_BUTTON_PRIMARY as u32);
    click_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let terminal_clone = terminal.clone();

    click_controller.connect_pressed(move |controller, n_press, x, y| {
        // Only intercept single Ctrl+Click on hyperlinks
        // Let all other clicks pass through to VTE for selection
        if n_press == 1 {
            let state = controller.current_event_state();
            if state.contains(ModifierType::CONTROL_MASK) {
                if let Some(uri) = terminal_clone.check_match_at(x, y).0 {
                    open_uri(&uri);
                    // Claim this event to prevent VTE from processing it
                    controller.set_state(gtk4::EventSequenceState::Claimed);
                    return;
                }
            }
        }
        // Explicitly deny to pass event to VTE for text selection
        controller.set_state(gtk4::EventSequenceState::Denied);
    });

    terminal.add_controller(click_controller);
}

/// Find the first Terminal in a widget tree (depth-first). Traverses children
/// generically so the live VTE buried under ScrolledWindow → Viewport →
/// block_list → active_holder is reachable.
pub(crate) fn find_first_terminal(widget: &gtk4::Widget) -> Option<Terminal> {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        return Some(term);
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(term) = find_first_terminal(&c) {
            return Some(term);
        }
        child = c.next_sibling();
    }
    None
}

/// Find the focused Terminal in a widget tree.
pub(crate) fn find_focused_terminal(widget: &gtk4::Widget) -> Option<Terminal> {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        if term.has_focus() {
            return Some(term);
        }
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(term) = find_focused_terminal(&c) {
            return Some(term);
        }
        child = c.next_sibling();
    }
    None
}

/// Collect all terminals in a widget tree.
pub(crate) fn collect_terminals(widget: &gtk4::Widget, out: &mut Vec<Terminal>) {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        out.push(term);
        return;
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        collect_terminals(&c, out);
        child = c.next_sibling();
    }
}

/// Walk the Paned tree and reattach a terminal to the first None child slot found.
pub(crate) fn reattach_terminal_to_tree(
    widget: &gtk4::Widget,
    child_to_reattach: &gtk4::Widget,
) -> bool {
    if let Ok(paned) = widget.clone().downcast::<Paned>() {
        if paned.start_child().is_none() {
            paned.set_start_child(Some(child_to_reattach));
            return true;
        }
        if paned.end_child().is_none() {
            paned.set_end_child(Some(child_to_reattach));
            return true;
        }
        if let Some(start) = paned.start_child() {
            if reattach_terminal_to_tree(&start, child_to_reattach) {
                return true;
            }
        }
        if let Some(end) = paned.end_child() {
            if reattach_terminal_to_tree(&end, child_to_reattach) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::InitialCommands;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn configured_commands_are_split_only_at_the_boundary() {
        let commands = InitialCommands::from_config(Some("cd /tmp, printf ready"));
        assert_eq!(
            commands.as_slice(),
            strings(&["cd /tmp", "printf ready"]).as_slice()
        );
        assert!(InitialCommands::from_config(None).as_slice().is_empty());
    }

    #[test]
    fn configured_commands_fail_closed_on_hidden_oversized_or_excessive_input() {
        assert!(InitialCommands::from_config(Some("echo safe\u{202e}fake"))
            .as_slice()
            .is_empty());
        assert!(InitialCommands::from_config(Some("echo one\necho two"))
            .as_slice()
            .is_empty());
        let oversized = "x".repeat(jterm_core::review_input::MAX_REVIEW_INPUT_BYTES + 1);
        assert!(InitialCommands::from_config(Some(&oversized))
            .as_slice()
            .is_empty());
        let excessive = std::iter::repeat_n("true", 129)
            .collect::<Vec<_>>()
            .join(", ");
        assert!(InitialCommands::from_config(Some(&excessive))
            .as_slice()
            .is_empty());
    }

    #[test]
    fn terminal_links_are_bounded_unambiguous_http_urls() {
        use jterm_core::link::{is_openable_url, MAX_OPENABLE_URL_BYTES};

        assert!(is_openable_url("https://example.com/a?q=1"));
        assert!(is_openable_url("HTTP://example.com"));
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("custom://run-command"));
        assert!(!is_openable_url("https:///missing-host"));
        assert!(!is_openable_url("https://safe.example/\u{202e}fake"));
        // Userinfo would hand the system opener a credential the user never
        // typed; the shared policy refuses it outright.
        assert!(!is_openable_url("https://user:token@example.com/"));
        assert!(!is_openable_url(&format!(
            "https://example.com/{}",
            "x".repeat(MAX_OPENABLE_URL_BYTES)
        )));
    }

    #[test]
    fn restored_argv_is_always_one_command_even_when_arguments_contain_commas() {
        let argv = strings(&["ssh", "host", "printf '%s, %s' one two"]);
        let commands = InitialCommands::from_restored_argv(Some(&argv), &strings(&["bash"]));
        assert_eq!(commands.as_slice().len(), 1);
        assert_eq!(
            commands.as_slice()[0],
            "'ssh' 'host' 'printf '\"'\"'%s, %s'\"'\"' one two'"
        );
    }

    #[test]
    fn unsafe_restored_argv_is_not_replayed() {
        let argv = strings(&["ssh", "host", "echo first\necho second"]);
        assert!(
            InitialCommands::from_restored_argv(Some(&argv), &strings(&["bash"]))
                .as_slice()
                .is_empty()
        );
        // Unknown shell grammars are not guessed either.
        let plain = strings(&["ssh", "host"]);
        assert!(InitialCommands::from_restored_argv(
            Some(&plain),
            &strings(&["/opt/exotic-shell"])
        )
        .as_slice()
        .is_empty());

        // An attacker-controlled snapshot must not gain arbitrary local code
        // execution merely because it preserved argv boundaries.
        let arbitrary = strings(&["sh", "-c", "touch /tmp/from-snapshot"]);
        assert!(
            InitialCommands::from_restored_argv(Some(&arbitrary), &strings(&["bash"]))
                .as_slice()
                .is_empty()
        );

        let too_many = (0..=crate::process::MAX_RESTORABLE_ARG_COUNT_LOCAL)
            .map(|index| {
                if index == 0 {
                    "ssh".to_string()
                } else {
                    format!("arg-{index}")
                }
            })
            .collect::<Vec<_>>();
        assert!(
            InitialCommands::from_restored_argv(Some(&too_many), &strings(&["bash"]))
                .as_slice()
                .is_empty()
        );

        let oversized_field = strings(&[
            "ssh",
            &"x".repeat(crate::process::MAX_RESTORABLE_ARG_BYTES_LOCAL + 1),
        ]);
        assert!(
            InitialCommands::from_restored_argv(Some(&oversized_field), &strings(&["bash"]))
                .as_slice()
                .is_empty()
        );

        let chunk = "x".repeat(crate::process::MAX_RESTORABLE_ARG_BYTES_LOCAL);
        let oversized_total = vec![
            "ssh".to_string(),
            chunk.clone(),
            chunk.clone(),
            chunk.clone(),
            chunk,
        ];
        assert!(
            InitialCommands::from_restored_argv(Some(&oversized_total), &strings(&["bash"]))
                .as_slice()
                .is_empty()
        );

        // POSIX quoting expands every embedded apostrophe to several bytes.
        // Bound the actual PTY line as well as the structured representation.
        let quote_heavy = vec![
            "ssh".to_string(),
            "'".repeat(crate::process::MAX_RESTORABLE_ARG_BYTES_LOCAL),
            "'".repeat(crate::process::MAX_RESTORABLE_ARG_BYTES_LOCAL),
            "'".repeat(crate::process::MAX_RESTORABLE_ARG_BYTES_LOCAL),
        ];
        assert!(crate::process::restorable_argv_within_local_limits(
            &quote_heavy
        ));
        assert!(
            InitialCommands::from_restored_argv(Some(&quote_heavy), &strings(&["bash"]))
                .as_slice()
                .is_empty()
        );
    }

    #[test]
    fn restored_argv_uses_powershell_call_syntax() {
        let argv = strings(&["ssh", "host", "printf 'safe'; one argument"]);
        let commands =
            InitialCommands::from_restored_argv(Some(&argv), &strings(&["/usr/bin/pwsh"]));
        assert_eq!(
            commands.as_slice(),
            strings(&["& 'ssh' 'host' 'printf ''safe''; one argument'"]).as_slice()
        );
    }
}
