//! Unified fuzzy command palette over actions, persistent history, workflows
//! and the review-first AI command entry.

use adw::prelude::*;
use gtk4::gdk::{Key, ModifierType};
use gtk4::{EventControllerKey, Label, ListBox, Orientation, ScrolledWindow, SearchEntry};
use libadwaita as adw;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use super::UiState;
use crate::palette::{Accept, Entry, PaletteMode, Query};

fn clear_rows(list: &ListBox) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
}

fn render_rows(list: &ListBox, entries: &[Entry]) {
    clear_rows(list);
    for entry in entries {
        let row = adw::ActionRow::builder()
            .title(&entry.label)
            .activatable(true)
            .build();
        if let Some(subtitle) = entry.sublabel.as_deref() {
            row.set_subtitle(subtitle);
        }
        if let Some(right) = entry.right.as_deref() {
            let label = Label::new(Some(right));
            label.add_css_class("dim-label");
            row.add_suffix(&label);
        }
        list.append(&row);
    }
    if let Some(first) = list.row_at_index(0) {
        list.select_row(Some(&first));
    }
}

impl UiState {
    /// The single UI boundary for review-only shell insertion. Every caller
    /// gets the same control-character rejection and visible failure instead
    /// of silently turning a multi-line value into submitted shell input.
    pub(crate) fn insert_review_text(&self, pane: &crate::ui::PaneLeaf, text: &str) -> bool {
        match pane.write_review_input(text) {
            Ok(()) => {
                pane.grab_focus();
                true
            }
            Err(error) => {
                log::warn!("refusing unsafe review-only shell input: {error}");
                let dialog = adw::AlertDialog::new(
                    Some("Command was not inserted"),
                    Some(&format!(
                        "This review-only action cannot insert the command because {error}."
                    )),
                );
                dialog.add_response("ok", "OK");
                dialog.set_default_response(Some("ok"));
                dialog.present(Some(&self.window));
                false
            }
        }
    }

    fn gather_palette_entries(&self, raw_query: &str) -> Vec<Entry> {
        let workflows = crate::workflows::load_all();
        let history_path = {
            let config = self.config.borrow();
            config
                .command_history_enabled
                .then(|| config.command_history_path.clone())
                .flatten()
        };
        crate::palette::gather(
            &Query::parse(raw_query, PaletteMode::All),
            &self.keybinding_map.borrow(),
            history_path.as_deref().map(Path::new),
            &workflows,
            100,
        )
    }

    fn accept_palette_entry(&self, entry: Entry) {
        match entry.accept {
            Accept::Action(action) => self.execute_action(action),
            Accept::TypeCommand(command) => {
                if command.is_empty() {
                    return;
                }
                if let Some(pane) = self.current_pane_leaf() {
                    self.insert_review_text(&pane, &command);
                }
            }
            Accept::AskAi(request) => {
                if request.is_empty() {
                    return;
                }
                self.generate_command_for_review(request);
            }
            Accept::RunWorkflow(path) => {
                let Some(workflow) = crate::workflows::load_all()
                    .into_iter()
                    .find(|workflow| workflow.source_path == path)
                else {
                    log::warn!("workflow disappeared before activation: {}", path.display());
                    return;
                };
                let Some(pane) = self.current_pane_leaf() else {
                    return;
                };
                if workflow.args.is_empty() {
                    self.insert_review_text(&pane, &workflow.command);
                } else {
                    self.show_workflow_args_dialog(workflow, pane);
                }
            }
        }
    }

    fn generate_command_for_review(&self, request: String) {
        let client = match crate::ai::AiClient::from_config(&self.config.borrow()) {
            Ok(client) => client,
            Err(error) => {
                self.show_ai_error(&error.to_string());
                return;
            }
        };
        let Some(pane) = self.current_pane_leaf() else {
            self.show_ai_error("There is no active terminal pane to receive the command.");
            return;
        };
        let cwd = pane
            .block_view()
            .map(|view| view.cwd())
            .filter(|cwd| !cwd.is_empty())
            .or_else(|| crate::terminal::terminal_working_directory(pane.terminal()))
            .unwrap_or_else(|| ".".to_string());

        if !self.ai_panel_visible.get() {
            self.toggle_ai_panel();
        }
        let generation_chat_id = self.ai_panel.command_generation_started(&request);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = crate::ai::nl_to_command_blocking(&client, &request, &cwd);
            let _ = tx.send(result);
        });

        let ui = self.clone();
        let rx = RefCell::new(rx);
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            match rx.borrow().try_recv() {
                Ok(Ok(command)) => {
                    if pane
                        .block_view()
                        .is_some_and(|view| !view.can_accept_agent_command())
                    {
                        let message = "The target Block prompt is busy or now contains input; the generated command was not inserted.";
                        ui.ai_panel
                            .command_generation_failed(generation_chat_id, message);
                        ui.show_ai_error(message);
                        return gtk4::glib::ControlFlow::Break;
                    }

                    let danger = crate::agent::is_dangerous(&command);
                    ui.ai_panel
                        .command_generation_review_required(generation_chat_id, &command);
                    let title = if danger.is_some() {
                        "Review potentially destructive command"
                    } else {
                        "Review generated command"
                    };
                    let message = if let Some(reason) = danger {
                        format!(
                            "Warning: {reason}.\n\n{command}\n\nInsert it at the prompt without running it?"
                        )
                    } else {
                        format!("{command}\n\nInsert it at the prompt without running it?")
                    };
                    let dialog = adw::AlertDialog::new(Some(title), Some(&message));
                    dialog.add_responses(&[("cancel", "Cancel"), ("insert", "Insert")]);
                    dialog.set_default_response(Some("cancel"));
                    dialog.set_close_response("cancel");
                    if danger.is_some() {
                        dialog.set_response_appearance(
                            "insert",
                            adw::ResponseAppearance::Destructive,
                        );
                    } else {
                        dialog
                            .set_response_appearance("insert", adw::ResponseAppearance::Suggested);
                    }
                    let pane = pane.clone();
                    let command_for_insert = command.clone();
                    let ui_for_response = ui.clone();
                    dialog.connect_response(None, move |_, response| {
                        if response != "insert" {
                            ui_for_response
                                .ai_panel
                                .command_generation_failed(
                                    generation_chat_id,
                                    "Command insertion cancelled.",
                                );
                            return;
                        }
                        if pane
                            .block_view()
                            .is_some_and(|view| !view.can_accept_agent_command())
                        {
                            let message = "The target Block prompt is busy or now contains input; the generated command was not inserted.";
                            ui_for_response
                                .ai_panel
                                .command_generation_failed(generation_chat_id, message);
                            ui_for_response.show_ai_error(message);
                            return;
                        }
                        if ui_for_response.insert_review_text(&pane, &command_for_insert) {
                            ui_for_response
                                .ai_panel
                                .command_generation_inserted(generation_chat_id);
                        } else {
                            ui_for_response.ai_panel.command_generation_failed(
                                generation_chat_id,
                                "The generated command contained unsafe terminal control characters.",
                            );
                        }
                    });
                    dialog.present(Some(&ui.window));
                    gtk4::glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    let message = format!("Command generation failed: {error}");
                    ui.ai_panel
                        .command_generation_failed(generation_chat_id, &message);
                    ui.show_ai_error(&message);
                    gtk4::glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => gtk4::glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let message = "Command generation worker disconnected.";
                    ui.ai_panel
                        .command_generation_failed(generation_chat_id, message);
                    ui.show_ai_error(message);
                    gtk4::glib::ControlFlow::Break
                }
            }
        });
    }

    pub(crate) fn toggle_unified_command_palette(&self) {
        // `force_close` emits `closed` synchronously; release this borrow first
        // so its callback can clear the slot without a RefCell re-entry panic.
        let dialog_to_close = self.command_palette_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
            return;
        }

        let dialog = adw::Dialog::builder()
            .title("Command Palette")
            .content_width(620)
            .content_height(520)
            .build();
        let header = adw::HeaderBar::new();
        let filter = SearchEntry::new();
        filter.set_placeholder_text(Some(
            "Search all · > actions · @ history · : workflows · ? ask AI",
        ));
        filter.set_hexpand(true);

        let list = ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::Single);
        list.add_css_class("boxed-list");
        list.set_margin_start(12);
        list.set_margin_end(12);
        list.set_margin_bottom(12);
        let entries = Rc::new(RefCell::new(self.gather_palette_entries("")));
        render_rows(&list, &entries.borrow());

        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list)
            .build();
        let body = gtk4::Box::new(Orientation::Vertical, 0);
        filter.set_margin_start(12);
        filter.set_margin_end(12);
        filter.set_margin_top(8);
        filter.set_margin_bottom(8);
        body.append(&filter);
        body.append(&scrolled);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&body));
        dialog.set_child(Some(&toolbar));

        {
            let ui = self.clone();
            let list = list.clone();
            let entries = entries.clone();
            filter.connect_search_changed(move |entry| {
                let next = ui.gather_palette_entries(&entry.text());
                render_rows(&list, &next);
                *entries.borrow_mut() = next;
            });
        }

        let activate = {
            let ui = self.clone();
            let entries = entries.clone();
            let dialog = dialog.clone();
            Rc::new(move |index: usize| {
                let Some(entry) = entries.borrow().get(index).cloned() else {
                    return;
                };
                dialog.force_close();
                ui.accept_palette_entry(entry);
            })
        };
        {
            let activate = activate.clone();
            list.connect_row_activated(move |_, row| activate(row.index() as usize));
        }

        let keys = EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let list = list.clone();
            let dialog = dialog.clone();
            let dialog_slot = self.command_palette_dialog.clone();
            let activate = activate.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
                if key == Key::Escape
                    || (matches!(key, Key::P | Key::p)
                        && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK))
                {
                    dialog_slot.borrow_mut().take();
                    dialog.force_close();
                    return true.into();
                }
                if matches!(key, Key::Return | Key::KP_Enter) {
                    if let Some(row) = list.selected_row() {
                        activate(row.index() as usize);
                    }
                    return true.into();
                }
                if matches!(key, Key::Down | Key::Up) {
                    let current = list.selected_row().map(|row| row.index()).unwrap_or(0);
                    let next = if key == Key::Down {
                        current.saturating_add(1)
                    } else {
                        current.saturating_sub(1)
                    };
                    if let Some(row) = list.row_at_index(next) {
                        list.select_row(Some(&row));
                    }
                    return true.into();
                }
                false.into()
            });
        }
        dialog.add_controller(keys);

        let slot = self.command_palette_dialog.clone();
        dialog.connect_closed(move |_| *slot.borrow_mut() = None);
        *self.command_palette_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
        filter.grab_focus();
    }
}
