//! layout — tab placement (sidebar vs top bar) management for UiState.
use gtk4::prelude::*;
use gtk4::{Orientation, ToggleButton};

use super::*;
use crate::config::{SidebarView, TabPlacement};

impl UiState {
    /// Move the tab strip into the holder matching the current placement and
    /// adjust orientation, per-button sizing, and top-bar spacer behavior.
    pub(crate) fn apply_tab_placement(&self) {
        let placement = self.tab_placement.get();

        // Detach the strip from whichever scroll holder currently owns it.
        self.tab_strip_scroll.set_child(None::<&gtk4::Widget>);
        self.top_tab_scroll.set_child(None::<&gtk4::Widget>);
        if let Some(parent) = self.tab_search_wrapper.parent() {
            if let Ok(parent) = parent.downcast::<gtk4::Box>() {
                parent.remove(&self.tab_search_wrapper);
            }
        }

        match placement {
            TabPlacement::Sidebar => {
                self.tab_strip.set_orientation(Orientation::Vertical);
                self.tab_strip.set_valign(gtk4::Align::Start);
                self.tab_strip.set_hexpand(false);
                self.tab_strip.set_vexpand(true);
                self.tab_strip.remove_css_class("top-tabs");
                self.tab_strip_scroll.set_child(Some(&self.tab_strip));
                self.sidebar_tab_search_holder
                    .append(&self.tab_search_wrapper);
                self.top_spacer.set_hexpand(true);
                self.sidebar_tab_search_holder.set_visible(true);
                self.top_tab_search_holder.set_visible(false);
            }
            TabPlacement::TopBar => {
                self.tab_strip.set_orientation(Orientation::Horizontal);
                self.tab_strip.set_valign(gtk4::Align::Center);
                self.tab_strip.set_hexpand(true);
                self.tab_strip.set_vexpand(false);
                self.tab_strip.add_css_class("top-tabs");
                self.top_tab_scroll.set_child(Some(&self.tab_strip));
                self.top_tab_search_holder.append(&self.tab_search_wrapper);
                self.top_spacer.set_hexpand(false);
                self.sidebar_tab_search_holder.set_visible(false);
                self.top_tab_search_holder.set_visible(true);
            }
        }

        // Resize each existing strip button for the new orientation.
        let mut child = self.tab_strip.first_child();
        while let Some(c) = child {
            if let Ok(btn) = c.clone().downcast::<ToggleButton>() {
                self.apply_strip_btn_placement(&btn);
            }
            child = c.next_sibling();
        }

        // The sidebar Tabs view only makes sense when tabs live in the sidebar.
        match placement {
            TabPlacement::Sidebar => {
                self.sidebar_tabs_btn.set_sensitive(true);
                self.apply_sidebar_view(self.sidebar_view.get(), false);
            }
            TabPlacement::TopBar => {
                self.sidebar_tabs_btn.set_sensitive(false);
                // Force the file tree without clobbering the saved preference.
                self.apply_sidebar_view(SidebarView::Files, false);
            }
        }

        self.sync_tab_bar_visibility();
    }

    /// Show one sidebar view (tab list vs file tree) and reflect it in the
    /// segmented buttons. When `persist`, remember the choice in config.
    pub(crate) fn apply_sidebar_view(&self, view: SidebarView, persist: bool) {
        match view {
            SidebarView::Tabs => self.sidebar_stack.set_visible_child_name("tabs"),
            SidebarView::Files => self.sidebar_stack.set_visible_child_name("files"),
        }
        // set_active does not refire `clicked`, so this won't recurse.
        self.sidebar_tabs_btn.set_active(view == SidebarView::Tabs);
        self.sidebar_files_btn
            .set_active(view == SidebarView::Files);

        if persist {
            self.sidebar_view.set(view);
            self.config.borrow_mut().sidebar_view = view;
            self.persist_config();
        }
    }

    /// Size a single strip button for the active placement: fill width in the
    /// sidebar, hug content in the top bar.
    pub(crate) fn apply_strip_btn_placement(&self, btn: &ToggleButton) {
        match self.tab_placement.get() {
            TabPlacement::Sidebar => btn.set_hexpand(true),
            TabPlacement::TopBar => btn.set_hexpand(false),
        }
    }

    /// Flip the tab strip between the sidebar and the top bar, then persist.
    pub(crate) fn toggle_tab_placement(&self) {
        let next = match self.tab_placement.get() {
            TabPlacement::Sidebar => TabPlacement::TopBar,
            TabPlacement::TopBar => TabPlacement::Sidebar,
        };
        self.tab_placement.set(next);
        self.config.borrow_mut().tab_placement = next;
        self.apply_tab_placement();
        self.persist_config();
    }
}
