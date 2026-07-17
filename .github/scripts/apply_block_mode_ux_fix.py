from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}\n--- needle ---\n{old}")
    file_path.write_text(text.replace(old, new, 1))


# Long finished blocks should use a pane-sized inner viewport instead of
# allocating their complete output height on the outer document.
replace_once(
    "src/block_view/blocks.rs",
    '''fn collapsed_output_summary(rows: i64) -> String {
    format!("▸ {} hidden — click to show", line_count_text(rows))
}

fn block_edge_scroll_target(
''',
    '''fn collapsed_output_summary(rows: i64) -> String {
    format!("▸ {} hidden — click to show", line_count_text(rows))
}

/// Rows consumed by a finished block outside its output VTE: metadata header,
/// command row, and card chrome. Together with the compact live input rows this
/// leaves a long block filling the rest of the pane without growing the outer
/// document by hundreds of rows.
const FINISHED_BLOCK_NON_OUTPUT_ROWS: i64 = 3;

fn fitted_output_rows_for_viewport(
    viewport_rows: Option<i64>,
    fallback_rows: i64,
    output_rows: i64,
) -> i64 {
    let output_rows = output_rows.max(1);
    let reserve = super::MIN_INPUT_ROWS as i64 + FINISHED_BLOCK_NON_OUTPUT_ROWS;
    viewport_rows
        .map(|rows| rows.saturating_sub(reserve))
        .unwrap_or(fallback_rows)
        .max(3)
        .min(output_rows)
}

fn fitted_output_rows_for_widget(
    vte: &vte4::Terminal,
    fallback_rows: i64,
    output_rows: i64,
) -> i64 {
    let viewport_rows = vte
        .ancestor(gtk4::ScrolledWindow::static_type())
        .and_then(|widget| widget.downcast::<gtk4::ScrolledWindow>().ok())
        .and_then(|scroll| super::viewport_rows_for(vte, &scroll));
    fitted_output_rows_for_viewport(viewport_rows, fallback_rows, output_rows)
}

fn block_edge_scroll_target(
''',
)

replace_once(
    "src/block_view/blocks.rs",
    '''    let display_text = output_display_text(text);
    let visible_rows = output_rows.min(viewport_cap).clamp(1, 32);
    let overflow_rows = output_rows.saturating_sub(visible_rows).saturating_add(64);
''',
    '''    let display_text = output_display_text(text);
    // The pixel height request below is based on this same row count. Capping
    // the VTE grid at 32 while requesting a taller widget created the large
    // blank tail visible in long cards.
    let visible_rows = output_rows.min(viewport_cap).max(1);
    let overflow_rows = output_rows.saturating_sub(visible_rows).saturating_add(64);
''',
)

replace_once(
    "src/block_view/blocks.rs",
    '''    if expand_to_buffer {
        settle_finished_terminal_after_feed(vte);
    }
    if let Some(adj) = vte.vadjustment() {
        adj.set_value(adj.lower());
    }
}
''',
    '''    if expand_to_buffer {
        settle_finished_terminal_after_feed(vte);
    } else {
        // feed() settles asynchronously. Keep capped snapshots anchored at the
        // first retained row without invoking the full-height settle path.
        let vte = vte.clone();
        glib::idle_add_local_once(move || {
            if let Some(adj) = vte.vadjustment() {
                adj.set_value(adj.lower());
            }
        });
    }
    if let Some(adj) = vte.vadjustment() {
        adj.set_value(adj.lower());
    }
}
''',
)

replace_once(
    "src/block_view/blocks.rs",
    '''        let is_background = cmd.trim().is_empty();
        // Finished blocks stay on jterm4's full-height, virtualized outer canvas.
        // `long_output` is only an interaction threshold for explicit navigation.
        let output_rows = output_visual_row_count(output, cols);
        let viewport_cap = output_rows.max(1);
        let max_expanded_cap = viewport_cap;
        let long_output = output_rows > (config.finished_block_viewport_rows as i64).max(1);
        let capture_rows = output_rows
''',
    '''        let is_background = cmd.trim().is_empty();
        let output_rows = output_visual_row_count(output, cols);
        let fallback_viewport_cap = (config.finished_block_viewport_rows as i64).max(3);
        let viewport_cap =
            fitted_output_rows_for_viewport(None, fallback_viewport_cap, output_rows);
        let current_viewport_cap = Rc::new(Cell::new(viewport_cap));
        let max_expanded_cap = output_rows.max(viewport_cap);
        let long_output = output_rows > viewport_cap;
        let capture_rows = output_rows
''',
)

replace_once(
    "src/block_view/blocks.rs",
    '''        } else {
            let b = gtk4::Box::new(Orientation::Vertical, 0);
            b.add_css_class("block-finished");
            b
        };
        if config.block_compact {
''',
    '''        } else {
            let b = gtk4::Box::new(Orientation::Vertical, 0);
            b.add_css_class("block-finished");
            b
        };
        // Pooled cards must not retain expansion flags from an earlier use.
        // The output VTE owns the explicit height; the card itself never absorbs
        // spare vertical space from the document box.
        outer.set_hexpand(true);
        outer.set_vexpand(false);
        if config.block_compact {
''',
)

replace_once(
    "src/block_view/blocks.rs",
    '''        // Output VTE: full output is allocated at its complete wrapped height.
        // The outer ScrolledWindow is the sole vertical canvas.
        let full_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let displayed_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let output_scrollable = output_rows > viewport_cap;
        let output_vte = create_finished_terminal(config, cols, output_rows, viewport_cap, false);
        let initial_visible_rows = output_rows.min(viewport_cap).max(1);
        output_vte
            .set_height_request(initial_visible_rows as i32 * estimated_cell_height_px(config));
        // Tracks whether the user has toggled this block to its expanded
        // height. Survives unmap/remap so re-feeding picks the right cap.
        let expanded: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            let cols_for_map = cols.max(1);
            let cap_for_map = viewport_cap;
            let max_for_map = max_expanded_cap;
            let displayed_for_map = displayed_output.clone();
            let expanded_for_map = expanded.clone();
            output_vte.connect_map(move |w| {
                let text = displayed_for_map.borrow();
                let rows = output_visual_row_count(&text, cols_for_map);
                let cap = if expanded_for_map.get() {
                    max_for_map
                } else {
                    cap_for_map
                };
                let visible_rows = rows.min(cap).max(1);
                render_bytes_into_finished_vte(
                    w,
                    &text,
                    cols_for_map,
                    rows,
                    cap,
                    capture_rows,
                    true,
                );
                // Pin a minimum pixel height so GTK's vertical Box layout cannot
                // shrink this VTE below what set_size requested. Without this,
                // finished VTEs can be allocated at ~1 row and VTE scrolls their
                // content into internal scrollback. Do not clear on unmap: GTK
                // virtual scrolling and ordinary layout churn can unmap visible
                // blocks transiently, and clearing there loses output if a later
                // remap is skipped or coalesced.
                let ch = w.char_height() as i32;
                if ch > 0 {
                    w.set_height_request((visible_rows as i32) * ch);
                }
            });
        }

        // Show the expand toggle only when there's content beyond the cap.
        // Click swaps the output VTE between capped and expanded heights and
        // updates the icon (expand ↔ compress). The map handler reads the
        // shared `expanded` flag so a re-feed after scroll-off/on respects it.
        if output_rows > viewport_cap {
            let expand_for_btn = expanded.clone();
            let output_vte_for_btn = output_vte.clone();
            let displayed_for_btn = displayed_output.clone();
            let cols_for_btn = cols.max(1);
            expand_btn.connect_clicked(move |btn| {
                let now_expanded = !expand_for_btn.get();
                expand_for_btn.set(now_expanded);
                let cap = if now_expanded {
                    max_expanded_cap
                } else {
                    viewport_cap
                };
                let rows = output_visual_row_count(&displayed_for_btn.borrow(), cols_for_btn);
                let visible_rows = rows.min(cap).max(1);
                output_vte_for_btn.set_size(cols_for_btn, visible_rows);
                let ch = output_vte_for_btn.char_height() as i32;
                if ch > 0 {
                    output_vte_for_btn.set_height_request((visible_rows as i32) * ch);
                }
                btn.set_label(if now_expanded { "\\u{f066}" } else { "\\u{f065}" });
                btn.set_tooltip_text(Some(if now_expanded {
                    "Collapse to default height"
                } else {
                    "Expand block"
                }));
            });
        } else {
            expand_btn.set_visible(false);
        }
''',
    '''        // Long output receives an inner viewport sized from the current pane.
        // The compact live input remains visible below it; wheel events paginate
        // the VTE until an edge, then continue through the outer block document.
        let full_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let displayed_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let output_scrollable = long_output;
        let output_vte = create_finished_terminal(config, cols, output_rows, viewport_cap, false);
        let initial_visible_rows = output_rows.min(viewport_cap).max(1);
        output_vte
            .set_height_request(initial_visible_rows as i32 * estimated_cell_height_px(config));
        // Tracks whether the user has toggled this block to its complete height.
        // The default cap is recomputed whenever virtualization remaps the card.
        let expanded: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            let cols_for_map = cols.max(1);
            let fallback_cap_for_map = viewport_cap;
            let max_for_map = max_expanded_cap;
            let current_cap_for_map = current_viewport_cap.clone();
            let displayed_for_map = displayed_output.clone();
            let expanded_for_map = expanded.clone();
            let expand_btn_for_map = expand_btn.clone();
            let jump_btn_for_map = jump_bottom_btn.clone();
            output_vte.connect_map(move |w| {
                let text = displayed_for_map.borrow();
                let rows = output_visual_row_count(&text, cols_for_map);
                let fitted_cap =
                    fitted_output_rows_for_widget(w, fallback_cap_for_map, rows);
                current_cap_for_map.set(fitted_cap);
                let cap = if expanded_for_map.get() {
                    max_for_map
                } else {
                    fitted_cap
                };
                let visible_rows = rows.min(cap).max(1);
                let can_expand = rows > fitted_cap;
                expand_btn_for_map.set_visible(can_expand);
                jump_btn_for_map.set_visible(can_expand);
                render_bytes_into_finished_vte(
                    w,
                    &text,
                    cols_for_map,
                    rows,
                    cap,
                    capture_rows,
                    false,
                );
                // The VTE grid and pixel request use the identical row count.
                // This prevents GTK from allocating a tall empty card around a
                // smaller terminal surface.
                let ch = w.char_height() as i32;
                if ch > 0 {
                    w.set_height_request((visible_rows as i32) * ch);
                }
            });
        }

        if long_output {
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
)

replace_once(
    "src/block_view/blocks.rs",
    '''                let expand_btn = expand_btn.clone();
                let expanded = expanded.clone();
                let filter_btn = filter_btn.clone();
''',
    '''                let expand_btn = expand_btn.clone();
                let expanded = expanded.clone();
                let current_viewport_cap = current_viewport_cap.clone();
                let filter_btn = filter_btn.clone();
''',
)

replace_once(
    "src/block_view/blocks.rs",
    '''                    let shown_rows = output_row_count(&shown);
                    let shown_visual_rows = output_visual_row_count(&shown, cols);
                    let can_expand = shown_visual_rows > viewport_cap;
                    // A narrow filter result must not leave the block logically
                    // expanded; clearing the query should return to default height.
                    if !can_expand && expanded.replace(false) {
                        expand_btn.set_label("\\u{f065}");
                        expand_btn.set_tooltip_text(Some("Expand block"));
                    }
                    let active_cap = if expanded.get() {
                        max_expanded_cap
                    } else {
                        viewport_cap
                    };
                    render_bytes_into_finished_vte(
                        &output_vte,
                        &shown,
                        cols,
                        shown_visual_rows,
                        active_cap,
                        capture_rows,
                        true,
                    );
''',
    '''                    let shown_rows = output_row_count(&shown);
                    let shown_visual_rows = output_visual_row_count(&shown, cols);
                    let fitted_cap = fitted_output_rows_for_widget(
                        &output_vte,
                        current_viewport_cap.get(),
                        shown_visual_rows,
                    );
                    current_viewport_cap.set(fitted_cap);
                    let can_expand = shown_visual_rows > fitted_cap;
                    // A narrow filter result must not leave the block logically
                    // expanded; clearing the query should return to viewport height.
                    if !can_expand && expanded.replace(false) {
                        expand_btn.set_label("\\u{f065}");
                        expand_btn.set_tooltip_text(Some("Expand block"));
                    }
                    let active_cap = if expanded.get() {
                        max_expanded_cap
                    } else {
                        fitted_cap
                    };
                    render_bytes_into_finished_vte(
                        &output_vte,
                        &shown,
                        cols,
                        shown_visual_rows,
                        active_cap,
                        capture_rows,
                        false,
                    );
''',
)

replace_once(
    "src/block_view/blocks.rs",
    '''        let block_for_jump = self.clone();
        let outer_for_jump = outer.clone();
        self.jump_bottom_btn.connect_clicked(move |_| {
            block_for_jump.scroll_to_edge(&outer_for_jump, true);
        });
''',
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
)

replace_once(
    "src/block_view/blocks.rs",
    '''    #[test]
    fn visual_row_count_ignores_ansi_and_overwritten_progress_rows() {
''',
    '''    #[test]
    fn long_output_cap_fills_space_above_compact_input() {
        assert_eq!(super::fitted_output_rows_for_viewport(Some(60), 30, 200), 51);
        assert_eq!(super::fitted_output_rows_for_viewport(Some(60), 30, 40), 40);
        assert_eq!(super::fitted_output_rows_for_viewport(None, 30, 200), 30);
        assert_eq!(super::fitted_output_rows_for_viewport(Some(8), 30, 200), 3);
    }

    #[test]
    fn visual_row_count_ignores_ansi_and_overwritten_progress_rows() {
''',
)

# Tab activation must restore both keyboard focus and the outer block canvas to
# the live input, after GTK/virtualization have settled their final height.
replace_once(
    "src/block_view/mod.rs",
    '''    pub fn scroll_lines(&self, lines: i32) {
''',
    '''    /// Reveal the live input when its tab becomes active. A single bottom
    /// adjustment is too early during `switch-page`: mapping and virtualized block
    /// visibility can change `upper` for several idle turns. Re-pin until the
    /// geometry is stable, while marking every write as programmatic so it cannot
    /// accidentally engage history scroll-lock.
    pub(crate) fn reveal_live_input(&self) {
        self.user_scrolled_up.set(false);
        self.unread_count.set(0);
        set_jump_fab_label(&self.jump_fab, 0);
        self.jump_fab.set_visible(false);
        self.block_list.queue_allocate();

        let scroll = self.block_scroll.clone();
        let user_scrolled = self.user_scrolled_up.clone();
        let programmatic = self.programmatic_scroll.clone();
        let attempts = Rc::new(Cell::new(0u8));
        let stable_turns = Rc::new(Cell::new(0u8));
        glib::idle_add_local(move || {
            attempts.set(attempts.get().saturating_add(1));
            user_scrolled.set(false);

            let adj = scroll.vadjustment();
            let target = (adj.upper() - adj.page_size()).max(adj.lower());
            programmatic.set(true);
            adj.set_value(target);
            programmatic.set(false);

            if (adj.value() - target).abs() < 1.0 {
                stable_turns.set(stable_turns.get().saturating_add(1));
            } else {
                stable_turns.set(0);
            }

            if stable_turns.get() >= 2 || attempts.get() >= 12 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    pub fn scroll_lines(&self, lines: i32) {
''',
)

replace_once(
    "src/ui/actions.rs",
    '''    /// Return the page's exact live input surface.
    ///
    /// Block pages contain read-only VTE snapshots in addition to the active
    /// input VTE, so callers must use the typed pane controller rather than
    /// walking the GTK widget tree.
    pub(crate) fn terminal_in_page(&self, widget: &gtk4::Widget) -> Option<Terminal> {
        PaneNode::from_widget(widget).and_then(|node| node.active_terminal())
    }
''',
    '''    /// Return the page's exact Block controller, when the active leaf uses
    /// block mode. This mirrors `terminal_in_page` so tab activation never has to
    /// discover the live surface by walking through read-only snapshot VTEs.
    pub(crate) fn term_view_in_page(&self, widget: &gtk4::Widget) -> Option<Rc<TermView>> {
        PaneNode::from_widget(widget)
            .and_then(|node| node.active_leaf())
            .and_then(|leaf| leaf.block_view())
    }

    /// Return the page's exact live input surface.
    ///
    /// Block pages contain read-only VTE snapshots in addition to the active
    /// input VTE, so callers must use the typed pane controller rather than
    /// walking the GTK widget tree.
    pub(crate) fn terminal_in_page(&self, widget: &gtk4::Widget) -> Option<Terminal> {
        PaneNode::from_widget(widget).and_then(|node| node.active_terminal())
    }
''',
)

replace_once(
    "src/main.rs",
    '''            } else if let Some(target_terminal) = ui_for_switch.terminal_in_page(widget) {
                target_terminal.grab_focus();
''',
    '''            } else if let Some(target_terminal) = ui_for_switch.terminal_in_page(widget) {
                if let Some(term_view) = ui_for_switch.term_view_in_page(widget) {
                    term_view.reveal_live_input();
                }
                target_terminal.grab_focus();
''',
)

print("Applied block-mode tab focus and long-output viewport fix")
