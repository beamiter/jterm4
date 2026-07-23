//! Shared command-review UI used by every Block-mode assistant surface.
//!
//! The model/source-specific state machines stay in their owning modules. This
//! module owns the interaction contract users should not have to relearn:
//! editable one-line command, live risk feedback, copy, secondary actions, and
//! one clearly labelled primary action.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Box as GBox, Button, Entry, Label, Orientation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReviewPresentation {
    Standalone,
    Embedded,
}

pub(super) struct CommandReviewSpec {
    pub(super) presentation: ReviewPresentation,
    pub(super) compact: bool,
    pub(super) icon: &'static str,
    pub(super) title: String,
    pub(super) badge: String,
    pub(super) description: String,
    pub(super) command: String,
    pub(super) primary_label: String,
    pub(super) primary_executes: bool,
    pub(super) auxiliary_label: Option<String>,
    pub(super) secondary_label: Option<String>,
    pub(super) close_button: bool,
}

#[derive(Clone)]
pub(super) struct CommandReviewCard {
    pub(super) root: GBox,
    pub(super) entry: Entry,
    pub(super) primary: Button,
    pub(super) auxiliary: Option<Button>,
    pub(super) secondary: Option<Button>,
    pub(super) close: Option<Button>,
    pub(super) feedback: Label,
    risk: Label,
    primary_executes: Rc<Cell<bool>>,
}

#[derive(Clone)]
pub(super) struct CommandReviewPrimary {
    button: Button,
    risk: Label,
    executes: Rc<Cell<bool>>,
}

impl CommandReviewPrimary {
    pub(super) fn set(&self, label: &str, executes: bool, command: &str) {
        self.button.set_label(label);
        self.executes.set(executes);
        sync_risk(&self.risk, &self.button, command, executes);
    }

    pub(super) fn executes(&self) -> bool {
        self.executes.get()
    }
}

impl CommandReviewCard {
    pub(super) fn new(spec: CommandReviewSpec) -> Self {
        let root = GBox::new(Orientation::Vertical, 0);
        root.add_css_class("command-review");
        match spec.presentation {
            ReviewPresentation::Standalone => {
                root.add_css_class("block-finished");
                root.add_css_class("block-assistant");
                root.add_css_class("command-review-standalone");
                if spec.compact {
                    root.add_css_class("block-compact");
                    root.set_margin_top(1);
                    root.set_margin_bottom(1);
                    root.set_margin_start(4);
                    root.set_margin_end(4);
                } else {
                    root.set_margin_top(4);
                    root.set_margin_bottom(4);
                    root.set_margin_start(8);
                    root.set_margin_end(8);
                }
            }
            ReviewPresentation::Embedded => root.add_css_class("command-review-embedded"),
        }
        root.set_hexpand(true);
        root.set_vexpand(false);

        let header = GBox::new(Orientation::Horizontal, 8);
        header.add_css_class("command-review-header");
        if spec.presentation == ReviewPresentation::Standalone {
            header.add_css_class("block-header");
        }
        let (side_margin, top_margin, bottom_margin) =
            if spec.compact { (8, 3, 1) } else { (12, 6, 2) };
        header.set_margin_start(side_margin);
        header.set_margin_end(if spec.compact { 6 } else { 8 });
        header.set_margin_top(top_margin);
        header.set_margin_bottom(bottom_margin);

        let icon = Label::new(Some(spec.icon));
        icon.add_css_class("assistant-card-icon");
        header.append(&icon);

        let title = Label::new(Some(&spec.title));
        title.add_css_class("assistant-card-title");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        header.append(&title);

        let badge = Label::new(Some(&spec.badge));
        badge.add_css_class("assistant-card-badge");
        badge.set_hexpand(true);
        badge.set_halign(gtk4::Align::End);
        badge.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        badge.set_tooltip_text(Some(&spec.badge));
        header.append(&badge);

        let close = spec.close_button.then(|| {
            let button = Button::with_label("\u{2715}");
            button.add_css_class("flat");
            button.set_focusable(false);
            button.set_tooltip_text(Some("Dismiss (Esc)"));
            header.append(&button);
            button
        });
        root.append(&header);

        let body = GBox::new(Orientation::Vertical, 7);
        body.add_css_class("command-review-body");
        body.set_margin_start(side_margin);
        body.set_margin_end(side_margin);
        body.set_margin_top(2);
        body.set_margin_bottom(if spec.compact { 7 } else { 11 });

        let description = Label::new(Some(&spec.description));
        description.add_css_class("command-review-description");
        description.set_xalign(0.0);
        description.set_wrap(true);
        description.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        description.set_selectable(true);
        body.append(&description);

        let risk = Label::new(None);
        risk.add_css_class("command-review-risk");
        risk.set_xalign(0.0);
        risk.set_wrap(true);
        body.append(&risk);

        let entry = Entry::new();
        entry.add_css_class("command-review-entry");
        entry.set_text(&spec.command);
        entry.set_hexpand(true);
        entry.update_property(&[gtk4::accessible::Property::Label("Proposed shell command")]);
        body.append(&entry);

        let feedback = Label::new(None);
        feedback.add_css_class("command-review-feedback");
        feedback.set_xalign(0.0);
        feedback.set_wrap(true);
        feedback.set_visible(false);
        feedback.set_accessible_role(gtk4::AccessibleRole::Status);
        body.append(&feedback);

        let actions = GBox::new(Orientation::Horizontal, 6);
        actions.add_css_class("command-review-actions");
        actions.set_halign(gtk4::Align::End);
        let copy = Button::with_label("Copy");
        copy.set_tooltip_text(Some("Copy the command without inserting or running it"));
        actions.append(&copy);
        let auxiliary = spec.auxiliary_label.map(|label| {
            let button = Button::with_label(&label);
            actions.append(&button);
            button
        });
        let secondary = spec.secondary_label.map(|label| {
            let button = Button::with_label(&label);
            actions.append(&button);
            button
        });
        let primary = Button::with_label(&spec.primary_label);
        actions.append(&primary);
        body.append(&actions);
        root.append(&body);

        {
            let entry = entry.clone();
            let feedback = feedback.clone();
            copy.connect_clicked(move |button| {
                button.clipboard().set_text(&entry.text());
                feedback.set_text("Copied. Nothing was inserted or run.");
                feedback.remove_css_class("error");
                feedback.set_visible(true);
            });
        }

        let primary_executes = Rc::new(Cell::new(spec.primary_executes));
        sync_risk(&risk, &primary, &entry.text(), spec.primary_executes);
        {
            let risk = risk.clone();
            let primary = primary.clone();
            let executes = primary_executes.clone();
            let feedback = feedback.clone();
            entry.connect_changed(move |entry| {
                feedback.set_visible(false);
                sync_risk(&risk, &primary, &entry.text(), executes.get());
            });
        }

        Self {
            root,
            entry,
            primary,
            auxiliary,
            secondary,
            close,
            feedback,
            risk,
            primary_executes,
        }
    }

    pub(super) fn validated_command(&self) -> Result<String, String> {
        crate::review_input::validate(&self.entry.text())
            .map(str::to_string)
            .map_err(|error| error.to_string())
    }

    pub(super) fn show_error(&self, message: &str) {
        self.feedback.set_text(message);
        self.feedback.add_css_class("error");
        self.feedback.set_visible(true);
    }

    pub(super) fn show_info(&self, message: &str) {
        self.feedback.set_text(message);
        self.feedback.remove_css_class("error");
        self.feedback.set_visible(true);
    }

    pub(super) fn focus(&self) {
        self.entry.grab_focus();
    }

    /// Switch the primary action between non-executing review insertion and
    /// approval-gated execution. This is useful when editing invalidates a
    /// previously verified correction.
    pub(super) fn set_primary_action(&self, label: &str, executes: bool) {
        self.primary_controller()
            .set(label, executes, &self.entry.text());
    }

    pub(super) fn primary_executes(&self) -> bool {
        self.primary_executes.get()
    }

    pub(super) fn primary_controller(&self) -> CommandReviewPrimary {
        CommandReviewPrimary {
            button: self.primary.clone(),
            risk: self.risk.clone(),
            executes: self.primary_executes.clone(),
        }
    }
}

fn sync_risk(risk: &Label, primary: &Button, command: &str, executes: bool) {
    if let Some(reason) = crate::agent::is_dangerous(command) {
        risk.set_text(&format!("Potentially destructive: {reason}"));
        risk.add_css_class("error");
        primary.remove_css_class("suggested-action");
        if executes {
            primary.add_css_class("destructive-action");
            primary.set_tooltip_text(Some(
                "Running this command requires a second exact-command confirmation",
            ));
        } else {
            primary.remove_css_class("destructive-action");
            primary.add_css_class("suggested-action");
            primary.set_tooltip_text(Some("Insert this command at the prompt without running it"));
        }
    } else {
        risk.set_text(if executes {
            "Review the exact command. It runs only after explicit approval."
        } else {
            "Review first. The primary action inserts this command but does not run it."
        });
        risk.remove_css_class("error");
        primary.remove_css_class("destructive-action");
        primary.add_css_class("suggested-action");
        primary.set_tooltip_text(Some(if executes {
            "Run this exact command after approval"
        } else {
            "Insert this command at the prompt without running it"
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::sync_risk;
    use gtk4::{Button, Label};

    // GTK widget construction requires a display in normal unit-test runs, so
    // behavior exercised without GTK lives in agent/review_input tests. Keep
    // this symbol referenced to prevent accidental dead-code drift.
    #[test]
    fn shared_risk_renderer_is_linked() {
        let _renderer: fn(&Label, &Button, &str, bool) = sync_risk;
    }
}
