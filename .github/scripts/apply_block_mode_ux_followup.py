from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}\n--- needle ---\n{old}")
    file_path.write_text(text.replace(old, new, 1))


# Virtualization should estimate the capped card, not the complete command
# output, otherwise the outer scroll range can retain a large phantom gap.
replace_once(
    "src/block_view/blocks.rs",
    '''pub(crate) fn estimated_finished_block_height_for_text(
    config: &Config,
    output: &str,
    cols: i64,
) -> i32 {
    let rows = output_visual_row_count(output, cols).max(1);
    estimated_finished_block_height(config, rows)
}
''',
    '''pub(crate) fn estimated_finished_block_height_for_text(
    config: &Config,
    output: &str,
    cols: i64,
) -> i32 {
    let rows = output_visual_row_count(output, cols).max(1);
    // Before the widget maps, the configured cap is the only stable height
    // available. Using all output rows here makes virtualization reserve a
    // hundreds-of-lines phantom block even though the mapped VTE is capped.
    let fallback_cap = (config.finished_block_viewport_rows as i64).max(3);
    estimated_finished_block_height(config, rows.min(fallback_cap))
}
''',
)

# Scrollability is a live VTE-adjustment property once the pane-sized cap is
# known. Remove the construction-time boolean, which is wrong for small panes.
replace_once(
    "src/block_view/blocks.rs",
    '''    /// True only when this block has more output rows than can be shown at once.
    pub(crate) output_scrollable: bool,
''',
    '''''',
)
replace_once(
    "src/block_view/blocks.rs",
    '''            output_scrollable: self.output_scrollable,
''',
    '''''',
)
replace_once(
    "src/block_view/blocks.rs",
    '''        let output_scrollable = long_output;
        let output_vte = create_finished_terminal(config, cols, output_rows, viewport_cap, false);
''',
    '''        let output_vte = create_finished_terminal(config, cols, output_rows, viewport_cap, false);
''',
)
replace_once(
    "src/block_view/blocks.rs",
    '''            output_scrollable,
            long_output,
''',
    '''            long_output,
''',
)

# Every output block gets an expand handler; map-time geometry decides whether
# the button is visible. This covers short output that still overflows a small
# pane.
replace_once(
    "src/block_view/blocks.rs",
    '''        if long_output {
            let expand_for_btn = expanded.clone();
            let output_vte_for_btn = output_vte.clone();
            let displayed_for_btn = displayed_output.clone();
            let current_cap_for_btn = current_viewport_cap.clone();
            let cols_for_btn = cols.max(1);
            expand_btn.connect_clicked(move |btn| {
                let now_expanded = !expand_for_btn.get();
                expand_for_btn.set(now_expanded);
                let rows = output_visual_row_count(&displayed_for_btn.borrow(), cols_for_btn);
                let fitted_cap = fitted_output_rows_for_widget(
                    &output_vte_for_btn,
                    current_cap_for_btn.get(),
                    rows,
                );
                current_cap_for_btn.set(fitted_cap);
                let cap = if now_expanded {
                    max_expanded_cap
                } else {
                    fitted_cap
                };
                let visible_rows = rows.min(cap).max(1);
                render_bytes_into_finished_vte(
                    &output_vte_for_btn,
                    &displayed_for_btn.borrow(),
                    cols_for_btn,
                    rows,
                    cap,
                    capture_rows,
                    false,
                );
                let ch = output_vte_for_btn.char_height() as i32;
                if ch > 0 {
                    output_vte_for_btn.set_height_request((visible_rows as i32) * ch);
                }
                btn.set_label(if now_expanded { "\\u{f066}" } else { "\\u{f065}" });
                btn.set_tooltip_text(Some(if now_expanded {
                    "Collapse to viewport height"
                } else {
                    "Expand block"
                }));
            });
        } else {
            expand_btn.set_visible(false);
        }
''',
    '''        // Geometry is finalized on map, so install the handler for every
        // block and let the map callback decide whether expansion is useful.
        expand_btn.set_visible(long_output);
        {
            let expand_for_btn = expanded.clone();
            let output_vte_for_btn = output_vte.clone();
            let displayed_for_btn = displayed_output.clone();
            let current_cap_for_btn = current_viewport_cap.clone();
            let cols_for_btn = cols.max(1);
            expand_btn.connect_clicked(move |btn| {
                let now_expanded = !expand_for_btn.get();
                expand_for_btn.set(now_expanded);
                let rows = output_visual_row_count(&displayed_for_btn.borrow(), cols_for_btn);
                let fitted_cap = fitted_output_rows_for_widget(
                    &output_vte_for_btn,
                    current_cap_for_btn.get(),
                    rows,
                );
                current_cap_for_btn.set(fitted_cap);
                let cap = if now_expanded {
                    max_expanded_cap
                } else {
                    fitted_cap
                };
                let visible_rows = rows.min(cap).max(1);
                render_bytes_into_finished_vte(
                    &output_vte_for_btn,
                    &displayed_for_btn.borrow(),
                    cols_for_btn,
                    rows,
                    cap,
                    capture_rows,
                    false,
                );
                let ch = output_vte_for_btn.char_height() as i32;
                if ch > 0 {
                    output_vte_for_btn.set_height_request((visible_rows as i32) * ch);
                }
                btn.set_label(if now_expanded { "\\u{f066}" } else { "\\u{f065}" });
                btn.set_tooltip_text(Some(if now_expanded {
                    "Collapse to viewport height"
                } else {
                    "Expand block"
                }));
            });
        }
''',
)

replace_once(
    "src/block_view/blocks.rs",
    '''        let block_for_jump = self.clone();
        let outer_for_jump = outer.clone();
        self.jump_bottom_btn.connect_clicked(move |_| {
            if block_for_jump.output_scrollable {
                if let Some(adj) = block_for_jump.output_vte.vadjustment() {
                    adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
                }
            } else {
                block_for_jump.scroll_to_edge(&outer_for_jump, true);
            }
        });
''',
    '''        let block_for_jump = self.clone();
        let outer_for_jump = outer.clone();
        self.jump_bottom_btn.connect_clicked(move |_| {
            if let Some(adj) = block_for_jump.output_vte.vadjustment() {
                let target = (adj.upper() - adj.page_size()).max(adj.lower());
                if target > adj.lower() + f64::EPSILON {
                    adj.set_value(target);
                    return;
                }
            }
            block_for_jump.scroll_to_edge(&outer_for_jump, true);
        });
''',
)

replace_once(
    "src/block_view/blocks.rs",
    '''        let scroll_ctrl =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let vte = self.output_vte.clone();
        let outer_for_vte = outer.clone();
        let output_scrollable = self.output_scrollable;
        scroll_ctrl.connect_scroll(move |_, _dx, dy| {
            if !output_scrollable {
                forward_outer_scroll(&outer_for_vte, dy);
                return glib::Propagation::Stop;
            }

            let Some(inner_adj) = vte.vadjustment() else {
''',
    '''        let scroll_ctrl =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let vte = self.output_vte.clone();
        let outer_for_vte = outer.clone();
        scroll_ctrl.connect_scroll(move |_, _dx, dy| {
            // The cap is determined only after map/resize. Inspect the actual
            // VTE adjustment on every wheel event rather than trusting a stale
            // construction-time flag.
            let Some(inner_adj) = vte.vadjustment() else {
''',
)

# Require the bottom target itself to remain unchanged across idle turns. Merely
# checking that set_value() succeeded made the loop stop after two iterations
# even while virtualization was still changing `upper`.
replace_once(
    "src/block_view/mod.rs",
    '''        let attempts = Rc::new(Cell::new(0u8));
        let stable_turns = Rc::new(Cell::new(0u8));
        glib::idle_add_local(move || {
''',
    '''        let attempts = Rc::new(Cell::new(0u8));
        let stable_turns = Rc::new(Cell::new(0u8));
        let last_target = Rc::new(Cell::new(None::<f64>));
        glib::idle_add_local(move || {
''',
)
replace_once(
    "src/block_view/mod.rs",
    '''            if (adj.value() - target).abs() < 1.0 {
                stable_turns.set(stable_turns.get().saturating_add(1));
            } else {
                stable_turns.set(0);
            }

            if stable_turns.get() >= 2 || attempts.get() >= 12 {
''',
    '''            let target_is_stable = last_target
                .get()
                .is_some_and(|previous| (previous - target).abs() < 1.0);
            last_target.set(Some(target));
            if target_is_stable && (adj.value() - target).abs() < 1.0 {
                stable_turns.set(stable_turns.get().saturating_add(1));
            } else {
                stable_turns.set(0);
            }

            if stable_turns.get() >= 2 || attempts.get() >= 12 {
''',
)

print("Applied block-mode UX follow-up")
