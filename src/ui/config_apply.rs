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
use crate::config::{load_config, Theme};
use crate::terminal::collect_terminals;

impl UiState {
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
             .tab-strip-btn {{ color: rgba({fr},{fg_g},{fb},0.6); }}
             .tab-strip-btn:checked {{ color: rgb({fr},{fg_g},{fb}); }}
             .tab-strip-btn.tab-marked {{ background-color: rgba({fr},{fg_g},{fb},0.2); font-weight: bold; }}
             .tab-strip-search {{ color: rgb({fr},{fg_g},{fb}); }}
             .tab-strip-search text {{ color: rgb({fr},{fg_g},{fb}); caret-color: rgb({fr},{fg_g},{fb}); }}"
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
        let (new_config, themes, new_keybindings) = load_config();

        // Apply theme (finds the theme by name from the fresh theme list)
        let theme = themes
            .iter()
            .find(|t| t.name == new_config.theme_name)
            .unwrap_or(&themes[0])
            .clone();

        {
            let mut config = self.config.borrow_mut();
            config.window_opacity = new_config.window_opacity;
            config.terminal_scrollback_lines = new_config.terminal_scrollback_lines;
            config.font_desc = new_config.font_desc;
            config.default_font_scale = new_config.default_font_scale;
            config.theme_name = new_config.theme_name;
            config.foreground = theme.foreground;
            config.background = theme.background;
            config.cursor = theme.cursor;
            config.cursor_foreground = theme.cursor_foreground;
            config.palette = theme.palette;
            config.startup_commands = new_config.startup_commands;
        }

        // Apply all visual changes
        self.window_opacity.set(new_config.window_opacity);
        self.window.set_opacity(new_config.window_opacity);
        self.set_font_scale_all(new_config.default_font_scale);
        self.apply_font_all();
        self.apply_colors_all();
        self.apply_scrollback_all();

        // Update keybindings
        *self.keybinding_map.borrow_mut() = new_keybindings;

        log::info!("Configuration reloaded from disk");
    }
}
