//! Review-first correction suggestions for failed Block-mode commands.

mod logic;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::glib;
use gtk4::glib::prelude::ObjectExt;
use gtk4::prelude::*;

use crate::ai::AiClient;
use crate::block_view::TermView;
use logic::{Candidate, Context, SuggestionResult};

const ATTACHED_DATA_KEY: &str = "jterm4-command-fix-attached";

pub(crate) fn attach_to_block(view: &Rc<TermView>) {
    let root = view.widget();
    unsafe {
        if root.data::<bool>(ATTACHED_DATA_KEY).is_some() {
            return;
        }
        root.set_data::<bool>(ATTACHED_DATA_KEY, true);
    }

    let weak_view = Rc::downgrade(view);
    let weak_root = root.downgrade();
    let request_epoch = Rc::new(Cell::new(0_u64));

    view.connect_block_finished(move |command, exit_code, output| {
        if exit_code == 0 || command.trim().is_empty() || output.trim().is_empty() {
            return;
        }

        let Some(view) = weak_view.upgrade() else {
            return;
        };
        let Some(root) = weak_root.upgrade() else {
            return;
        };
        let remote = crate::ui::PaneLeaf::from_widget(&root)
            .is_some_and(|leaf| leaf.is_remote());
        let context = Context {
            command,
            cwd: view.cwd(),
            exit_code,
            output,
            remote,
        };
        let Some(failure) = logic::classify(&context) else {
            return;
        };

        let (enabled, ai_client) = current_ai_client();
        if !enabled {
            return;
        }

        let epoch = request_epoch.get().wrapping_add(1);
        request_epoch.set(epoch);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = logic::suggest_blocking(&context, &failure, ai_client.as_ref());
            let _ = sender.send(result);
        });

        let receiver = RefCell::new(receiver);
        let weak_view_for_ui = weak_view.clone();
        let weak_root_for_ui = weak_root.clone();
        let request_epoch_for_ui = request_epoch.clone();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            if request_epoch_for_ui.get() != epoch {
                return glib::ControlFlow::Break;
            }
            match receiver.borrow().try_recv() {
                Ok(SuggestionResult::Candidates(candidates)) if !candidates.is_empty() => {
                    if let (Some(view), Some(parent)) =
                        (weak_view_for_ui.upgrade(), weak_root_for_ui.upgrade())
                    {
                        show_dialog(&view, &parent, &candidates);
                    }
                    glib::ControlFlow::Break
                }
                Ok(_) => glib::ControlFlow::Break,
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    glib::ControlFlow::Break
                }
            }
        });
    });
}

fn current_ai_client() -> (bool, Option<AiClient>) {
    if std::env::var_os("JTERM4_SAFE_MODE").is_some() {
        return (false, None);
    }
    let (config, _, _) = crate::config::load_config();
    if !config.ai_enabled {
        return (false, None);
    }
    (true, AiClient::from_config(&config).ok())
}

fn show_dialog(view: &Rc<TermView>, parent: &gtk4::Widget, candidates: &[Candidate]) {
    let Some(best) = candidates.first() else {
        return;
    };

    let mut message = format!("{}\n\n{}", best.reason, best.evidence.label());
    if candidates.len() > 1 {
        message.push_str("\n\nOther candidates:");
        for candidate in candidates.iter().skip(1) {
            message.push_str(&format!("\n• {}", candidate.command));
        }
    }

    let entry = gtk4::Entry::new();
    entry.set_text(&best.command);
    entry.set_hexpand(true);
    entry.set_tooltip_text(Some(
        "Review or edit this command. Inserting it never presses Enter.",
    ));

    let hint = gtk4::Label::new(Some(
        "The command is inserted only when this pane is at an empty, idle prompt.",
    ));
    hint.set_xalign(0.0);
    hint.set_wrap(true);
    hint.add_css_class("dim-label");

    let extra = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    extra.append(&entry);
    extra.append(&hint);

    let dialog = adw::AlertDialog::new(Some("Possible command correction"), Some(&message));
    dialog.add_response("dismiss", "Dismiss");
    dialog.add_response("insert", "Insert command");
    dialog.set_default_response(Some("dismiss"));
    dialog.set_close_response("dismiss");
    dialog.set_response_appearance("insert", adw::ResponseAppearance::Suggested);
    dialog.set_extra_child(Some(&extra));

    let weak_view = Rc::downgrade(view);
    let weak_parent = parent.downgrade();
    dialog.connect_response(None, move |_, response| {
        if response != "insert" {
            return;
        }
        let Some(view) = weak_view.upgrade() else {
            return;
        };
        let Some(parent) = weak_parent.upgrade() else {
            return;
        };
        let command = entry.text();
        let command = command.trim();
        let Ok(command) = crate::review_input::validate(command) else {
            show_error(
                &parent,
                "The edited command contains a line break or terminal control character.",
            );
            return;
        };
        if !view.can_accept_agent_command() {
            show_error(
                &parent,
                "The target prompt is busy or already contains input. Clear it and try again.",
            );
            return;
        }
        view.write_input(command.as_bytes());
        view.grab_focus();
    });
    dialog.present(Some(parent));
}

fn show_error(parent: &gtk4::Widget, message: &str) {
    let dialog = adw::AlertDialog::new(Some("Command not inserted"), Some(message));
    dialog.add_response("ok", "OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");
    dialog.present(Some(parent));
}
