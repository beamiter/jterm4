#!/usr/bin/env python3
from pathlib import Path


def replace_exact(path: Path, old: str, new: str, count: int = 1) -> None:
    text = path.read_text(encoding="utf-8")
    actual = text.count(old)
    if actual != count:
        raise RuntimeError(
            f"{path}: expected {count} occurrence(s), found {actual}: {old[:100]!r}"
        )
    path.write_text(text.replace(old, new, count), encoding="utf-8")


root = Path(".")

# Keep the filter-toggle closure so FinishedBlock can expose it to the sticky header.
replace_exact(
    root / "src/block_view/blocks.rs",
    """        // matching the query, honoring regex / case / invert / context-lines.
        {
            let filter_row = gtk4::Box::new(Orientation::Horizontal, 4);
""",
    """        // matching the query, honoring regex / case / invert / context-lines.
        let toggle_filter = {
            let filter_row = gtk4::Box::new(Orientation::Horizontal, 4);
""",
)

# Clone the history scroller before the per-block right-click closure owns it.
replace_exact(
    root / "src/block_view/mod.rs",
    """                                let finished_menu_clone = finished_clone.clone();
                                let block_data_for_export = block_data_for_cb.clone();
                                right_click.connect_pressed(move |gesture, _n_press, x, y| {
""",
    """                                let finished_menu_clone = finished_clone.clone();
                                let block_data_for_export = block_data_for_cb.clone();
                                let block_scroll_for_menu = block_scroll_rc.clone();
                                right_click.connect_pressed(move |gesture, _n_press, x, y| {
""",
)
replace_exact(
    root / "src/block_view/mod.rs",
    "let scroll = block_scroll_rc.clone();",
    "let scroll = block_scroll_for_menu.clone();",
    2,
)

config = root / "src/config.rs"
text = config.read_text(encoding="utf-8")
replacements = [
    (
        """    pub(crate) truncation_threshold_lines: u32,
    #[allow(dead_code)]
""",
        """    pub(crate) truncation_threshold_lines: u32,
    /// Output rows shown before a finished block is considered long and gains
    /// top/bottom navigation controls.
    pub(crate) finished_block_viewport_rows: u32,
    #[allow(dead_code)]
""",
    ),
    (
        """    pub(crate) block_history_compress: bool,
    /// Saved SSH targets selectable from the context menu.
""",
        """    pub(crate) block_history_compress: bool,
    /// Use jterm1/Warp-style denser block spacing.
    pub(crate) block_compact: bool,
    /// Saved SSH targets selectable from the context menu.
""",
    ),
    (
        '    "truncation_threshold_lines",\n    "max_collapsed_output_lines",\n',
        '    "truncation_threshold_lines",\n    "finished_block_viewport_rows",\n    "max_collapsed_output_lines",\n',
    ),
    (
        '    "block_history_compress",\n    "remote_hosts",\n',
        '    "block_history_compress",\n    "block_compact",\n    "remote_hosts",\n',
    ),
    (
        '        "truncation_threshold_lines",\n        "max_collapsed_output_lines",\n',
        '        "truncation_threshold_lines",\n        "finished_block_viewport_rows",\n        "max_collapsed_output_lines",\n',
    ),
    (
        '        "block_history_compress",\n        "mouse_reporting_enabled",\n',
        '        "block_history_compress",\n        "block_compact",\n        "mouse_reporting_enabled",\n',
    ),
    (
        '    warn_int_range(&mut issues, "truncation_threshold_lines", 1, 10_000_000);\n    warn_int_range(&mut issues, "max_collapsed_output_lines", 1, 1_000_000);\n',
        '    warn_int_range(&mut issues, "truncation_threshold_lines", 1, 10_000_000);\n    warn_int_range(&mut issues, "finished_block_viewport_rows", 3, 5_000);\n    warn_int_range(&mut issues, "max_collapsed_output_lines", 1, 1_000_000);\n',
    ),
    (
        """    truncation_threshold_lines: Option<u32>,
    max_collapsed_output_lines: Option<u32>,
""",
        """    truncation_threshold_lines: Option<u32>,
    finished_block_viewport_rows: Option<u32>,
    max_collapsed_output_lines: Option<u32>,
""",
    ),
    (
        """    block_history_compress: Option<bool>,
    remote_hosts: Vec<RemoteHost>,
""",
        """    block_history_compress: Option<bool>,
    block_compact: Option<bool>,
    remote_hosts: Vec<RemoteHost>,
""",
    ),
    (
        '        truncation_threshold_lines: table_u32(&table, "truncation_threshold_lines"),\n        max_collapsed_output_lines: table_u32(&table, "max_collapsed_output_lines"),\n',
        '        truncation_threshold_lines: table_u32(&table, "truncation_threshold_lines"),\n        finished_block_viewport_rows: table_u32(&table, "finished_block_viewport_rows"),\n        max_collapsed_output_lines: table_u32(&table, "max_collapsed_output_lines"),\n',
    ),
    (
        """        block_history_compress: table
            .get("block_history_compress")
            .and_then(|v| v.as_bool()),
        remote_hosts,
""",
        """        block_history_compress: table
            .get("block_history_compress")
            .and_then(|v| v.as_bool()),
        block_compact: table.get("block_compact").and_then(|v| v.as_bool()),
        remote_hosts,
""",
    ),
    (
        '    let max_collapsed_output_lines = env_u32("JTERM4_MAX_COLLAPSED_LINES")\n',
        '    let finished_block_viewport_rows = env_u32("JTERM4_FINISHED_VIEWPORT_ROWS")\n        .or(fc.finished_block_viewport_rows)\n        .unwrap_or(24)\n        .clamp(3, 5_000);\n    let max_collapsed_output_lines = env_u32("JTERM4_MAX_COLLAPSED_LINES")\n',
    ),
    (
        """    let block_history_compress = fc.block_history_compress.unwrap_or(true);
    let shell = std::env::var("JTERM4_SHELL").ok().or(fc.shell);
""",
        """    let block_history_compress = fc.block_history_compress.unwrap_or(true);
    let block_compact = match std::env::var("JTERM4_BLOCK_COMPACT").ok().as_deref() {
        Some("1") | Some("true") => Some(true),
        Some("0") | Some("false") => Some(false),
        _ => None,
    }
    .or(fc.block_compact)
    .unwrap_or(false);
    let shell = std::env::var("JTERM4_SHELL").ok().or(fc.shell);
""",
    ),
    (
        """        truncation_threshold_lines,
        max_collapsed_output_lines,
""",
        """        truncation_threshold_lines,
        finished_block_viewport_rows,
        max_collapsed_output_lines,
""",
    ),
    (
        """        block_history_compress,
        remote_hosts: fc.remote_hosts,
""",
        """        block_history_compress,
        block_compact,
        remote_hosts: fc.remote_hosts,
""",
    ),
    (
        """    table.insert(
        "show_repo_strip".into(),
""",
        """    table.insert(
        "finished_block_viewport_rows".into(),
        toml::Value::Integer(config.finished_block_viewport_rows as i64),
    );
    table.insert(
        "block_compact".into(),
        toml::Value::Boolean(config.block_compact),
    );
    table.insert(
        "show_repo_strip".into(),
""",
    ),
]
for old, new in replacements:
    actual = text.count(old)
    if actual != 1:
        raise RuntimeError(
            f"{config}: expected one occurrence, found {actual}: {old[:100]!r}"
        )
    text = text.replace(old, new, 1)
config.write_text(text, encoding="utf-8")

replace_exact(
    root / "config.toml.example",
    """truncation_threshold_lines = 50000
max_collapsed_output_lines = 25
virtual_scroll_margin = 1
""",
    """truncation_threshold_lines = 50000
finished_block_viewport_rows = 24
max_collapsed_output_lines = 25
virtual_scroll_margin = 1
block_compact = false
""",
)

print("Applied block-mode compile/config fixes.")
