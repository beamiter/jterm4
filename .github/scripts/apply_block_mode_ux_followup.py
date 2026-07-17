#!/usr/bin/env python3
from pathlib import Path

def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f'{path}: expected one match, found {count}: {old[:80]!r}')
    file.write_text(text.replace(old, new, 1))

replace_once('src/block_view/mod.rs', 'fn history_edge_navigation_available(state: BlockState, editor_dirty: bool) -> bool {\n    !editor_dirty\n        && !matches!(\n            state,\n            BlockState::CollectingOutput | BlockState::AltScreen | BlockState::RawFallback\n        )\n}\n', 'fn history_edge_navigation_available(state: BlockState, editor_dirty: bool) -> bool {\n    !editor_dirty\n        && !matches!(\n            state,\n            BlockState::CollectingOutput | BlockState::AltScreen | BlockState::RawFallback\n        )\n}\n\nfn should_buffer_background_output(idle_input_dirty: bool, pty_synced: bool) -> bool {\n    !idle_input_dirty && !pty_synced\n}\n')
replace_once('src/block_view/mod.rs', '                                    if !idle_input_dirty_rc.get() {\n                                        let mut pending = background_output_rc.borrow_mut();\n', '                                    if should_buffer_background_output(\n                                        idle_input_dirty_rc.get(),\n                                        pty_synced_rc.get(),\n                                    ) {\n                                        let mut pending = background_output_rc.borrow_mut();\n')
replace_once('src/block_view/mod.rs', '        record_external_input, scroll_delta_to_reveal, selected_command_text, selected_id_range,\n        strip_ansi, strip_ansi_with_clear_detect, take_background_output,\n        truncate_plain_output_for_height, visible_indices_for_viewport, BlockData, BlockState,\n        ViewportState,\n', '        record_external_input, scroll_delta_to_reveal, selected_command_text, selected_id_range,\n        should_buffer_background_output, strip_ansi, strip_ansi_with_clear_detect,\n        take_background_output, truncate_plain_output_for_height, visible_indices_for_viewport,\n        BlockData, BlockState, ViewportState,\n')
replace_once('src/block_view/mod.rs', '    #[test]\n    fn restored_block_ids_are_unique_and_reserve_the_allocator() {\n', '    #[test]\n    fn programmatic_editor_sync_keeps_async_output_inline() {\n        assert!(should_buffer_background_output(false, false));\n        assert!(!should_buffer_background_output(true, false));\n        assert!(!should_buffer_background_output(false, true));\n    }\n\n    #[test]\n    fn restored_block_ids_are_unique_and_reserve_the_allocator() {\n')
