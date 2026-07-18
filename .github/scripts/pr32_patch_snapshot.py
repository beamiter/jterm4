#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}\n--- needle ---\n{old}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


block = Path("src/block_view/mod.rs")
config_apply = Path("src/ui/config_apply.rs")

replace_once(
    block,
    '''fn normalize_captured_command(captured: &str, prompt: &str) -> String {
    let captured = captured.trim();
    let prompt = prompt.trim();
    if !prompt.is_empty() {
        if let Some(command) = captured.strip_prefix(prompt) {
            return command.trim_start().to_string();
        }
    }
    captured.to_string()
}
''',
    '''fn normalize_captured_command(captured: &str, prompt: &str) -> String {
    let captured = captured.trim();
    let prompt = prompt.trim();
    if !prompt.is_empty() {
        if let Some(command) = captured.strip_prefix(prompt) {
            return command.trim_start().to_string();
        }
    }
    captured.to_string()
}

/// Resolve the command at CommandStart without trusting VTE feed timing. The PTY
/// reader can deliver the echoed command and OSC 133;C in one chunk: `feed()`
/// queues the echo for VTE, then the semantic event is handled immediately, so
/// the text range can still be empty. The keystroke shadow is deliberately only
/// a fallback; a settled VTE capture remains authoritative for history recall,
/// autosuggestions, IME, and shell line-editor redraws.
fn resolve_submitted_command(captured: &str, prompt: &str, typed_shadow: &str) -> String {
    let captured = normalize_captured_command(captured, prompt);
    if captured.trim().is_empty() {
        typed_shadow.trim().to_string()
    } else {
        captured
    }
}
''',
)

replace_once(
    block,
    '''                if bstate_for_commit.get() == BlockState::AwaitingCommand {
                    idle_input_dirty_for_commit.set(true);
                    if text.as_bytes().iter().any(|&b| b != b'\r' && b != b'\n') {
                        // A later recall must replace this edited readline buffer,
                        // not append to it. PromptEnd resets the flag for a new line.
                        pty_synced_for_commit.set(true);
                    }
                }

                pty_for_commit.write_bytes(text.as_bytes());
                // The finished-block command text comes from a live-VTE
                // text_range read at CommandStart (see PromptEnd / CommandStart
                // handlers), so this shadow buffer is only a fallback. It need
                // not reproduce every line-editor escape sequence.
                if bstate_for_commit.get() == BlockState::AwaitingCommand {
                    let mut cmd = typed_cmd_for_commit.borrow_mut();
                    for ch in text.chars() {
                        if ch == '\r' || ch == '\n' {
                            // Submitted — leave whatever is in the buffer; it
                            // is cleared at PromptEnd for the next prompt.
                        } else if ch == '\x7f' || ch == '\x08' {
                            cmd.pop();
                        } else if (ch as u32) < 0x20 {
                            // Control bytes: ignore.
                        } else {
                            cmd.push(ch);
                        }
                    }
                }
''',
    '''                let awaiting_command = bstate_for_commit.get() == BlockState::AwaitingCommand;
                if awaiting_command {
                    idle_input_dirty_for_commit.set(true);
                    if text.as_bytes().iter().any(|&b| b != b'\r' && b != b'\n') {
                        // A later recall must replace this edited readline buffer,
                        // not append to it. PromptEnd resets the flag for a new line.
                        pty_synced_for_commit.set(true);
                    }

                    // Update the fallback before exposing bytes to the PTY. A very
                    // fast shell can echo the line and emit OSC 133;C immediately;
                    // the reader must never observe CommandStart while this shadow
                    // still describes the previous editor state.
                    let mut cmd = typed_cmd_for_commit.borrow_mut();
                    for ch in text.chars() {
                        if ch == '\r' || ch == '\n' {
                            // Submitted — leave whatever is in the buffer; it
                            // is cleared at PromptEnd for the next prompt.
                        } else if ch == '\x7f' || ch == '\x08' {
                            cmd.pop();
                        } else if (ch as u32) < 0x20 {
                            // Control bytes: ignore.
                        } else {
                            cmd.push(ch);
                        }
                    }
                }

                pty_for_commit.write_bytes(text.as_bytes());
''',
)

replace_once(
    block,
    '''                                let cmd = if is_background {
                                    String::new()
                                } else {
                                    let vte_cmd = vte_typed_cmd_rc.borrow().trim().to_string();
                                    if !vte_cmd.is_empty() {
                                        vte_cmd
                                    } else {
                                        typed_cmd_rc.borrow().trim().to_string()
                                    }
                                };

                                if cmd.is_empty() && !is_background {
                                    // Nothing meaningful to record; just reset.
                                    let preserve = config_for_cb.borrow().preserve_live_scrollback;
                                    active_rc.borrow().reset_active(preserve);
                                    bstate_rc.set(BlockState::CollectingPrompt);
                                    prompt_buf_rc.borrow_mut().clear();
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    continue;
                                }
''',
    '''                                let mut cmd = if is_background {
                                    String::new()
                                } else {
                                    let vte_cmd = vte_typed_cmd_rc.borrow().trim().to_string();
                                    if !vte_cmd.is_empty() {
                                        vte_cmd
                                    } else {
                                        typed_cmd_rc.borrow().trim().to_string()
                                    }
                                };

                                if cmd.is_empty() && !is_background {
                                    // Never silently discard a command lifecycle. The
                                    // VTE range can be empty during an echo/feed race,
                                    // and line-editor control sequences do not always
                                    // populate the printable keystroke shadow. Keep a
                                    // visible diagnostic card whenever input activity
                                    // or actual output proves that something ran.
                                    let output_visible = background_output_has_visible_text(
                                        active_rc.borrow().output_text().as_bytes(),
                                    );
                                    if pty_synced_rc.get() || output_visible {
                                        log::warn!(
                                            "finished command text was unavailable; preserving block with placeholder"
                                        );
                                        cmd = "(command capture unavailable)".to_string();
                                    } else {
                                        // A genuinely empty submission with no output
                                        // is not useful history; reset for the prompt.
                                        let preserve =
                                            config_for_cb.borrow().preserve_live_scrollback;
                                        active_rc.borrow().reset_active(preserve);
                                        bstate_rc.set(BlockState::CollectingPrompt);
                                        prompt_buf_rc.borrow_mut().clear();
                                        scroll_debouncer.mark_dirty(&block_scroll_rc);
                                        continue;
                                    }
                                }
''',
)

replace_once(
    block,
    '''                            // Feed next initial command if any.
                            if let Some(cmd) = init_cmds_queue_for_cb.borrow_mut().pop_front() {
                                let text = format!("{}\r", cmd);
                                pty_for_init.write_bytes(text.as_bytes());
                            }
''',
    '''                            // Feed next initial command if any. Seed the same
                            // fallback state as interactive input before writing, so
                            // a fast command cannot outrun command capture.
                            if let Some(cmd) = init_cmds_queue_for_cb.borrow_mut().pop_front() {
                                *typed_cmd_rc.borrow_mut() = cmd.clone();
                                idle_input_dirty_rc.set(true);
                                pty_synced_rc.set(true);
                                let text = format!("{}\r", cmd);
                                pty_for_init.write_bytes(text.as_bytes());
                            }
''',
)

replace_once(
    block,
    '''                            let cmd_from_vte =
                                normalize_captured_command(&captured, &prompt_display_rc.borrow());
                            *vte_typed_cmd_rc.borrow_mut() = cmd_from_vte.clone();
                            *running_cmd_rc.borrow_mut() = cmd_from_vte;
''',
    '''                            let prompt_display = prompt_display_rc.borrow().clone();
                            let typed_shadow = typed_cmd_rc.borrow().clone();
                            let submitted_command = resolve_submitted_command(
                                &captured,
                                &prompt_display,
                                &typed_shadow,
                            );
                            *vte_typed_cmd_rc.borrow_mut() = submitted_command.clone();
                            *running_cmd_rc.borrow_mut() = submitted_command;
''',
)

replace_once(
    block,
    '''        coalesce_bytes_events, compute_viewport_state, normalize_captured_command,
        scroll_delta_to_reveal, selected_command_text, selected_id_range, strip_ansi,
''',
    '''        coalesce_bytes_events, compute_viewport_state, normalize_captured_command,
        resolve_submitted_command, scroll_delta_to_reveal, selected_command_text,
        selected_id_range, strip_ansi,
''',
)

replace_once(
    block,
    '''    #[test]
    fn captured_command_preserves_legitimate_text() {
        assert_eq!(
            normalize_captured_command("printf pwd", "yj ~ ❯"),
            "printf pwd"
        );
    }

''',
    '''    #[test]
    fn captured_command_preserves_legitimate_text() {
        assert_eq!(
            normalize_captured_command("printf pwd", "yj ~ ❯"),
            "printf pwd"
        );
    }

    #[test]
    fn submitted_command_falls_back_when_vte_echo_has_not_settled() {
        assert_eq!(
            resolve_submitted_command("", "yj ~/project ❯", "git status"),
            "git status"
        );
    }

    #[test]
    fn submitted_command_prefers_the_rendered_line_editor_state() {
        assert_eq!(
            resolve_submitted_command("git diff --stat", "yj ~/project ❯", "git status"),
            "git diff --stat"
        );
        assert_eq!(
            resolve_submitted_command("yj ~/project ❯ cargo test", "yj ~/project ❯", "cargo"),
            "cargo test"
        );
    }

''',
)

replace_once(
    config_apply,
    '''             .sidebar-box {{ background-color: rgb({br},{bg_g},{bb}); }}
             .tab-strip-btn {{ color: rgba({fr},{fg_g},{fb},0.6); }}
''',
    '''             .sidebar-box {{ background-color: rgb({br},{bg_g},{bb}); }}
             .sidebar-switcher, .sidebar-switcher button, .sidebar-switcher label,
             .file-tree-header, .file-tree-header button, .file-tree-header label,
             .file-tree-root {{ color: rgb({fr},{fg_g},{fb}); }}
             .file-tree-root {{ opacity: 1.0; }}
             .tab-strip-btn {{ color: rgba({fr},{fg_g},{fb},0.6); }}
''',
)

print("Applied block command-capture and sidebar-contrast fixes")
