#!/usr/bin/env python3
from pathlib import Path

def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}: {old[:100]!r}")
    file.write_text(text.replace(old, new, 1))

replace_once(
    "src/block_view/mod.rs",
    '''fn build_clipboard_paste_payload(text: &str, bracketed_paste: bool) -> Vec<u8> {
    if !bracketed_paste {
        return text.as_bytes().to_vec();
    }

    let mut payload = Vec::with_capacity(text.len() + 12);
    payload.extend_from_slice(b"\\x1b[200~");
    payload.extend_from_slice(text.as_bytes());
    payload.extend_from_slice(b"\\x1b[201~");
    payload
}
''',
    '''fn build_clipboard_paste(text: &str, bracketed_paste: bool) -> (String, Vec<u8>) {
    let normalized = text.replace("\\r\\n", "\\n").replace('\\r', "\\n");
    let inserted = if bracketed_paste {
        normalized
    } else {
        // Without bracketed paste, the PTY safety boundary keeps only the first
        // logical line. Mirror that exact text in the fallback editor model.
        normalized.split('\\n').next().unwrap_or("").to_string()
    };
    if inserted.is_empty() {
        return (inserted, Vec::new());
    }

    let payload = if bracketed_paste {
        let mut payload = Vec::with_capacity(inserted.len() + 12);
        payload.extend_from_slice(b"\\x1b[200~");
        payload.extend_from_slice(inserted.as_bytes());
        payload.extend_from_slice(b"\\x1b[201~");
        payload
    } else {
        inserted.as_bytes().to_vec()
    };
    (inserted, payload)
}
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''fn clear_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
) {
    selected_block_ids.borrow_mut().clear();
    selected_block_id.set(None);
    selection_anchor_id.set(None);
    sync_finished_block_selection(finished, selected_block_ids, selected_block_id);
}
''',
    '''fn clear_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
) {
    selected_block_ids.borrow_mut().clear();
    selected_block_id.set(None);
    selection_anchor_id.set(None);
    sync_finished_block_selection(finished, selected_block_ids, selected_block_id);
}

fn clear_vte_text_selections(finished: &[FinishedBlock], active_vte: &Terminal) {
    active_vte.unselect_all();
    for block in finished {
        block.command_vte.unselect_all();
        block.output_vte.unselect_all();
    }
}
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''    pty_for_key: Rc<OwnedPty>,
    pty_synced_for_key: Rc<Cell<bool>>,
''',
    '''    pty_for_key: Rc<OwnedPty>,
    active_vte_for_key: Terminal,
    pty_synced_for_key: Rc<Cell<bool>>,
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''            pty_for_key,
            pty_synced_for_key,
''',
    '''            pty_for_key,
            active_vte_for_key,
            pty_synced_for_key,
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''                if extend_finished_block_selection(
                    &finished,
                    &selected_block_ids_for_key,
                    &selected_block_id_for_key,
                    &selection_anchor_id_for_key,
                    &block_scroll_for_key,
                    direction,
                ) {
                    return glib::Propagation::Stop;
                }
''',
    '''                if extend_finished_block_selection(
                    &finished,
                    &selected_block_ids_for_key,
                    &selected_block_id_for_key,
                    &selection_anchor_id_for_key,
                    &block_scroll_for_key,
                    direction,
                ) {
                    clear_vte_text_selections(&finished, &active_vte_for_key);
                    return glib::Propagation::Stop;
                }
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''                move_finished_block_selection(
                    &finished,
                    &selected_block_ids_for_key,
                    &selected_block_id_for_key,
                    &selection_anchor_id_for_key,
                    &block_scroll_for_key,
                    direction,
                );
                return glib::Propagation::Stop;
''',
    '''                if move_finished_block_selection(
                    &finished,
                    &selected_block_ids_for_key,
                    &selected_block_id_for_key,
                    &selection_anchor_id_for_key,
                    &block_scroll_for_key,
                    direction,
                ) {
                    clear_vte_text_selections(&finished, &active_vte_for_key);
                    return glib::Propagation::Stop;
                }
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''                move_finished_block_selection(
                    &finished,
                    &selected_block_ids_for_key,
                    &selected_block_id_for_key,
                    &selection_anchor_id_for_key,
                    &block_scroll_for_key,
                    direction,
                );
                return glib::Propagation::Stop;
            }

            // Enter recalls every selected command in terminal order as one
''',
    '''                if move_finished_block_selection(
                    &finished,
                    &selected_block_ids_for_key,
                    &selected_block_id_for_key,
                    &selection_anchor_id_for_key,
                    &block_scroll_for_key,
                    direction,
                ) {
                    clear_vte_text_selections(&finished, &active_vte_for_key);
                    return glib::Propagation::Stop;
                }
            }

            // Enter recalls every selected command in terminal order as one
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''                if let Some(idx) = target {
                    let new_id = finished.get(idx).map(|b| b.id);
                    replace_finished_block_selection(
''',
    '''                if let Some(idx) = target {
                    clear_vte_text_selections(&finished, &active_vte_for_key);
                    let new_id = finished.get(idx).map(|b| b.id);
                    replace_finished_block_selection(
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''            KeyCtx {
                pty_for_key,
                pty_synced_for_key: pty_synced.clone(),
''',
    '''            KeyCtx {
                pty_for_key,
                active_vte_for_key: active_vte.clone(),
                pty_synced_for_key: pty_synced.clone(),
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''                                    {
                                        let finished = finished_blocks_for_menu.borrow();
                                        activate_finished_block_selection(
''',
    '''                                    {
                                        let finished = finished_blocks_for_menu.borrow();
                                        clear_vte_text_selections(&finished, &vte_for_copy);
                                        activate_finished_block_selection(
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''            record_external_input(
                bstate.get(),
                text.as_bytes(),
                &typed_cmd,
                &pty_synced,
                &idle_input_dirty,
            );
''',
    '''            let (inserted_text, payload) =
                build_clipboard_paste(&text, bracketed_paste.get());
            if payload.is_empty() {
                return;
            }

            record_external_input(
                bstate.get(),
                inserted_text.as_bytes(),
                &typed_cmd,
                &pty_synced,
                &idle_input_dirty,
            );
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''            let payload = build_clipboard_paste_payload(&text, bracketed_paste.get());
            pty.write_bytes(&payload);
''',
    '''            pty.write_bytes(&payload);
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''        background_output_has_visible_text, build_clipboard_paste_payload, build_command_recall,
''',
    '''        background_output_has_visible_text, build_clipboard_paste, build_command_recall,
''',
)

replace_once(
    "src/block_view/mod.rs",
    '''    fn clipboard_paste_is_framed_as_one_payload() {
        assert_eq!(
            build_clipboard_paste_payload("one\\ntwo", false),
            b"one\\ntwo".to_vec()
        );
        assert_eq!(
            build_clipboard_paste_payload("one\\ntwo", true),
            b"\\x1b[200~one\\ntwo\\x1b[201~".to_vec()
        );
    }
''',
    '''    fn clipboard_paste_matches_the_effective_editor_text() {
        assert_eq!(
            build_clipboard_paste("one\\r\\ntwo", false),
            ("one".to_string(), b"one".to_vec())
        );
        assert_eq!(
            build_clipboard_paste("one\\r\\ntwo", true),
            (
                "one\\ntwo".to_string(),
                b"\\x1b[200~one\\ntwo\\x1b[201~".to_vec()
            )
        );
    }
''',
)
