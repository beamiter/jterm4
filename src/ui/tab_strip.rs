//! tab_strip — UiState methods extracted from ui (mechanical split, no logic changes)
use gtk4::glib;
use gtk4::ToggleButton;
use libadwaita as adw;
use adw::prelude::*;

use super::*;

impl UiState {

    /// Update which tab strip button is :checked to match the active notebook page.
    pub(crate) fn sync_tab_strip_active(&self, active_page: Option<u32>) {
        let active = active_page.or(self.notebook.current_page()).unwrap_or(0);
        let mut idx = 0u32;
        let mut child = self.tab_strip.first_child();
        while let Some(c) = child {
            if let Ok(btn) = c.clone().downcast::<ToggleButton>() {
                btn.set_active(idx == active);
            }
            idx += 1;
            child = c.next_sibling();
        }
    }

    /// Show the top-bar tab strip only when tabs live there and more than one
    /// tab exists. The sidebar itself stays visible (it always offers the file
    /// tree); use Ctrl+\ to hide it.
    pub(crate) fn sync_tab_bar_visibility(&self) {
        use crate::config::TabPlacement;
        let show_strip = self.notebook.n_pages() > 1;
        match self.tab_placement.get() {
            TabPlacement::Sidebar => {
                self.tab_strip_scroll.set_visible(true);
                self.top_tab_scroll.set_visible(false);
            }
            TabPlacement::TopBar => {
                self.top_tab_scroll.set_visible(show_strip);
                self.tab_strip_scroll.set_visible(true);
            }
        }
    }

    /// Remove the tab strip button that corresponds to a notebook page widget.
    pub(crate) fn remove_strip_button_for(&self, widget: &gtk4::Widget) {
        let name = widget.widget_name();
        let mut child = self.tab_strip.first_child();
        while let Some(c) = child {
            if c.widget_name() == name {
                self.tab_strip.remove(&c);
                return;
            }
            child = c.next_sibling();
        }
    }

    pub(crate) fn switch_tab(&self, direction: i32) {
        if let Some(page) = self.notebook.current_page() {
            let n = self.notebook.n_pages();
            if n == 0 {
                return;
            }
            let next = if direction > 0 {
                if page < n - 1 { page + 1 } else { 0 }
            } else {
                if page > 0 { page - 1 } else { n.saturating_sub(1) }
            };
            self.notebook.set_current_page(Some(next));
        }
    }

    pub(crate) fn clear_tab_selection(&self) {
        for tab_name in self.selected_tabs.borrow().iter() {
            if let Some(mut child) = self.tab_strip.first_child() {
                loop {
                    if child.widget_name().as_str() == tab_name {
                        if let Ok(btn) = child.clone().downcast::<ToggleButton>() {
                            btn.remove_css_class("tab-selected");
                        }
                        break;
                    }
                    match child.next_sibling() {
                        Some(next) => child = next,
                        None => break,
                    }
                }
            }
        }
        self.selected_tabs.borrow_mut().clear();
    }

    pub(crate) fn toggle_tab_selection(&self, tab_name: &str) {
        let mut selected = self.selected_tabs.borrow_mut();
        if let Some(pos) = selected.iter().position(|x| x == tab_name) {
            selected.remove(pos);
            // Remove CSS class
            if let Some(mut child) = self.tab_strip.first_child() {
                loop {
                    if child.widget_name().as_str() == tab_name {
                        if let Ok(btn) = child.clone().downcast::<ToggleButton>() {
                            btn.remove_css_class("tab-selected");
                        }
                        break;
                    }
                    match child.next_sibling() {
                        Some(next) => child = next,
                        None => break,
                    }
                }
            }
        } else {
            selected.push(tab_name.to_string());
            // Add CSS class
            if let Some(mut child) = self.tab_strip.first_child() {
                loop {
                    if child.widget_name().as_str() == tab_name {
                        if let Ok(btn) = child.clone().downcast::<ToggleButton>() {
                            btn.add_css_class("tab-selected");
                        }
                        break;
                    }
                    match child.next_sibling() {
                        Some(next) => child = next,
                        None => break,
                    }
                }
            }
        }
    }

    pub(crate) fn select_tab_range(&self, from_name: &str, to_name: &str) {
        self.clear_tab_selection();
        let mut selected = self.selected_tabs.borrow_mut();
        let mut in_range = false;

        if let Some(mut child) = self.tab_strip.first_child() {
            loop {
                let child_name = child.widget_name();
                if child_name.as_str() == from_name {
                    in_range = true;
                }
                if in_range {
                    selected.push(child_name.to_string());
                    if let Ok(btn) = child.clone().downcast::<ToggleButton>() {
                        btn.add_css_class("tab-selected");
                    }
                }
                if child_name.as_str() == to_name {
                    in_range = false;
                }
                match child.next_sibling() {
                    Some(next) => child = next,
                    None => break,
                }
            }
        }
    }

    pub(crate) fn close_selected_tabs(&self) {
        let selected = self.selected_tabs.borrow();
        if selected.is_empty() {
            return;
        }

        for tab_name in selected.iter() {
            // Find the widget by name and close it
            for i in 0..self.notebook.n_pages() {
                if let Some(page_widget) = self.notebook.nth_page(Some(i)) {
                    if page_widget.widget_name().as_str() == tab_name {
                        self.remove_tab_by_widget(&page_widget);
                        break;
                    }
                }
            }
        }
        drop(selected);
        self.selected_tabs.borrow_mut().clear();
    }

    pub(crate) fn move_tab_left(&self) {
        if let Some(current_page) = self.notebook.current_page() {
            if current_page > 0 {
                let new_page = current_page - 1;
                self.notebook.reorder_child(&self.notebook.nth_page(Some(current_page)).unwrap(), Some(new_page));
                self.reorder_tab_strip_buttons();
                self.notebook.set_current_page(Some(new_page));
                self.sync_tab_strip_active(Some(new_page));
            }
        }
    }

    pub(crate) fn move_tab_right(&self) {
        if let Some(current_page) = self.notebook.current_page() {
            let n_pages = self.notebook.n_pages();
            if current_page < n_pages - 1 {
                let new_page = current_page + 1;
                self.notebook.reorder_child(&self.notebook.nth_page(Some(current_page)).unwrap(), Some(new_page));
                self.reorder_tab_strip_buttons();
                self.notebook.set_current_page(Some(new_page));
                self.sync_tab_strip_active(Some(new_page));
            }
        }
    }

    fn reorder_tab_strip_buttons(&self) {
        let mut button_order = Vec::new();
        let mut idx = 0u32;
        while let Some(page) = self.notebook.nth_page(Some(idx)) {
            let name = page.widget_name();
            button_order.push(name);
            idx += 1;
        }

        let mut child = self.tab_strip.first_child();
        let mut button_idx = 0;
        while let Some(c) = child.clone() {
            if button_idx < button_order.len() && c.widget_name() == button_order[button_idx] {
                if button_idx > 0 {
                    let mut prev_child = self.tab_strip.first_child();
                    let mut prev_idx = 0;
                    while let Some(pc) = prev_child {
                        if prev_idx == button_idx - 1 {
                            self.tab_strip.reorder_child_after(&c, Some(&pc));
                            break;
                        }
                        prev_idx += 1;
                        prev_child = pc.next_sibling();
                    }
                } else {
                    self.tab_strip.reorder_child_after(&c, None::<&gtk4::Widget>);
                }
                button_idx += 1;
            }
            child = c.next_sibling();
        }
    }

    pub(crate) fn toggle_current_tab_marked(&self) {
        if let Some(page) = self.notebook.current_page() {
            let mut idx = 0u32;
            let mut child = self.tab_strip.first_child();
            while let Some(c) = child {
                if idx == page {
                    if let Ok(btn) = c.clone().downcast::<ToggleButton>() {
                        if btn.has_css_class("tab-marked") {
                            btn.remove_css_class("tab-marked");
                            unsafe { btn.set_data::<bool>("marked", false); }
                        } else {
                            btn.add_css_class("tab-marked");
                            unsafe { btn.set_data::<bool>("marked", true); }
                        }
                    }
                    break;
                }
                idx += 1;
                child = c.next_sibling();
            }
        }
    }

    /// Find the strip button widget for a given tab widget name.
    pub(crate) fn find_strip_button(&self, widget_name: &str) -> Option<ToggleButton> {
        let mut child = self.tab_strip.first_child();
        while let Some(c) = child {
            if c.widget_name().as_str() == widget_name {
                return c.downcast::<ToggleButton>().ok();
            }
            child = c.next_sibling();
        }
        None
    }

    /// Mark a tab as having activity (new output on a non-active tab).
    pub(crate) fn mark_tab_activity(&self, tab_widget_name: &str) {
        if let Some(btn) = self.find_strip_button(tab_widget_name) {
            if !btn.is_active() {
                btn.add_css_class("tab-activity");
            }
        }
    }

    /// Mark a tab as having received a bell signal.
    pub(crate) fn mark_tab_bell(&self, tab_widget_name: &str) {
        if let Some(btn) = self.find_strip_button(tab_widget_name) {
            if !btn.is_active() {
                btn.add_css_class("tab-bell");
                btn.add_css_class("tab-bell-flash");
                // Remove flash animation class after it completes
                let btn_clone = btn.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                    btn_clone.remove_css_class("tab-bell-flash");
                });
            }
        }
    }

    /// Clear activity/bell indicators when a tab becomes active.
    pub(crate) fn clear_tab_indicators(&self, tab_widget_name: &str) {
        if let Some(btn) = self.find_strip_button(tab_widget_name) {
            btn.remove_css_class("tab-activity");
            btn.remove_css_class("tab-bell");
            btn.remove_css_class("tab-bell-flash");
        }
    }

    /// Locate the connection-status dot inside a tab's strip button, if any.
    fn find_conn_dot(&self, tab_num: u32) -> Option<gtk4::Widget> {
        let btn = self.find_strip_button(&format!("tab-{}", tab_num))?;
        let strip_box = btn.child()?;
        let mut child = strip_box.first_child();
        while let Some(c) = child {
            if c.has_css_class("tab-conn-dot") {
                return Some(c);
            }
            child = c.next_sibling();
        }
        None
    }

    /// Update the per-tab connection-status dot (yellow/green/red).
    pub(crate) fn set_tab_conn_status(&self, tab_num: u32, status: super::ConnStatus) {
        if let Some(dot) = self.find_conn_dot(tab_num) {
            dot.remove_css_class("tab-connecting");
            dot.remove_css_class("tab-connected");
            dot.remove_css_class("tab-disconnected");
            match status {
                super::ConnStatus::Connecting => dot.add_css_class("tab-connecting"),
                super::ConnStatus::Connected => dot.add_css_class("tab-connected"),
                super::ConnStatus::Disconnected => dot.add_css_class("tab-disconnected"),
            }
            dot.set_visible(true);
        }
    }

    /// Relabel a tab's strip button (used for the reconnect countdown).
    pub(crate) fn set_tab_strip_label(&self, tab_num: u32, text: &str) {
        if let Some(btn) = self.find_strip_button(&format!("tab-{}", tab_num)) {
            if let Some(strip_box) = btn.child() {
                let mut child = strip_box.first_child();
                while let Some(c) = child {
                    if let Ok(label) = c.clone().downcast::<gtk4::Label>() {
                        if !label.has_css_class("tab-conn-dot")
                            && !label.has_css_class("tab-process-indicator")
                        {
                            label.set_text(text);
                            return;
                        }
                    }
                    child = c.next_sibling();
                }
            }
        }
    }
}
