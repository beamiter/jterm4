//! config_apply — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::gdk::RGBA;
use gtk4::pango::FontDescription;
use libadwaita as adw;
use std::rc::Rc;
use vte4::Terminal;
use vte4::{TerminalExt, TerminalExtManual};

use super::*;
use crate::block_view::TermView;
use crate::config::{
    choose_shell_argv, config_file_path, load_config, validate_config_contents, Theme,
};
use crate::terminal::collect_terminals;

impl UiState {
    fn show_config_error(&self, title: &str, message: &str) {
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
        if std::env::var_os("JTERM4_SAFE_MODE").is_some() {
            self.show_config_error(
                "Temporary safe-mode setting",
                "This change applies only to the current window and will not be saved.",
            );
            return;
        }
        let result = crate::config::save_config(&self.config.borrow());
        let Err(error) = result else {
            return;
        };
        self.show_config_error(
            "Settings were not saved",
            &format!(
                "{error}\n\nThe in-memory setting is still active. Reload the configuration (Ctrl+Shift+R) before trying again if the file changed elsewhere."
            ),
        );
    }

    /// Push the current behavioral configuration into every live Block pane,
    /// including panes nested under splits.
    pub(crate) fn sync_block_configs(&self) {
        for page in 0..self.notebook.n_pages() {
            let Some(widget) = self.notebook.nth_page(Some(page)) else {
                continue;
            };
            let Some(node) = PaneNode::from_widget(&widget) else {
                continue;
            };
            for leaf in node.leaves() {
                if let Some(view) = leaf.block_view() {
                    view.reload_config(&self.config.borrow());
                }
            }
        }
    }

    pub(crate) fn set_font_scale_all(&self, new_scale: f64) {
        self.font_scale.set(new_scale);
        for i in 0..self.notebook.n_pages() {
            if let Some(widget) = self.notebook.nth_page(Some(i)) {
                // Update TermView if present
                if let Some(term_view) = unsafe { widget.data::<Rc<TermView>>("term-view") } {
                    let term_view = unsafe { term_view.as_ref() };
                    term_view.set_font_scale(new_scale);
                }
                // Also update any standalone VTE terminals (for split panes)
                let mut terms = Vec::new();
                collect_terminals(&widget, &mut terms);
                for term in terms {
                    term.set_font_scale(new_scale);
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
        let fg = &config.foreground;
        let br = (bg.red() * 255.0) as u8;
        let bg_g = (bg.green() * 255.0) as u8;
        let bb = (bg.blue() * 255.0) as u8;
        let fr = (fg.red() * 255.0) as u8;
        let fg_g = (fg.green() * 255.0) as u8;
        let fb = (fg.blue() * 255.0) as u8;
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
             .tab-strip-btn {{ color: rgba({fr},{fg_g},{fb},0.6); }}
             .tab-strip-btn:checked {{ color: rgb({fr},{fg_g},{fb}); }}
             .tab-strip-btn.tab-marked {{ background-color: rgba({fr},{fg_g},{fb},0.2); font-weight: bold; }}
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
             .ai-panel-subtitle {{ color: rgba({fr},{fg_g},{fb},0.60); font-size: 0.86em; }}
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
             .ai-chat-row.archived {{ color: rgba({fr},{fg_g},{fb},0.62); }}
             .ai-chat-row.unread {{ font-weight: 700; }}
             .ai-chat-row.error {{ color: @error_color; }}
             .ai-chat-section {{
                 color: rgba({fr},{fg_g},{fb},0.56);
                 font-size: 0.82em;
                 font-weight: 700;
                 padding: 8px 8px 4px 8px;
             }}
             .ai-chat-empty {{ color: rgba({fr},{fg_g},{fb},0.56); padding: 28px; }}
             .ai-transcript, .ai-transcript text {{
                 background-color: rgb({br},{bg_g},{bb});
                 color: rgb({fr},{fg_g},{fb});
             }}
             .ai-empty-state {{ color: rgba({fr},{fg_g},{fb},0.62); padding: 24px; }}
             .ai-empty-title {{ color: rgba({fr},{fg_g},{fb},0.88); font-weight: 700; font-size: 1.08em; }}
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
                 color: rgba({fr},{fg_g},{fb},0.66);
             }}
             .ai-panel-status-row.error {{ color: @error_color; }}
             .ai-status-action {{ min-height: 28px; padding: 2px 8px; }}
             .ai-panel-composer {{
                 padding: 8px;
                 border-top: 1px solid rgba({fr},{fg_g},{fb},0.12);
             }}
             .ai-context-chip {{
                 padding: 5px 8px;
                 color: rgba({fr},{fg_g},{fb},0.82);
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
             .ai-input-placeholder {{ color: rgba({fr},{fg_g},{fb},0.44); padding: 8px; }}
             .ai-input-hint {{ color: rgba({fr},{fg_g},{fb},0.52); font-size: 0.82em; }}
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
                 color: @accent_color;
                 background-color: alpha(@accent_bg_color, 0.18);
                 border-radius: 10px;
                 padding: 8px;
             }}
             .agent-chip {{
                 color: rgba({fr},{fg_g},{fb},0.78);
                 background-color: rgba({fr},{fg_g},{fb},0.08);
                 border-radius: 999px;
                 padding: 4px 9px;
                 font-size: 0.82em;
             }}
             .agent-safety-chip {{
                 color: @success_color;
                 background-color: alpha(@success_bg_color, 0.14);
             }}
             .agent-setting-card {{ padding: 10px 12px; }}
             .agent-section-label {{
                 color: rgba({fr},{fg_g},{fb},0.58);
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
             .agent-status {{ color: rgba({fr},{fg_g},{fb},0.78); }}
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
                 border: 1px solid alpha(@warning_color, 0.48);
             }}
             .agent-danger-command {{
                 padding: 8px;
                 font-family: monospace;
                 background-color: alpha(@warning_bg_color, 0.16);
                 border-radius: 7px;
             }}
             .agent-composer {{ padding: 9px; }}
             .agent-input {{ min-height: 34px; }}
             .agent-send {{ min-width: 72px; min-height: 34px; }}
             .agent-input-hint {{ font-size: 0.82em; }}"
        );
        self.scrollbar_css.load_from_string(&css);
    }

    pub(crate) fn apply_font_all(&self) {
        let config = self.config.borrow();
        let font_desc = FontDescription::from_string(&config.font_desc);
        drop(config);
        for i in 0..self.notebook.n_pages() {
            if let Some(widget) = self.notebook.nth_page(Some(i)) {
                // Update TermView if present
                if let Some(term_view) = unsafe { widget.data::<Rc<TermView>>("term-view") } {
                    let term_view = unsafe { term_view.as_ref() };
                    term_view.set_font(&font_desc);
                }
                // Also update any standalone VTE terminals (for split panes)
                let mut terms = Vec::new();
                collect_terminals(&widget, &mut terms);
                for term in terms {
                    term.set_font(Some(&font_desc));
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
        if std::env::var_os("JTERM4_SAFE_MODE").is_some() {
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
        if path.exists() {
            let validation = std::fs::read_to_string(&path)
                .map_err(|err| err.to_string())
                .and_then(|contents| {
                    validate_config_contents(&contents).map_err(|err| err.to_string())
                });
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
                    log::error!("Config reload rejected for {}: {err}", path.display());
                    self.show_config_error(
                        "Configuration reload rejected",
                        &format!(
                            "The current settings remain active. {}: {err}",
                            path.display()
                        ),
                    );
                    return;
                }
                _ => {}
            }
        }
        let (new_config, _themes, new_keybindings) = load_config();
        let opacity = new_config.window_opacity;
        let font_scale = new_config.default_font_scale;
        let tab_placement = new_config.tab_placement;
        let sidebar_view = new_config.sidebar_view;
        let sidebar_visible = new_config.sidebar_visible;
        let ai_visible = new_config.ai_enabled && new_config.ai_panel_visible;

        // New panes/tabs immediately use a changed shell; all other config is
        // replaced as one coherent snapshot instead of retaining stale fields.
        *self.shell_argv.borrow_mut() = choose_shell_argv(new_config.shell.as_deref());
        *self.config.borrow_mut() = new_config;

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

        self.ai_panel_visible.set(ai_visible);
        if ai_visible {
            self.ai_paned.set_end_child(Some(&self.ai_panel.root));
            self.restore_ai_panel_width();
        } else {
            self.ai_paned.set_end_child(None::<&gtk4::Widget>);
        }
        self.ai_panel.refresh_config_display();
        self.ai_panel.refresh_persisted_privacy();
        self.sync_agent_toggle();

        // Update keybindings
        *self.keybinding_map.borrow_mut() = new_keybindings;

        log::info!("Configuration reloaded from disk");
    }
}
