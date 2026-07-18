#!/usr/bin/env python3
from pathlib import Path

path = Path("/tmp/apply_block_capture_contrast_fix.py")
source = path.read_text(encoding="utf-8")

old_import = (
    "        coalesce_bytes_events, compute_viewport_state, normalize_captured_command,\n"
    "        scroll_delta_to_reveal, selected_command_text, selected_id_range, strip_ansi,\n"
)
old_replacement = (
    "        coalesce_bytes_events, compute_viewport_state, normalize_captured_command,\n"
    "        resolve_submitted_command, scroll_delta_to_reveal, selected_command_text,\n"
    "        selected_id_range, strip_ansi,\n"
)
current_import = (
    "        background_output_has_visible_text, build_clipboard_paste, build_command_recall,\n"
    "        build_keyboard_query_reply, coalesce_bytes_events, compute_viewport_state,\n"
    "        history_edge_navigation_available, normalize_captured_command, normalize_loaded_block_ids,\n"
    "        record_external_input, scroll_delta_to_reveal, selected_command_text, selected_id_range,\n"
    "        should_buffer_background_output, strip_ansi, strip_ansi_with_clear_detect,\n"
    "        take_background_output, truncate_plain_output_for_height, visible_indices_for_viewport,\n"
    "        BlockData, BlockState, ViewportState,\n"
)
current_replacement = (
    "        background_output_has_visible_text, build_clipboard_paste, build_command_recall,\n"
    "        build_keyboard_query_reply, coalesce_bytes_events, compute_viewport_state,\n"
    "        history_edge_navigation_available, normalize_captured_command, normalize_loaded_block_ids,\n"
    "        record_external_input, resolve_submitted_command, scroll_delta_to_reveal,\n"
    "        selected_command_text, selected_id_range, should_buffer_background_output, strip_ansi,\n"
    "        strip_ansi_with_clear_detect, take_background_output, truncate_plain_output_for_height,\n"
    "        visible_indices_for_viewport, BlockData, BlockState, ViewportState,\n"
)

old_count = source.count(old_import)
replacement_count = source.count(old_replacement)
print(f"PR 32 patch import counts: old={old_count}, replacement={replacement_count}")
if old_count != 1 or replacement_count != 1:
    raise SystemExit("could not adapt PR 32 test import assertion")

source = source.replace(old_import, current_import, 1)
source = source.replace(old_replacement, current_replacement, 1)
path.write_text(source, encoding="utf-8")
