//! config_apply — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::gdk::RGBA;
use gtk4::glib;
use libadwaita as adw;
use vte4::Terminal;
use vte4::{TerminalExt, TerminalExtManual};

use super::*;
use crate::config::{
    choose_shell_argv, config_file_path, load_config, validate_config_contents, Theme,
};
use crate::terminal::collect_terminals;

/// Attempts a reload spends waiting for the persistence worker's short
/// critical section on the revision slot before it gives up and says so.
const CONFIG_REVISION_READ_ATTEMPTS: u8 = 4;
/// Gap between those attempts. The writer holds the slot only for a move, so
/// one retry is already generous; four bound the wait at 80 ms of idle timer.
const CONFIG_REVISION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Read the revision this window believes the file holds, without ever waiting.
///
/// `None` means the persistence worker holds the slot right now. The GTK thread
/// must not block on it — that is the whole point of taking the disk I/O out
/// from under that mutex — and it must not guess either, because guessing
/// "unknown" reads as "the file moved" and would apply a reload nobody proved
/// was safe. The caller retries instead.
fn live_config_revision(
    config: &crate::config::Config,
) -> Option<Option<crate::config_store::ConfigRevision>> {
    match config.persistence_revision.try_lock() {
        Ok(revision) => Some(revision.clone()),
        // A poisoned slot is not contention: the value behind it is still the
        // last revision a writer published, and refusing every future reload
        // over it would be worse than reading it.
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner().clone()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

fn reload_matches_live_revision(
    live_revision: Option<&crate::config_store::ConfigRevision>,
    disk_revision: &crate::config_store::ConfigRevision,
) -> bool {
    live_revision == Some(disk_revision)
}

/// What an incoming configuration reload is allowed to do to the live settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigReloadDecision {
    /// The file already holds the revision this window last read or wrote.
    Skip,
    /// The file moved and this window has nothing waiting to be written.
    Apply,
    /// The file moved while a UI edit was still inside its debounce. Applying
    /// would drop that edit on the floor, so the user resolves it.
    Conflict,
}

/// Decide a reload against the file's revision and this window's dirty epoch.
///
/// `Skip` outranks `Conflict` deliberately: if the bytes on disk are the ones
/// this window already accounted for, there is no external change to conflict
/// with, and an unsaved edit simply stays pending.
fn decide_config_reload(
    live_revision: Option<&crate::config_store::ConfigRevision>,
    disk_revision: &crate::config_store::ConfigRevision,
    unsaved_ui_edits: bool,
) -> ConfigReloadDecision {
    if reload_matches_live_revision(live_revision, disk_revision) {
        ConfigReloadDecision::Skip
    } else if unsaved_ui_edits {
        ConfigReloadDecision::Conflict
    } else {
        ConfigReloadDecision::Apply
    }
}

/// Why an applied reload could not be trusted, or `None` when it can.
///
/// `reload_config` reads the file twice: once to take its revision and check
/// its syntax, and once inside [`load_config`], which is the read that actually
/// produces the `Config`. They are separate opens with no lock between them, so
/// the second one can fail outright — `load_file_config` answers a read error
/// with `FileConfig::default()` — or can land on different bytes after a racing
/// writer. Either way the old code replaced every live setting: theme,
/// keybindings, remote hosts and all, silently reverted to defaults or to a
/// revision nothing had validated. Refuse instead; the live settings stay and
/// the watcher fires again for whatever the file settles on.
fn reload_read_failure(
    load_error: Option<String>,
    loaded_revision: Option<&crate::config_store::ConfigRevision>,
    validated_revision: &crate::config_store::ConfigRevision,
) -> Option<String> {
    if let Some(reason) = load_error {
        return Some(reason);
    }
    if loaded_revision != Some(validated_revision) {
        return Some("the file changed again while it was being read".to_string());
    }
    None
}

/// Whether the in-memory configuration holds UI edits the file has not seen.
///
/// The window between a settings change and its write is real and wide: a
/// generic mutation waits [`CONFIG_PERSIST_DEBOUNCE`] (250 ms) and a font step
/// waits [`FONT_PERSIST_DEBOUNCE`] (400 ms), while the config-file watcher
/// reloads after only 200 ms of quiet. A reload landing inside that window used
/// to replace the whole `Config` — the pending edit with it — and the debounced
/// write then persisted the *reloaded* snapshot, so the user's change vanished
/// with nothing on screen to say it ever existed.
///
/// `edits` counts UI-originated changes on the GTK thread. `persisted` is the
/// highest edit count a write actually committed; it is an atomic because that
/// commit happens on the persistence worker thread, and it is raised with
/// `fetch_max` so a write finishing out of order can never mark a newer edit
/// clean.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConfigDirtyEpoch {
    edits: Rc<Cell<u64>>,
    persisted: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ConfigDirtyEpoch {
    /// Record one UI-originated change.
    pub(crate) fn record_edit(&self) {
        self.edits.set(self.edits.get().wrapping_add(1));
    }

    pub(crate) fn has_unsaved_edits(&self) -> bool {
        self.edits.get() > self.persisted.load(std::sync::atomic::Ordering::Acquire)
    }

    /// A witness for the snapshot being handed to a writer, moved to whichever
    /// thread performs the write and redeemed only when that write succeeds.
    fn commit_handle(&self) -> ConfigPersistCommit {
        ConfigPersistCommit {
            epoch: self.edits.get(),
            persisted: std::sync::Arc::clone(&self.persisted),
        }
    }

    /// Abandon every unsaved edit, because the user chose the file's version.
    fn abandon_unsaved_edits(&self) {
        self.persisted
            .store(self.edits.get(), std::sync::atomic::Ordering::Release);
    }
}

/// Proof that a specific snapshot reached the file.
struct ConfigPersistCommit {
    epoch: u64,
    persisted: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ConfigPersistCommit {
    /// `fetch_max`, never a plain store: an edit made while this write was in
    /// flight carries a higher epoch and must stay dirty afterwards.
    fn commit(self) {
        self.persisted
            .fetch_max(self.epoch, std::sync::atomic::Ordering::AcqRel);
    }
}

impl UiState {
    fn show_safe_mode_config_notice(&self) {
        // Safe mode deliberately refuses persistence; that is informational,
        // not an error that should block the settings surface. Its guard is
        // deliberately separate from save/reload errors: an informational
        // toast must never hide an actionable failure dialog.
        if self.safe_mode_config_notice_visible.replace(true) {
            return;
        }
        let toast = adw::Toast::new(
            "Safe mode: this change applies only to the current window and will not be saved.",
        );
        toast.set_timeout(4);
        let visible = self.safe_mode_config_notice_visible.clone();
        toast.connect_dismissed(move |_| visible.set(false));
        self.toast_overlay.add_toast(toast);
    }

    pub(crate) fn show_config_error(&self, title: &str, message: &str) {
        if self.config_save_error_visible.replace(true) {
            return;
        }
        let dialog = adw::AlertDialog::new(Some(title), Some(message));
        dialog.add_response("ok", "OK");
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("ok");
        let visible = self.config_save_error_visible.clone();
        dialog.connect_response(None, move |_, _| visible.set(false));
        dialog.present(Some(&self.window));
    }

    /// Persist a UI-originated configuration change and make conflicts,
    /// validation refusal, lock timeouts and I/O failures visible to the user.
    pub(crate) fn persist_config(&self) {
        self.schedule_config_persist(true);
    }

    pub(crate) fn flush_pending_config(&self) {
        let generation = self.config_persist_generation.get().wrapping_add(1);
        self.config_persist_generation.set(generation);
        self.persist_config_now(false, true);
    }

    fn schedule_config_persist(&self, show_safe_mode_notice: bool) {
        if std::env::var_os("FORGE_SAFE_MODE").is_some() {
            if show_safe_mode_notice {
                self.show_safe_mode_config_notice();
            }
            return;
        }
        // Dirty from here, not from the moment the debounce fires: the reload
        // that would discard this edit can arrive at any point inside the wait.
        self.config_dirty.record_edit();
        let generation = self.config_persist_generation.get().wrapping_add(1);
        self.config_persist_generation.set(generation);
        let ui = self.clone();
        glib::timeout_add_local_once(CONFIG_PERSIST_DEBOUNCE, move || {
            if ui.config_persist_generation.get() == generation {
                ui.persist_config_now(false, false);
            }
        });
    }

    fn persist_config_now(&self, show_safe_mode_notice: bool, allow_sync_fallback: bool) {
        if std::env::var_os("FORGE_SAFE_MODE").is_some() {
            if show_safe_mode_notice {
                self.show_safe_mode_config_notice();
            }
            return;
        }
        let snapshot = self.config.borrow().clone();
        let path = config_file_path();
        let key = crate::persistence::PersistenceKey::for_path("config", &path);
        // The witness names the edit this exact snapshot carries. Redeeming it
        // on the worker thread, immediately after the store published the new
        // revision, keeps "the file is current" and "nothing is pending" from
        // being observable in the wrong order by the 200 ms watch debounce.
        let queued_commit = self.config_dirty.commit_handle();
        let fallback_commit = self.config_dirty.commit_handle();
        if let Err(error) = crate::persistence::enqueue(key, CONFIG_PERSIST_OPERATION, move || {
            crate::config_store::save_config(&snapshot)
                .map(|_| queued_commit.commit())
                .map_err(|error| std::io::Error::other(error.to_string()))
        }) {
            if allow_sync_fallback {
                match crate::config::save_config(&self.config.borrow()) {
                    Ok(()) => fallback_commit.commit(),
                    Err(sync_error) => self.show_config_error(
                        "Settings were not saved",
                        &format!(
                            "{sync_error}\n\nThe in-memory setting is still active. Reload the configuration (Ctrl+Shift+R) before trying again if the file changed elsewhere."
                        ),
                    ),
                }
                return;
            }
            self.show_config_error(
                "Settings were not saved",
                &format!(
                    "{error}\n\nThe in-memory setting is still active. Reload the configuration (Ctrl+Shift+R) before trying again if the file changed elsewhere."
                ),
            );
        }
    }

    /// Push the current behavioral configuration into every live Block pane,
    /// including panes nested under splits.
    pub(crate) fn sync_block_configs(&self) {
        // Keep the UiState cell unborrowed while pane callbacks apply the
        // snapshot. The cells are separate today, but holding this `Ref`
        // across `reload_config` would become re-entrant if their ownership is
        // ever unified.
        let config = self.config.borrow().clone();
        for page in 0..self.notebook.n_pages() {
            let Some(widget) = self.notebook.nth_page(Some(page)) else {
                continue;
            };
            let Some(node) = PaneNode::from_widget(&widget) else {
                continue;
            };
            for leaf in node.leaves() {
                if let Some(view) = leaf.block_view() {
                    view.reload_config(&config);
                }
            }
        }
    }

    /// Apply a font scale to the live panes, the in-memory config, and — once
    /// the steps stop arriving — the config file. Used by the hotkeys, the
    /// Ctrl+wheel path, and the settings dialog; all three emit bursts (a held
    /// key, a wheel notch train, a dragged SpinRow) that must not rewrite the
    /// config file once per step.
    ///
    /// Coalesced: one Ctrl+scroll gesture delivers a train of 0.025 notches,
    /// and applying each one walked every pane and re-measured every VTE in it
    /// for a scale that was superseded milliseconds later. The scale the user
    /// is aiming at is recorded immediately — so anything reading it sees the
    /// live value — and the expensive widget sweep runs once, on the idle after
    /// the burst has been dispatched.
    pub(crate) fn apply_font_scale(&self, new_scale: f64) {
        self.font_scale.set(new_scale);
        self.config.borrow_mut().default_font_scale = new_scale;
        // The generic persist below is still 400 ms of debounce away, and the
        // config already differs from the file. Mark it dirty now so a reload
        // arriving inside a held Ctrl+wheel gesture cannot undo the zoom.
        self.config_dirty.record_edit();

        if claim_font_scale_sweep(&self.pending_font_scale, new_scale) {
            let ui = self.clone();
            glib::idle_add_local_once(move || {
                if let Some(scale) = ui.pending_font_scale.take() {
                    ui.set_font_scale_all(scale);
                }
            });
        }
        let generation = self.font_persist_generation.get().wrapping_add(1);
        self.font_persist_generation.set(generation);
        let ui = self.clone();
        glib::timeout_add_local_once(FONT_PERSIST_DEBOUNCE, move || {
            // A newer step superseded this one; it owns the write instead.
            if ui.font_persist_generation.get() == generation {
                ui.persist_config();
            }
        });
    }

    pub(crate) fn set_font_scale_all(&self, new_scale: f64) {
        self.font_scale.set(new_scale);
        for i in 0..self.notebook.n_pages() {
            if let Some(widget) = self.notebook.nth_page(Some(i)) {
                let Some(node) = PaneNode::from_widget(&widget) else {
                    continue;
                };
                for leaf in node.leaves() {
                    if let Some(view) = leaf.block_view() {
                        // Updates the live surface, every finished renderer,
                        // and the TermView config used by future blocks.
                        view.set_font_scale(new_scale);
                    } else {
                        leaf.terminal().set_font_scale(new_scale);
                    }
                }
            }
        }
    }

    pub(crate) fn for_each_terminal(&self, f: impl Fn(&Terminal)) {
        for i in 0..self.notebook.n_pages() {
            if let Some(widget) = self.notebook.nth_page(Some(i)) {
                let mut terms = Vec::new();
                collect_terminals(&widget, &mut terms);
                for term in terms {
                    f(&term);
                }
            }
        }
    }

    pub(crate) fn apply_colors_all(&self) {
        let config = self.config.borrow();
        let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
        self.for_each_terminal(|term| {
            term.set_colors(
                Some(&config.foreground),
                Some(&config.background),
                &palette_refs,
            );
            term.set_color_bold(None);
            term.set_color_cursor(Some(&config.cursor));
            term.set_color_cursor_foreground(Some(&config.cursor_foreground));
        });
        drop(config);
        self.apply_dynamic_css();
    }

    pub(crate) fn apply_dynamic_css(&self) {
        self.install_command_correction_monitor();
        let config = self.config.borrow();
        let bg = &config.background;
        // Some terminal palettes intentionally choose a softer foreground
        // than WCAG permits for application chrome. Keep the terminal color
        // untouched, but make labels, buttons, and status text readable.
        let semantic = config.semantic_colors();
        let fg = &semantic.foreground;
        let br = (bg.red() * 255.0) as u8;
        let bg_g = (bg.green() * 255.0) as u8;
        let bb = (bg.blue() * 255.0) as u8;
        let fr = (fg.red() * 255.0) as u8;
        let fg_g = (fg.green() * 255.0) as u8;
        let fb = (fg.blue() * 255.0) as u8;
        let muted = &semantic.muted;
        let (mut_r, mut_g, mut_b) = (
            (muted.red() * 255.0) as u8,
            (muted.green() * 255.0) as u8,
            (muted.blue() * 255.0) as u8,
        );
        // Terminal ANSI colors are allowed to be low contrast by design, but
        // Forge chrome is ordinary UI text. Keep each theme's hue while
        // adjusting it toward the foreground when necessary.
        let ok = &semantic.success;
        let (ok_r, ok_g, ok_b) = (
            (ok.red() * 255.0) as u8,
            (ok.green() * 255.0) as u8,
            (ok.blue() * 255.0) as u8,
        );
        let err = &semantic.error;
        let (err_r, err_g, err_b) = (
            (err.red() * 255.0) as u8,
            (err.green() * 255.0) as u8,
            (err.blue() * 255.0) as u8,
        );
        let warning = &semantic.warning;
        let (warn_r, warn_g, warn_b) = (
            (warning.red() * 255.0) as u8,
            (warning.green() * 255.0) as u8,
            (warning.blue() * 255.0) as u8,
        );
        let accent = &semantic.accent;
        let (acc_r, acc_g, acc_b) = (
            (accent.red() * 255.0) as u8,
            (accent.green() * 255.0) as u8,
            (accent.blue() * 255.0) as u8,
        );
        let info = &semantic.info;
        let (info_r, info_g, info_b) = (
            (info.red() * 255.0) as u8,
            (info.green() * 255.0) as u8,
            (info.blue() * 255.0) as u8,
        );
        let css = format!(
            ".terminal-box scrollbar {{ background-color: rgb({br},{bg_g},{bb}); }}
             .terminal-box scrollbar trough {{ background-color: rgb({br},{bg_g},{bb}); }}
             .terminal-box scrollbar slider {{ background-color: rgba({fr},{fg_g},{fb},0.4); }}
             .terminal-box scrollbar slider:hover {{ background-color: rgba({fr},{fg_g},{fb},0.7); }}
             .top-bar {{ background-color: rgb({br},{bg_g},{bb}); color: rgb({fr},{fg_g},{fb}); }}
             .top-bar button {{ color: rgb({fr},{fg_g},{fb}); }}
             .sidebar-box {{ background-color: rgb({br},{bg_g},{bb}); }}
             .sidebar-switcher, .sidebar-switcher button, .sidebar-switcher label,
             .file-tree-header, .file-tree-header button, .file-tree-header label,
             .file-tree-root {{ color: rgb({fr},{fg_g},{fb}); }}
             .file-tree-root {{ opacity: 1.0; }}
             .tab-strip-btn {{ color: rgb({mut_r},{mut_g},{mut_b}); }}
             .tab-strip-btn:checked {{ color: rgb({fr},{fg_g},{fb}); }}
             .tab-strip-btn.tab-marked {{ background-color: rgba({fr},{fg_g},{fb},0.2); font-weight: bold; }}
             .tab-bell, .tab-pin-icon, .tab-conn-dot.tab-connecting {{ color: rgb({warn_r},{warn_g},{warn_b}); }}
             .tab-conn-dot.tab-connected {{ color: rgb({ok_r},{ok_g},{ok_b}); }}
             .tab-conn-dot.tab-disconnected {{ color: rgb({err_r},{err_g},{err_b}); }}
             .pane-header-command {{ color: rgb({info_r},{info_g},{info_b}); }}
             .tab-strip-search {{ color: rgb({fr},{fg_g},{fb}); }}
             .tab-strip-search text {{ color: rgb({fr},{fg_g},{fb}); caret-color: rgb({fr},{fg_g},{fb}); }}
             .ai-panel {{
                 min-width: 240px;
                 background-color: rgb({br},{bg_g},{bb});
                 color: rgb({fr},{fg_g},{fb});
                 border-left: 1px solid rgba({fr},{fg_g},{fb},0.16);
             }}
             .ai-panel-header {{
                 padding: 10px 10px 8px 10px;
                 border-bottom: 1px solid rgba({fr},{fg_g},{fb},0.12);
             }}
             .ai-panel-title {{ color: rgb({fr},{fg_g},{fb}); font-weight: 700; }}
             .ai-panel-subtitle {{ color: rgb({mut_r},{mut_g},{mut_b}); font-size: 0.86em; }}
             .ai-chat-header-button {{ min-width: 30px; min-height: 30px; padding: 4px; }}
             .ai-chat-library {{ background-color: rgb({br},{bg_g},{bb}); }}
             .ai-chat-library-toolbar {{
                 padding: 10px;
                 border-bottom: 1px solid rgba({fr},{fg_g},{fb},0.12);
             }}
             .ai-chat-search {{ color: rgb({fr},{fg_g},{fb}); }}
             .ai-chat-search text {{
                 color: rgb({fr},{fg_g},{fb});
                 caret-color: rgb({fr},{fg_g},{fb});
             }}
             .ai-chat-list {{
                 margin: 8px;
                 background-color: transparent;
             }}
             .ai-chat-row {{
                 color: rgb({fr},{fg_g},{fb});
                 border-radius: 8px;
                 margin: 2px 0;
             }}
             .ai-chat-row:hover {{ background-color: rgba({fr},{fg_g},{fb},0.08); }}
             .ai-chat-row.active {{ background-color: rgba({fr},{fg_g},{fb},0.14); }}
             .ai-chat-row.archived {{ color: rgb({mut_r},{mut_g},{mut_b}); }}
             .ai-chat-row.unread {{ font-weight: 700; }}
             .ai-chat-row.error {{ color: rgb({err_r},{err_g},{err_b}); }}
             .ai-chat-section {{
                 color: rgb({mut_r},{mut_g},{mut_b});
                 font-size: 0.82em;
                 font-weight: 700;
                 padding: 8px 8px 4px 8px;
             }}
             .ai-chat-empty {{ color: rgb({mut_r},{mut_g},{mut_b}); padding: 28px; }}
             .ai-transcript, .ai-transcript text {{
                 background-color: rgb({br},{bg_g},{bb});
                 color: rgb({fr},{fg_g},{fb});
             }}
             .ai-empty-state {{ color: rgb({mut_r},{mut_g},{mut_b}); padding: 24px; }}
             .ai-empty-title {{ color: rgb({fr},{fg_g},{fb}); font-weight: 700; font-size: 1.08em; }}
             .ai-empty-actions {{ margin: 4px 0; }}
             .ai-empty-action {{
                 min-height: 32px;
                 color: rgb({fr},{fg_g},{fb});
                 border: 1px solid rgba({fr},{fg_g},{fb},0.16);
                 border-radius: 8px;
             }}
             .ai-panel-status-row {{
                 min-height: 22px;
                 padding: 2px 10px 4px 10px;
                 color: rgb({mut_r},{mut_g},{mut_b});
             }}
             .ai-panel-status-row.error {{ color: rgb({err_r},{err_g},{err_b}); }}
             .ai-status-action {{ min-height: 28px; padding: 2px 8px; }}
             .ai-panel-composer {{
                 padding: 8px;
                 border-top: 1px solid rgba({fr},{fg_g},{fb},0.12);
             }}
             .ai-context-chip {{
                 padding: 5px 8px;
                 color: rgb({mut_r},{mut_g},{mut_b});
                 background-color: rgba({fr},{fg_g},{fb},0.07);
                 border: 1px solid rgba({fr},{fg_g},{fb},0.16);
                 border-radius: 9px;
             }}
             .ai-context-label {{ font-size: 0.88em; }}
             .ai-context-clear {{ min-height: 24px; padding: 1px 6px; }}
             .ai-panel-input {{
                 background-color: rgba({fr},{fg_g},{fb},0.06);
                 border: 1px solid rgba({fr},{fg_g},{fb},0.20);
                 border-radius: 10px;
             }}
             .ai-panel-input textview, .ai-panel-input text {{
                 background-color: transparent;
                 color: rgb({fr},{fg_g},{fb});
                 caret-color: rgb({fr},{fg_g},{fb});
             }}
             .ai-input-placeholder {{ color: rgb({mut_r},{mut_g},{mut_b}); padding: 8px; }}
             .ai-input-hint {{ color: rgb({mut_r},{mut_g},{mut_b}); font-size: 0.82em; }}
             .ai-send-button {{ min-width: 72px; min-height: 32px; }}
             .agent-surface {{
                 background-color: rgb({br},{bg_g},{bb});
                 color: rgb({fr},{fg_g},{fb});
             }}
             .agent-surface headerbar {{
                 background-color: rgb({br},{bg_g},{bb});
                 color: rgb({fr},{fg_g},{fb});
                 box-shadow: none;
             }}
             .agent-dashboard {{
                 background-color: rgb({br},{bg_g},{bb});
                 color: rgb({fr},{fg_g},{fb});
             }}
             .agent-overview, .agent-setting-card, .agent-status-card,
             .agent-composer, .agent-transcript-card {{
                 background-color: rgba({fr},{fg_g},{fb},0.055);
                 border: 1px solid rgba({fr},{fg_g},{fb},0.14);
                 border-radius: 12px;
             }}
             .agent-context-card {{
                 padding: 8px 10px;
                 background-color: rgba({fr},{fg_g},{fb},0.045);
                 border: 1px solid rgba({fr},{fg_g},{fb},0.12);
                 border-radius: 9px;
             }}
             .agent-overview {{ padding: 12px; }}
             .agent-icon {{
                 color: rgb({acc_r},{acc_g},{acc_b});
                 background-color: alpha(@accent_bg_color, 0.18);
                 border-radius: 10px;
                 padding: 8px;
             }}
             .agent-chip {{
                 color: rgb({mut_r},{mut_g},{mut_b});
                 background-color: rgba({fr},{fg_g},{fb},0.08);
                 border-radius: 999px;
                 padding: 4px 9px;
                 font-size: 0.82em;
             }}
             .agent-safety-chip {{
                 color: rgb({ok_r},{ok_g},{ok_b});
                 background-color: alpha(@success_bg_color, 0.14);
             }}
             .agent-setting-card {{ padding: 10px 12px; }}
             .agent-section-label {{
                 color: rgb({mut_r},{mut_g},{mut_b});
                 font-size: 0.78em;
                 font-weight: 700;
                 padding: 9px 11px 7px 11px;
                 border-bottom: 1px solid rgba({fr},{fg_g},{fb},0.10);
             }}
             .agent-transcript, .agent-transcript text {{
                 background-color: transparent;
                 color: rgb({fr},{fg_g},{fb});
             }}
             .agent-status-card {{ padding: 9px 11px; }}
             .agent-status {{ color: rgb({mut_r},{mut_g},{mut_b}); }}
             .agent-status-card progressbar trough {{
                 min-height: 4px;
                 background-color: rgba({fr},{fg_g},{fb},0.10);
                 border-radius: 999px;
             }}
             .agent-status-card progressbar progress {{
                 min-height: 4px;
                 background-color: @accent_bg_color;
                 border-radius: 999px;
             }}
             .agent-proposal-card {{
                 padding: 12px;
                 color: rgb({fr},{fg_g},{fb});
                 background-color: rgb({br},{bg_g},{bb});
                 border: 1px solid rgba({warn_r},{warn_g},{warn_b},0.48);
                 border-radius: 12px;
                 box-shadow: none;
             }}
             .agent-proposal-card .command-review {{
                 color: rgb({fr},{fg_g},{fb});
             }}
             .agent-danger-command {{
                 padding: 8px;
                 font-family: monospace;
                 background-color: alpha(@warning_bg_color, 0.16);
                 border-radius: 7px;
             }}
             .agent-composer {{ padding: 9px; }}
             .agent-input {{
                 min-height: 34px;
                 color: rgb({fr},{fg_g},{fb});
                 caret-color: rgb({fr},{fg_g},{fb});
                 background-color: rgba({br},{bg_g},{bb},0.62);
                 border-color: rgba({fr},{fg_g},{fb},0.20);
             }}
             .agent-input text {{
                 color: rgb({fr},{fg_g},{fb});
                 caret-color: rgb({fr},{fg_g},{fb});
             }}
             .agent-input placeholder, .agent-input text placeholder {{
                 color: rgb({mut_r},{mut_g},{mut_b});
             }}
             .agent-input:disabled, .agent-input:disabled text {{
                 color: rgb({mut_r},{mut_g},{mut_b});
             }}
             .agent-turn-label {{
                 color: rgb({mut_r},{mut_g},{mut_b});
             }}
             .agent-send {{ min-width: 72px; min-height: 34px; }}
             .agent-input-hint {{
                 color: rgb({mut_r},{mut_g},{mut_b});
                 font-size: 0.82em;
             }}
             .bottom-bar {{
                 background-color: rgb({br},{bg_g},{bb});
                 color: rgb({fr},{fg_g},{fb});
             }}
             .bottom-bar .bb-normal {{ color: rgb({fr},{fg_g},{fb}); }}
             .bottom-bar .bb-muted {{ color: rgb({mut_r},{mut_g},{mut_b}); }}
             .bottom-bar .bb-ok {{ color: rgb({ok_r},{ok_g},{ok_b}); }}
             .bottom-bar .bb-err {{ color: rgb({err_r},{err_g},{err_b}); }}"
        );
        self.scrollbar_css.load_from_string(&css);
        self.ai_panel.apply_theme_colors();
    }

    pub(crate) fn apply_font_all(&self) {
        let config = self.config.borrow();
        let desc = config.font_desc.clone();
        let font_desc = crate::font::terminal_font_description(&desc, &config);
        drop(config);
        for i in 0..self.notebook.n_pages() {
            if let Some(widget) = self.notebook.nth_page(Some(i)) {
                let Some(node) = PaneNode::from_widget(&widget) else {
                    continue;
                };
                for leaf in node.leaves() {
                    if let Some(view) = leaf.block_view() {
                        view.set_font(&desc);
                    } else {
                        leaf.terminal().set_font(Some(&font_desc));
                    }
                }
            }
        }
    }

    pub(crate) fn apply_scrollback_all(&self) {
        let lines = self.config.borrow().terminal_scrollback_lines;
        self.for_each_terminal(|term| {
            term.set_scrollback_lines(lines as i64);
        });
    }

    pub(crate) fn apply_theme(&self, theme: &Theme) {
        {
            let mut config = self.config.borrow_mut();
            config.theme_name = theme.name.clone();
            config.foreground = theme.foreground;
            config.background = theme.background;
            config.cursor = theme.cursor;
            config.cursor_foreground = theme.cursor_foreground;
            config.palette = theme.palette;
        }
        self.apply_colors_all();
    }

    /// Reload configuration from disk and apply changes.
    pub(crate) fn reload_config(&self) {
        self.reload_config_attempt(CONFIG_REVISION_READ_ATTEMPTS);
    }

    /// Tell the user their in-window settings and the file have diverged, and
    /// let them pick which one survives.
    ///
    /// Neither answer is taken by default. Escape and the close gesture both
    /// resolve to keeping the unsaved edit, because discarding a change the
    /// user made is the destructive half and must be asked for out loud. The
    /// pending write is then refused by the store's revision check rather than
    /// silently overwriting the other program's file, which is the explicit
    /// resolution path the wording points at.
    fn show_config_reload_conflict(&self, path: &std::path::Path) {
        if self.config_reload_conflict_visible.replace(true) {
            return;
        }
        let path_display =
            jterm_core::review_input::safe_inline_display(&path.to_string_lossy(), 2 * 1024);
        let dialog = adw::AlertDialog::new(
            Some("Configuration changed on disk"),
            Some(&format!(
                "{path_display} was changed elsewhere while this window still had settings waiting to be written.\n\nKeep this window's settings and its pending save will be refused until you reload, so nothing is overwritten without you seeing it. Discard them to take the file's settings instead."
            )),
        );
        dialog.add_response("keep", "Keep My Settings");
        dialog.add_response("discard", "Discard and Reload");
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("keep"));
        dialog.set_close_response("keep");
        let visible = self.config_reload_conflict_visible.clone();
        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            visible.set(false);
            if response == "discard" {
                ui.discard_unsaved_config_edits_and_reload();
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Take the file's settings, deliberately losing this window's unsaved ones.
    fn discard_unsaved_config_edits_and_reload(&self) {
        // Both debounced writers are cancelled first. A timer still armed would
        // fire after the reload and write back the very snapshot the user just
        // chose to discard, turning an explicit answer into a silent reversal.
        self.config_persist_generation
            .set(self.config_persist_generation.get().wrapping_add(1));
        self.font_persist_generation
            .set(self.font_persist_generation.get().wrapping_add(1));
        // Likewise the queued font-zoom widget sweep: the reload sets every
        // pane's scale from the file, and a pending sweep would re-apply the
        // discarded one to the widgets while the config said otherwise.
        self.pending_font_scale.set(None);
        self.config_dirty.abandon_unsaved_edits();
        self.reload_config();
    }

    fn reload_config_attempt(&self, attempts_left: u8) {
        if std::env::var_os("FORGE_SAFE_MODE").is_some() {
            let dialog = adw::AlertDialog::new(
                Some("Configuration reload disabled"),
                Some("Safe mode keeps the built-in VTE profile isolated from user configuration."),
            );
            dialog.add_response("ok", "OK");
            dialog.set_default_response(Some("ok"));
            dialog.present(Some(&self.window));
            return;
        }
        let path = config_file_path();
        let contents = match crate::config_store::read_config_text(&path) {
            Ok(contents) => contents,
            Err(error) => {
                let path_display = jterm_core::review_input::safe_inline_display(
                    &path.to_string_lossy(),
                    2 * 1024,
                );
                log::error!("Config reload rejected for {path_display}: {error}");
                self.show_config_error(
                    "Configuration reload rejected",
                    &format!("The current settings remain active. {path_display}: {error}"),
                );
                return;
            }
        };
        let disk_revision = contents
            .as_deref()
            .map_or_else(crate::config_store::ConfigRevision::missing, |contents| {
                crate::config_store::ConfigRevision::from_bytes(contents.as_bytes())
            });
        let live_revision = {
            let config = self.config.borrow();
            live_config_revision(&config)
        };
        let Some(live_revision) = live_revision else {
            // The persistence worker is inside its short critical section on
            // the revision slot. Nothing is decided on a guess; come back once
            // it has published, and say so rather than reloading blind if the
            // slot somehow stays busy.
            if attempts_left > 0 {
                let ui = self.clone();
                glib::timeout_add_local_once(CONFIG_REVISION_RETRY_DELAY, move || {
                    ui.reload_config_attempt(attempts_left - 1);
                });
            } else {
                self.show_config_error(
                    "Configuration reload could not run",
                    "The settings file is being written right now. The current settings remain active; reload again (Ctrl+Shift+R) once the save has finished.",
                );
            }
            return;
        };
        match decide_config_reload(
            live_revision.as_ref(),
            &disk_revision,
            self.config_dirty.has_unsaved_edits(),
        ) {
            ConfigReloadDecision::Skip => {
                log::debug!("Config reload skipped: file matches the current in-memory revision");
                return;
            }
            ConfigReloadDecision::Conflict => {
                log::warn!("Config reload deferred: unsaved settings would be discarded");
                self.show_config_reload_conflict(&path);
                return;
            }
            ConfigReloadDecision::Apply => {}
        }
        let validation = match contents.as_deref() {
            Some(contents) => validate_config_contents(contents).map_err(|error| {
                crate::config::config_syntax_diagnostic(contents, &error).to_string()
            }),
            None => Ok(Vec::new()),
        };
        {
            match validation {
                Ok(issues) if issues.iter().any(|issue| issue.is_error()) => {
                    let details = issues
                        .iter()
                        .filter(|issue| issue.is_error())
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n");
                    for issue in issues {
                        log::error!("Config reload rejected: {issue}");
                    }
                    self.show_config_error(
                        "Configuration reload rejected",
                        &format!(
                            "The current settings remain active. Fix these errors first:\n\n{details}"
                        ),
                    );
                    return;
                }
                Err(err) => {
                    let path_display = jterm_core::review_input::safe_inline_display(
                        &path.to_string_lossy(),
                        2 * 1024,
                    );
                    log::error!("Config reload rejected for {path_display}: {err}");
                    self.show_config_error(
                        "Configuration reload rejected",
                        &format!("The current settings remain active. {path_display}: {err}"),
                    );
                    return;
                }
                _ => {}
            }
        }
        let (new_config, _themes, new_keybindings) = load_config();
        // `load_config` re-read the file. Prove it read the same bytes this
        // reload validated, and that it read them at all, before any of them
        // reaches a live setting.
        if let Some(reason) = reload_read_failure(
            crate::config::load_error(),
            live_config_revision(&new_config).flatten().as_ref(),
            &disk_revision,
        ) {
            let path_display =
                jterm_core::review_input::safe_inline_display(&path.to_string_lossy(), 2 * 1024);
            log::error!("Config reload abandoned for {path_display}: {reason}");
            self.show_config_error(
                "Configuration reload abandoned",
                &format!(
                    "The current settings remain active. {path_display}: {reason}. Nothing was reset to its default."
                ),
            );
            return;
        }
        let opacity = new_config.window_opacity;
        let font_scale = new_config.default_font_scale;
        let tab_placement = new_config.tab_placement;
        let sidebar_view = new_config.sidebar_view;
        let sidebar_visible = new_config.sidebar_visible;
        let ai_visible = new_config.ai_enabled && new_config.ai_panel_visible;
        let previous_remote_hosts = self.config.borrow().remote_hosts.clone();

        // New panes/tabs immediately use a changed shell; all other config is
        // replaced as one coherent snapshot instead of retaining stale fields.
        *self.shell_argv.borrow_mut() = choose_shell_argv(new_config.shell.as_deref());
        *self.config.borrow_mut() = new_config;

        // Preserve an index-backed tree location only when its exact profile
        // still exists. A reorder is remapped; replacement/removal returns to
        // Local instead of redirecting already-visible rows to another host.
        self.reconcile_file_tree_remote_hosts(&previous_remote_hosts);

        // TermView owns a shared clone used by long-lived callbacks. Refresh it
        // as well so behavior changes do not require reopening block tabs.
        self.sync_block_configs();

        // Apply all visual changes
        self.window_opacity.set(opacity);
        self.window.set_opacity(opacity);
        self.set_font_scale_all(font_scale);
        self.apply_font_all();
        self.apply_colors_all();
        self.apply_scrollback_all();

        self.tab_placement.set(tab_placement);
        self.sidebar_view.set(sidebar_view);
        self.apply_tab_placement();
        self.set_sidebar_visible(sidebar_visible, false);
        self.sync_bottom_bar_visibility();

        self.ai_panel_visible.set(ai_visible);
        // One attach/detach funnel: the side stack holds both right-side
        // panels, and the Tasks panel keeps its page while it is open.
        self.sync_side_panel();
        if ai_visible {
            self.restore_ai_panel_width();
        }
        self.ai_panel.refresh_config_display();
        self.ai_panel.refresh_persisted_privacy();
        self.sync_agent_toggle();

        // Update keybindings
        *self.keybinding_map.borrow_mut() = new_keybindings;

        log::info!("Configuration reloaded from disk");
    }
}

/// Record the scale a coalesced font-zoom sweep should apply, and report
/// whether this call has to start that sweep.
///
/// `true` exactly once per burst: the first notch schedules the idle, every
/// later notch only moves the target that idle will read.
fn claim_font_scale_sweep(pending: &Cell<Option<f64>>, scale: f64) -> bool {
    pending.replace(Some(scale)).is_none()
}

#[cfg(test)]
mod tests {
    use super::{
        claim_font_scale_sweep, decide_config_reload, reload_matches_live_revision,
        reload_read_failure, ConfigDirtyEpoch, ConfigReloadDecision,
    };
    use crate::config_store::ConfigRevision;
    use std::cell::Cell;

    /// A wheel gesture arrives as a train of 0.025 notches. Exactly one of them
    /// may schedule the widget sweep, and the sweep must run at the scale the
    /// user actually stopped on.
    #[test]
    fn a_font_zoom_burst_schedules_exactly_one_sweep() {
        let pending: Cell<Option<f64>> = Cell::new(None);

        assert!(claim_font_scale_sweep(&pending, 1.025));
        for scale in [1.05, 1.075, 1.1] {
            assert!(
                !claim_font_scale_sweep(&pending, scale),
                "a queued sweep is retargeted, never duplicated"
            );
        }
        assert_eq!(
            pending.take(),
            Some(1.1),
            "the sweep applies the last scale in the burst"
        );

        // The next gesture, after the idle consumed the target, schedules again.
        assert!(claim_font_scale_sweep(&pending, 1.125));
    }

    #[test]
    fn reload_skips_matching_present_revision() {
        let revision = ConfigRevision::from_bytes(b"theme = 'light'\n");
        assert!(reload_matches_live_revision(Some(&revision), &revision));
    }

    #[test]
    fn reload_skips_matching_missing_revision() {
        let revision = ConfigRevision::missing();
        assert!(reload_matches_live_revision(Some(&revision), &revision));
    }

    #[test]
    fn reload_keeps_external_revision_changes_visible() {
        let live = ConfigRevision::from_bytes(b"theme = 'light'\n");
        let disk = ConfigRevision::from_bytes(b"theme = 'dark'\n");
        assert!(!reload_matches_live_revision(Some(&live), &disk));
        assert!(!reload_matches_live_revision(None, &disk));
    }

    /// The 200 ms watch debounce racing the 250 ms persist debounce, replayed
    /// as state instead of as sleeps. The reload arrives while the user's edit
    /// exists only in memory; adopting the file there is what silently threw
    /// the edit away, and the debounced write then persisted the reloaded
    /// snapshot so nothing on screen ever mentioned it.
    #[test]
    fn an_external_reload_inside_the_persist_debounce_conflicts_instead_of_discarding() {
        let dirty = ConfigDirtyEpoch::default();
        let live = ConfigRevision::from_bytes(b"opacity = 0.9\n");
        let disk = ConfigRevision::from_bytes(b"opacity = 0.5\n");

        // Nothing pending: an external change is adopted, exactly as before.
        assert!(!dirty.has_unsaved_edits());
        assert_eq!(
            decide_config_reload(Some(&live), &disk, dirty.has_unsaved_edits()),
            ConfigReloadDecision::Apply
        );

        // t+0 ms: the user changes a setting. Its write is 250 ms away.
        dirty.record_edit();
        assert!(dirty.has_unsaved_edits());

        // t+200 ms: the watcher fires for somebody else's write.
        assert_eq!(
            decide_config_reload(Some(&live), &disk, dirty.has_unsaved_edits()),
            ConfigReloadDecision::Conflict
        );
        // Bytes this window already accounts for are never a conflict; the
        // pending edit simply stays pending.
        assert_eq!(
            decide_config_reload(Some(&live), &live, dirty.has_unsaved_edits()),
            ConfigReloadDecision::Skip
        );

        // t+250 ms: the snapshot reaches the worker. Queued is not written, so
        // only the worker's success may clear the epoch.
        let commit = dirty.commit_handle();
        assert!(dirty.has_unsaved_edits());
        commit.commit();
        assert!(!dirty.has_unsaved_edits());
        assert_eq!(
            decide_config_reload(Some(&live), &disk, dirty.has_unsaved_edits()),
            ConfigReloadDecision::Apply
        );
    }

    /// A write clears exactly the edit its own snapshot carried. Anything
    /// changed after that snapshot was taken is still only in memory, and a
    /// late completion must not walk the clean mark backwards either.
    #[test]
    fn a_write_only_clears_the_edit_its_own_snapshot_carried() {
        let dirty = ConfigDirtyEpoch::default();
        dirty.record_edit();
        let in_flight = dirty.commit_handle();
        dirty.record_edit();
        in_flight.commit();
        assert!(
            dirty.has_unsaved_edits(),
            "the edit made while the write was in flight is still unsaved"
        );

        let stale = dirty.commit_handle();
        dirty.record_edit();
        dirty.commit_handle().commit();
        assert!(!dirty.has_unsaved_edits());
        stale.commit();
        assert!(
            !dirty.has_unsaved_edits(),
            "an older write completing late cannot un-save a newer one"
        );

        // The conflict dialog's destructive answer, which must leave nothing
        // pending for the cancelled debounce timers to resurrect.
        dirty.record_edit();
        assert!(dirty.has_unsaved_edits());
        dirty.abandon_unsaved_edits();
        assert!(!dirty.has_unsaved_edits());
    }

    /// The reload's second read is the one that produces the `Config`. If it
    /// fails, `load_file_config` answers with `FileConfig::default()`; if the
    /// file moved between the two reads, it produces bytes nothing validated.
    /// Both used to replace every live setting without a word.
    #[test]
    fn a_failed_or_raced_second_read_refuses_to_replace_the_live_settings() {
        let validated = ConfigRevision::from_bytes(b"theme = 'dark'\n");
        assert_eq!(
            reload_read_failure(None, Some(&validated), &validated),
            None
        );

        assert_eq!(
            reload_read_failure(
                Some("/home/u/.config/forge/config.toml: permission denied".to_string()),
                Some(&validated),
                &validated
            )
            .as_deref(),
            Some("/home/u/.config/forge/config.toml: permission denied")
        );

        let raced = ConfigRevision::from_bytes(b"theme = 'light'\n");
        assert!(reload_read_failure(None, Some(&raced), &validated).is_some());
        assert!(
            reload_read_failure(None, None, &validated).is_some(),
            "a loader that recorded no revision at all proves nothing"
        );
    }
}
