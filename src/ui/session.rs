//! session — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::Label;
use gtk4::ToggleButton;
use libadwaita as adw;

use super::*;
use crate::terminal::wrap_with_scrollbar;

impl UiState {
    /// Recursively restore a pane layout from saved state
    pub(crate) fn restore_pane_layout(
        &self,
        layout: crate::state::PaneLayout,
        tab_name: Option<String>,
    ) -> gtk4::Widget {
        use crate::state::PaneLayout;

        match layout {
            PaneLayout::Leaf {
                dir,
                sid,
                cmds,
                pinned,
            } => {
                // Create a simple tab with the leaf layout
                let _terminal = self.add_new_tab(Some(dir), tab_name, Some(sid), cmds);
                // Apply pinned state if set
                if pinned == Some(true) {
                    let page_num = self.notebook.n_pages().saturating_sub(1);
                    if let Some(page_widget) = self.notebook.nth_page(Some(page_num)) {
                        unsafe {
                            page_widget.set_data::<bool>("pinned", true);
                        }
                    }
                    // Find the corresponding strip button and mark as pinned
                    let tab_name_fmt = format!("tab-{}", page_num);
                    let mut child = self.tab_strip.first_child();
                    while let Some(ref c) = child {
                        if c.widget_name().as_str() == tab_name_fmt {
                            if let Ok(btn) = c.clone().downcast::<ToggleButton>() {
                                btn.add_css_class("tab-pinned");
                            }
                            break;
                        }
                        child = c.next_sibling();
                    }
                }
                // Return the page widget (last added page)
                let page_num = self.notebook.n_pages().saturating_sub(1);
                self.notebook
                    .nth_page(Some(page_num))
                    .expect("Just added a page")
            }
            PaneLayout::Split {
                orientation,
                position,
                start,
                end,
            } => {
                // First, create a temporary leaf to get a page
                let _terminal = self.add_new_tab(None, tab_name.clone(), None, None);
                let page_num = self.notebook.n_pages().saturating_sub(1);
                let _page_widget = self
                    .notebook
                    .nth_page(Some(page_num))
                    .expect("Just added a page");

                // Remove the temporary page
                self.notebook.remove_page(Some(page_num));

                // Recursively restore start and end layouts
                let start_widget = self.restore_pane_layout_internal(*start);
                let end_widget = self.restore_pane_layout_internal(*end);

                // Create the Paned container
                let paned = gtk4::Paned::new(match orientation {
                    'h' => gtk4::Orientation::Horizontal,
                    'v' => gtk4::Orientation::Vertical,
                    _ => gtk4::Orientation::Horizontal,
                });

                paned.set_start_child(Some(&start_widget));
                paned.set_end_child(Some(&end_widget));
                paned.set_position(position);

                // Add to notebook
                let label = tab_name.as_deref().unwrap_or("Split");
                self.notebook
                    .append_page(&paned, Some(&Label::new(Some(label))));
                let new_page_num = self.notebook.n_pages().saturating_sub(1);
                self.notebook.set_tab_reorderable(&paned, true);

                // Add tab strip button
                let strip_label = Label::new(Some(label));
                let btn = gtk4::ToggleButton::builder()
                    .child(&strip_label)
                    .css_classes(["flat", "tab-strip-btn"])
                    .build();
                unsafe {
                    btn.set_data::<Label>("tab-title-label", strip_label);
                }
                btn.set_focus_on_click(false);
                btn.set_can_focus(false);
                btn.set_widget_name(&format!("tab-{}", new_page_num));
                self.tab_strip.append(&btn);

                paned.upcast::<gtk4::Widget>()
            }
        }
    }

    /// Internal helper for recursive pane restoration
    fn restore_pane_layout_internal(&self, layout: crate::state::PaneLayout) -> gtk4::Widget {
        use crate::state::PaneLayout;

        match layout {
            PaneLayout::Leaf {
                dir,
                sid,
                cmds,
                pinned,
            } => {
                let terminal = self.add_new_tab(Some(dir), None, Some(sid), cmds);
                let page_num = self.notebook.n_pages().saturating_sub(1);
                if let Some(page_widget) = self.notebook.nth_page(Some(page_num)) {
                    if pinned == Some(true) {
                        unsafe {
                            page_widget.set_data::<bool>("pinned", true);
                        }
                    }
                }
                // Get the wrapped terminal widget
                wrap_with_scrollbar(&terminal).upcast::<gtk4::Widget>()
            }
            PaneLayout::Split {
                orientation,
                position,
                start,
                end,
            } => {
                let start_widget = self.restore_pane_layout_internal(*start);
                let end_widget = self.restore_pane_layout_internal(*end);

                let paned = gtk4::Paned::new(match orientation {
                    'h' => gtk4::Orientation::Horizontal,
                    'v' => gtk4::Orientation::Vertical,
                    _ => gtk4::Orientation::Horizontal,
                });

                paned.set_start_child(Some(&start_widget));
                paned.set_end_child(Some(&end_widget));
                paned.set_position(position);

                paned.upcast::<gtk4::Widget>()
            }
        }
    }
}
