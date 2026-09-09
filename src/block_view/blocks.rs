//! blocks — finished-block widgets (VTE-backed) and the live ActiveBlock.
use super::bounded_bytes::BoundedByteRing;
use super::*;
use crate::config::Config;
use crate::terminal::open_uri;
use gtk4::Orientation;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use vte4::Terminal;
use vte4::TerminalExt;

/// Conservative per-pane estimated-memory budget for completed-block objects.
///
/// This is deliberately independent of the configurable record-count limit:
/// a handful of ANSI-heavy snapshots can otherwise retain hundreds of MiB in
/// duplicate Strings and VTE buffers long before the count limit is reached.
/// The newest block is the sole exception when it cannot fit by itself.
pub(crate) const MAX_COMPLETED_BLOCK_RETAINED_BYTES: usize = 128 * 1024 * 1024;
const FINISHED_OUTPUT_FILTER_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

// A finished card owns two VTEs plus a sizeable GTK widget/controller tree.
// These bases cover allocations which cannot be recovered from text lengths.
const RETAINED_BYTES_PER_VTE_BASE: usize = 128 * 1024;
const RETAINED_BYTES_PER_WIDGET_TREE_BASE: usize = 256 * 1024;
const FINISHED_BLOCK_FIXED_RETAINED_BYTES: usize =
    2 * RETAINED_BYTES_PER_VTE_BASE + RETAINED_BYTES_PER_WIDGET_TREE_BASE;

// For ordinary output, full_output, the optional filtered display override,
// stripped_output, and BlockData.output can be similarly sized byte owners.
// Keep the original capture multiple as a conservative floor for repaint-heavy
// streams, then separately charge the actual rendered/plain lengths. VTE
// stores a terminal grid (cells plus attributes and row metadata), not a byte
// string: charging
// one byte per printable cell made a pane with a few multi-megabyte blocks
// exceed the advertised budget by a wide margin. 32 bytes per materialized
// cell/row byte is a deliberately conservative retained-memory estimate for
// the output and command VTE grids; plain reconstruction also covers expansion
// from tabs and sparse cursor movement.
const ORIGINAL_OUTPUT_RETENTION_EQUIVALENT: usize = 5;
const MATERIALIZED_OUTPUT_RETAINED_OWNERS: usize = 3;
const PLAIN_OUTPUT_RETAINED_OWNERS: usize = 2;
const VTE_RETAINED_BYTES_PER_MATERIALIZED_BYTE: usize = 32;
// GDK may retain both decoded pixels and the encoded/source backing.
const IMAGE_RETAINED_OWNERS: usize = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CompletedBlockRetentionPlan {
    /// Number of oldest entries to remove from every block-indexed collection.
    pub(crate) evict_prefix: usize,
    pub(crate) retained_count: usize,
    pub(crate) retained_estimated_bytes: usize,
    /// Evictions required in addition to those imposed by the count limit.
    pub(crate) byte_budget_evictions: usize,
    /// The explicit newest-wins exception to the hard byte cap.
    pub(crate) newest_exceeds_byte_budget: bool,
}

/// Plan prefix eviction over oldest-to-newest `(block_id, estimated_bytes)`.
///
/// Scanning from the newest entry makes the latest-wins rule explicit and
/// avoids summing entries which the count cap would discard anyway. Arithmetic
/// overflow is treated as over-budget. `max_blocks == 0` still retains one
/// newest block, matching the byte-budget exception.
pub(crate) fn completed_block_retention_plan(
    blocks: &[(u64, usize)],
    max_blocks: usize,
    max_bytes: usize,
) -> CompletedBlockRetentionPlan {
    let Some(&(_, newest_bytes)) = blocks.last() else {
        return CompletedBlockRetentionPlan::default();
    };

    let count_limit = max_blocks.max(1);
    let count_limited_start = blocks.len().saturating_sub(count_limit);
    let mut retained_start = blocks.len() - 1;
    let mut retained_count = 1;
    let mut retained_estimated_bytes = newest_bytes;

    for index in (count_limited_start..retained_start).rev() {
        let Some(next_bytes) = retained_estimated_bytes.checked_add(blocks[index].1) else {
            break;
        };
        if next_bytes > max_bytes {
            break;
        }
        retained_start = index;
        retained_count += 1;
        retained_estimated_bytes = next_bytes;
    }

    CompletedBlockRetentionPlan {
        evict_prefix: retained_start,
        retained_count,
        retained_estimated_bytes,
        byte_budget_evictions: retained_start.saturating_sub(count_limited_start),
        newest_exceeds_byte_budget: newest_bytes > max_bytes,
    }
}

#[allow(clippy::too_many_arguments)]
fn estimated_completed_block_retained_bytes(
    prompt_bytes: usize,
    command_bytes: usize,
    command_markup_bytes: usize,
    rendered_command_bytes: usize,
    raw_output_bytes: usize,
    materialized_output_bytes: usize,
    plain_output_bytes: usize,
    cwd_bytes: usize,
    image_pixel_bytes: usize,
) -> usize {
    // A collapsed repaint stream contains only printable bytes, CR/LF, and
    // SGR. Its later stripped cache cannot exceed the materialized byte length;
    // use the larger value so both the cache and BlockData copy stay covered.
    let plain_owner_bytes = plain_output_bytes.max(materialized_output_bytes);
    let output_bytes = raw_output_bytes
        .saturating_mul(ORIGINAL_OUTPUT_RETENTION_EQUIVALENT)
        .max(
            materialized_output_bytes
                .saturating_mul(MATERIALIZED_OUTPUT_RETAINED_OWNERS)
                .saturating_add(plain_owner_bytes.saturating_mul(PLAIN_OUTPUT_RETAINED_OWNERS))
                .saturating_add(
                    plain_owner_bytes
                        .min(MAX_FINISHED_VTE_GRID_CELLS)
                        .saturating_mul(VTE_RETAINED_BYTES_PER_MATERIALIZED_BYTE),
                ),
        );
    FINISHED_BLOCK_FIXED_RETAINED_BYTES
        .saturating_add(output_bytes)
        // BlockData.cmd plus FinishedBlock.cmd_text.
        .saturating_add(command_bytes.saturating_mul(2))
        // The map closure owns the rendered command while VTE retains a cell
        // grid whose allocation is substantially larger than the UTF-8 feed.
        .saturating_add(rendered_command_bytes)
        .saturating_add(
            rendered_command_bytes
                .min(MAX_FINISHED_VTE_GRID_CELLS)
                .saturating_mul(VTE_RETAINED_BYTES_PER_MATERIALIZED_BYTE),
        )
        // BlockData.prompt plus FinishedBlock.prompt_text.
        .saturating_add(prompt_bytes.saturating_mul(2))
        .saturating_add(command_markup_bytes)
        // BlockData.cwd plus the rendered cwd chip.
        .saturating_add(cwd_bytes.saturating_mul(2))
        .saturating_add(image_pixel_bytes.saturating_mul(IMAGE_RETAINED_OWNERS))
        .saturating_add(std::mem::size_of::<BlockData>())
        .saturating_add(std::mem::size_of::<FinishedBlock>())
}

/// Upper-bound terminal grid units from the UTF-8/control stream without
/// parsing it a second time. Every byte is charged as one unit; HT receives an
/// additional full-row allowance because a one-byte tab may advance to the
/// right margin after applications modify tab stops. ANSI sparse-cursor
/// expansion is covered separately by the reconstructed plain-output length.
fn terminal_grid_units_upper_bound(bytes: &[u8], cols: usize) -> usize {
    let tab_extra = cols.max(1).saturating_sub(1);
    bytes.iter().fold(bytes.len(), |units, byte| {
        if *byte == b'\t' {
            units.saturating_add(tab_extra)
        } else {
            units
        }
    })
}

/// Conservative estimate available before the GTK/VTE widget tree exists.
/// This lets the live-finalize path evict old cards before constructing a
/// potentially huge newest card, avoiding an old-budget + new-card RSS spike.
#[allow(clippy::too_many_arguments)]
pub(crate) fn estimated_live_finished_block_retained_bytes(
    prompt: &str,
    cmd: &str,
    cmd_ansi: Option<&str>,
    raw_output: &str,
    plain_output: &str,
    cwd: Option<&str>,
    cols: i64,
    images: &[gtk4::gdk::Texture],
) -> usize {
    let cols = cols.max(1) as usize;
    let display_cmd = jterm_core::review_input::safe_multiline_display(
        cmd,
        jterm_core::review_input::MAX_REVIEW_INPUT_BYTES,
    );
    let command = finished_command_bytes(&display_cmd);
    let image_pixel_bytes = images.iter().fold(0usize, |total, texture| {
        total.saturating_add(
            (texture.width().max(0) as usize)
                .saturating_mul(texture.height().max(0) as usize)
                .saturating_mul(4),
        )
    });
    estimated_completed_block_retained_bytes(
        prompt.len(),
        cmd.len(),
        cmd_ansi.map_or(0, str::len),
        command
            .len()
            .max(terminal_grid_units_upper_bound(&command, cols).min(MAX_FINISHED_VTE_GRID_CELLS)),
        raw_output.len(),
        plain_output.len(),
        plain_output.len().max(
            terminal_grid_units_upper_bound(plain_output.as_bytes(), cols)
                .min(MAX_FINISHED_VTE_GRID_CELLS),
        ),
        cwd.map_or(0, str::len),
        image_pixel_bytes,
    )
    .saturating_add(if images.is_empty() {
        0
    } else {
        // Pending admission charges encoded PNG backing, decoded pixels and
        // every Texture/Picture object together. The exact per-image split is
        // no longer available after we move only Textures into FinishedBlock,
        // so charge the complete block graphics budget conservatively.
        super::kitty_graphics::MAX_PENDING_BYTES_PER_BLOCK
    })
}

// ─── FinishedBlock ────────────────────────────────────────────────────────────

pub(crate) const BLOCK_LIFECYCLE_SCHEMA: u32 = 0x4a54_4c31;

fn block_lifecycle_schema() -> u32 {
    BLOCK_LIFECYCLE_SCHEMA
}

/// Data for a finished command block (decoupled from widget representation)
#[derive(Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub(crate) struct BlockData {
    pub(crate) id: u64,
    pub(crate) prompt: String,
    pub(crate) cmd: String,
    pub(crate) cmd_markup: Option<String>,
    pub(crate) output: String,
    /// `None` when the shell reported no exit status for this command (a bare
    /// FinalTerm `D` mark), and for background output, which belongs to no
    /// command at all. Deliberately not folded into `Some(0)`: an unknown
    /// outcome rendered as a success is how a failed command looks fine.
    pub(crate) exit_code: Option<i32>,
    #[serde(skip, default = "block_lifecycle_schema")]
    pub(crate) lifecycle_schema: u32,
    #[serde(default)]
    pub(crate) completion_provenance: CompletionProvenanceWire,
    #[serde(default)]
    pub(crate) start_mark_seen: bool,
    pub(crate) estimated_height: i32,
    pub(crate) line_count: usize,
    #[serde(default)]
    pub(crate) start_time_ms: Option<u64>,
    #[serde(default)]
    pub(crate) end_time_ms: Option<u64>,
    #[serde(default)]
    pub(crate) duration_ms: Option<u64>,
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    /// Live-VTE column count at the time this block was finalized. Restored
    /// blocks render at the same cols so their byte stream (which was formatted
    /// for this width, e.g. by `ls`) reproduces the original line breaks
    /// instead of being reflowed at the current window's width. 0 = unknown
    /// (old saves before this field existed) — caller should fall back.
    #[serde(default)]
    pub(crate) cols: u16,
    /// The command line came from the shell's own OSC 133 report, not a
    /// screen scrape. Defaults to false so blocks saved before this field
    /// existed can never pose as exact evidence for agent tasks.
    #[serde(default)]
    pub(crate) command_exact: bool,
    /// The shell admitted its command report was truncated; the visible text
    /// must not be treated as the command that actually ran.
    #[serde(default)]
    pub(crate) command_truncated: bool,
}

pub(super) fn markdown_fence(text: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(longest.saturating_add(1).max(3))
}

impl BlockData {
    pub(crate) fn is_background(&self) -> bool {
        self.cmd.trim().is_empty()
    }

    pub(crate) fn lifecycle_health(&self) -> BlockLifecycleHealth {
        assess_lifecycle(self.start_mark_seen, self.completion_provenance.into())
    }

    pub(crate) fn timing_is_authoritative(&self) -> bool {
        let provenance = CompletionProvenance::from(self.completion_provenance);
        self.is_background()
            || provenance == CompletionProvenance::JournalRecovered
            || (provenance == CompletionProvenance::ShellReported && self.start_mark_seen)
    }

    pub(crate) fn lifecycle_notice(&self) -> Option<String> {
        if self.is_background() {
            return None;
        }
        match self.lifecycle_health() {
            BlockLifecycleHealth::Healthy => None,
            BlockLifecycleHealth::Recovered => Some(
                "Recovered command record — terminal rows were reconstructed from session history"
                    .to_string(),
            ),
            BlockLifecycleHealth::Degraded => Some(match self.completion_provenance.into() {
                CompletionProvenance::BoundaryInferred =>
                    "Command completion inferred from a trusted prompt boundary; exit status and timing are unavailable".to_string(),
                CompletionProvenance::ShellReported =>
                    "The shell reported an end marker without a matching command-start marker".to_string(),
                _ => "Command lifecycle provenance is degraded".to_string(),
            }),
            BlockLifecycleHealth::Incomplete => Some(
                "Command lifecycle is incomplete; no trusted completion source was retained"
                    .to_string(),
            ),
        }
    }

    /// Conservative cost of rebuilding this text-only history record as a
    /// finished card. Persisted output has no separate ANSI source, so its
    /// exact byte length is charged as the raw snapshot length.
    pub(crate) fn estimated_restored_retained_bytes(&self) -> usize {
        let display_cmd = jterm_core::review_input::safe_multiline_display(
            &self.cmd,
            jterm_core::review_input::MAX_REVIEW_INPUT_BYTES,
        );
        let rendered_command = finished_command_bytes(&display_cmd);
        // `cols == 0` is the legacy on-disk sentinel. Rendering later falls
        // back to the live terminal width, which is unavailable during the
        // retention plan; use the persisted u16 ceiling so tab expansion can
        // never be underestimated before widgets are built.
        let cols = if self.cols == 0 {
            u16::MAX as usize
        } else {
            usize::from(self.cols)
        };
        estimated_completed_block_retained_bytes(
            self.prompt.len(),
            self.cmd.len(),
            self.cmd_markup.as_ref().map_or(0, String::len),
            rendered_command.len().max(
                terminal_grid_units_upper_bound(&rendered_command, cols)
                    .min(MAX_FINISHED_VTE_GRID_CELLS),
            ),
            self.output.len(),
            self.output.len(),
            self.output.len().max(
                terminal_grid_units_upper_bound(self.output.as_bytes(), cols)
                    .min(MAX_FINISHED_VTE_GRID_CELLS),
            ),
            self.cwd.as_ref().map_or(0, String::len),
            0,
        )
    }

    /// Export block to JSON format
    pub fn to_json(&self) -> String {
        let Ok(mut value) = serde_json::to_value(self) else {
            return "{}".to_string();
        };
        if let Some(object) = value.as_object_mut() {
            if self.is_background() {
                object.remove("completion_provenance");
                object.remove("start_mark_seen");
            } else {
                object.insert(
                    "lifecycle_health".to_string(),
                    serde_json::Value::String(self.lifecycle_health().schema_name().to_string()),
                );
            }
            if !self.timing_is_authoritative() {
                for key in ["start_time_ms", "end_time_ms", "duration_ms"] {
                    object.insert(key.to_string(), serde_json::Value::Null);
                }
            }
        }
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
    }

    /// Export block to Markdown format
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();

        if self.is_background() {
            md.push_str("## Background Output\n\n");
        } else {
            md.push_str("## Command Block\n\n");

            if !self.prompt.is_empty() {
                let fence = markdown_fence(&self.prompt);
                md.push_str(&format!(
                    "**Prompt:**\n{fence}text\n{}\n{fence}\n\n",
                    self.prompt
                ));
            }

            let fence = markdown_fence(&self.cmd);
            md.push_str(&format!("**Command:**\n{fence}bash\n"));
            md.push_str(&self.cmd);
            md.push_str(&format!("\n{fence}\n\n"));
        }

        if !self.output.is_empty() {
            let fence = markdown_fence(&self.output);
            md.push_str(&format!("**Output:**\n{fence}\n"));
            md.push_str(&self.output);
            md.push_str(&format!("\n{fence}\n\n"));
        }

        if !self.is_background() {
            match self.exit_code {
                Some(code) => md.push_str(&format!("**Exit Code:** {code}\n\n")),
                None => md.push_str("**Exit Code:** unknown (the shell reported none)\n\n"),
            }
            md.push_str(&format!(
                "**Lifecycle:** {} ({})\n\n",
                self.lifecycle_health().schema_name(),
                self.completion_provenance.as_str(),
            ));
        }

        if let Some(dur) = self
            .timing_is_authoritative()
            .then_some(self.duration_ms)
            .flatten()
        {
            let dur_sec = dur as f64 / 1000.0;
            md.push_str(&format!("**Duration:** {:.3}s\n\n", dur_sec));
        }

        md
    }
}

/// Shown wherever an unknown exit status is presented, because "the badge says
/// `?`" is not self-explanatory and the honest answer is short.
pub(crate) const UNKNOWN_EXIT_TOOLTIP: &str = "The shell reported no exit status for this command";

/// Stand-in code for the shared surfaces whose types predate an unknown status
/// and take a plain `i32`: the family's command-history JSONL
/// (`jterm_core::command_history`), the AI block context
/// (`jterm_core::ai::BlockContext`) and jagent's observation turn.
///
/// `-1` is not a POSIX wait status — real ones are `0..=255`, with signals as
/// `128 + n` — so nothing downstream can read it as a success or as a signal
/// death, which is exactly what folding the case into `0` used to do. Surfaces
/// that also carry free text pair it with [`UNKNOWN_EXIT_NOTE`].
pub(crate) const UNKNOWN_EXIT_SENTINEL: i32 = -1;

/// The same fact in words, for the surfaces that send text to a model.
pub(crate) const UNKNOWN_EXIT_NOTE: &str =
    "[terminal] the shell reported no exit status for this command";

/// Split an optional exit status into the `i32` a shared surface requires plus
/// the note that makes an unknown status legible in accompanying text.
pub(crate) fn exit_code_for_shared_surface(exit_code: Option<i32>) -> (i32, Option<&'static str>) {
    match exit_code {
        Some(code) => (code, None),
        None => (UNKNOWN_EXIT_SENTINEL, Some(UNKNOWN_EXIT_NOTE)),
    }
}

/// How a finished block presents its outcome.
///
/// `Unknown` is a case of its own: a shell that emits the bare FinalTerm `D`
/// mark tells us a command ended but not how, and rendering that with the green
/// check of a success is how a failure disappears from the history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockOutcome {
    Background,
    Success,
    Failure(i32),
    /// The command was stopped rather than having failed on its own: a signal
    /// the user (or Forge's own Stop button) sent, or a consumer that closed
    /// the pipe. See [`BlockOutcome::interrupt_signal`].
    Interrupted(i32),
    Unknown,
}

impl BlockOutcome {
    /// Exit statuses that mean "this was stopped", not "this went wrong".
    ///
    /// Painting these as hard failures floods a long session with red at
    /// exactly the moments the user was in control: leaving `top`, ending a
    /// `tail -f`, cutting a runaway test short. Forge manufactures the case
    /// itself, too: the live card's Stop button writes `\x03`, so the block it
    /// produces was interrupted by Forge's own UI.
    ///
    /// `128 + signal`, restricted to the three signals that carry no fault:
    /// SIGINT (Ctrl+C), SIGPIPE (the reader went away, as in `... | head`) and
    /// SIGTERM (an orderly stop request). Faults keep their failure styling:
    /// SIGSEGV, SIGABRT and SIGQUIT are real crashes, and SIGKILL is usually
    /// the OOM killer, all of which the user needs to see in red.
    ///
    /// A script that genuinely exits 130 for its own reasons is misread here.
    /// The raw code stays visible in the badge, in export and in history for
    /// exactly that reason.
    const fn interrupt_signal(exit_code: i32) -> Option<&'static str> {
        match exit_code {
            130 => Some("SIGINT"),
            141 => Some("SIGPIPE"),
            143 => Some("SIGTERM"),
            _ => None,
        }
    }

    /// Translate the shared semantic contract into Forge's renderer-owned UI
    /// enum. `resolved_command` must be the final command after Forge's
    /// metadata/screen fallback, never the optional field from a raw OSC mark.
    pub(crate) fn classify(resolved_command: Option<&str>, exit_code: Option<i32>) -> Self {
        use jterm_core::block_contract::CompletedBlockOutcome;

        match jterm_core::block_contract::classify_completed(resolved_command, exit_code) {
            CompletedBlockOutcome::Background => Self::Background,
            CompletedBlockOutcome::Success => Self::Success,
            CompletedBlockOutcome::Failed(code) if Self::interrupt_signal(code).is_some() => {
                Self::Interrupted(code)
            }
            CompletedBlockOutcome::Failed(code) => Self::Failure(code),
            CompletedBlockOutcome::Unknown => Self::Unknown,
        }
    }

    /// Classify a record whose owning backend has already established that it
    /// represents a command lifecycle. This deliberately does not inspect
    /// command text: metadata identity remains authoritative even if a legacy
    /// foreground record has lost that display field.
    pub(crate) fn classify_foreground(exit_code: Option<i32>) -> Self {
        match exit_code {
            Some(0) => Self::Success,
            Some(code) if Self::interrupt_signal(code).is_some() => Self::Interrupted(code),
            Some(code) => Self::Failure(code),
            None => Self::Unknown,
        }
    }

    /// Whether this outcome counts as a failure for the scrollbar ticks, the
    /// Failed filter and failure navigation.
    pub(crate) const fn is_failure(self) -> bool {
        matches!(self, Self::Failure(_))
    }

    pub(crate) const fn reported_exit_code(self) -> Option<i32> {
        match self {
            Self::Success => Some(0),
            Self::Failure(code) | Self::Interrupted(code) => Some(code),
            Self::Background | Self::Unknown => None,
        }
    }

    /// Every value `stripe_css_class` can return. The card pool clears all of
    /// them before reusing a widget, so the two must not drift apart.
    const STRIPE_CSS_CLASSES: [&'static str; 5] = [
        "block-background",
        "block-success",
        "block-failed",
        "block-interrupted",
        "block-unknown",
    ];

    fn stripe_css_class(self) -> &'static str {
        match self {
            Self::Background => "block-background",
            Self::Success => "block-success",
            Self::Failure(_) => "block-failed",
            Self::Interrupted(_) => "block-interrupted",
            Self::Unknown => "block-unknown",
        }
    }

    /// Status glyphs available in ordinary system UI fonts.
    fn status_glyph(self) -> &'static str {
        match self {
            Self::Background => "↻",
            Self::Success => "✓",
            Self::Failure(_) => "✕",
            Self::Interrupted(_) => "⊘",
            Self::Unknown => "?",
        }
    }

    fn accessible_label(self) -> &'static str {
        match self {
            Self::Background => "Background output",
            Self::Success => "Command succeeded",
            Self::Failure(_) => "Command failed",
            Self::Interrupted(_) => "Command interrupted",
            Self::Unknown => "Command exit status unavailable",
        }
    }

    fn status_css_class(self) -> &'static str {
        match self {
            Self::Background => "block-status-background",
            Self::Success => "block-status-ok",
            Self::Failure(_) => "block-status-bad",
            Self::Interrupted(_) => "block-status-interrupted",
            Self::Unknown => "block-status-unknown",
        }
    }
}

pub(crate) fn block_clipboard_text(cmd: &str, output: &str, output_only: bool) -> String {
    if output_only || cmd.trim().is_empty() {
        output.to_string()
    } else if output.trim().is_empty() {
        cmd.to_string()
    } else {
        format!("{}\n{}", cmd, output)
    }
}

/// Filters for searching/filtering blocks
#[derive(Clone, Default)]
pub struct BlockFilters {
    pub exit_code: Option<i32>,
    pub min_duration_ms: Option<u64>,
    pub max_duration_ms: Option<u64>,
    pub failed_only: bool,
    pub slow_only: bool,
    pub background_only: bool,
    pub bookmarked_only: bool,
    pub slow_threshold_ms: u64,
    pub use_regex: bool,
}

pub(crate) struct FinishedBlock {
    pub(crate) id: u64,
    /// Commandless output emitted while the shell prompt was idle.
    pub(crate) is_background: bool,
    pub(crate) widget: gtk4::Box,
    /// Inner card content. Virtualization hides this child while the outer box
    /// retains a measured placeholder height, keeping one stable history canvas.
    content: gtk4::Box,
    virtualized_height: Rc<Cell<i32>>,
    virtualized: Rc<Cell<bool>>,
    /// Density which [`Self::virtualized_height`] currently describes. Kept
    /// beside the height so a live switch can translate an already-measured
    /// placeholder without walking the card's transcript again.
    compact: Rc<Cell<bool>>,
    pub(crate) prompt_text: String,
    /// Read-only VTE displaying the executed command line (single-row typically).
    pub(crate) command_vte: vte4::Terminal,
    /// Read-only VTE displaying captured output. A block never grows past the
    /// space its pane can show at once: longer output keeps private VTE
    /// scrollback, reachable through `output_scrollbar`.
    pub(crate) output_vte: vte4::Terminal,
    /// Per-block scrollbar bound to `output_vte`'s private adjustment. Visible
    /// only while the snapshot is taller than the block's viewport, so long
    /// output can be walked with the mouse without moving the outer history.
    pub(crate) output_scrollbar: gtk4::Scrollbar,
    /// Raw ANSI-bearing output bytes — the source for filter re-feed and the
    /// copy-output action. Mutable so filter can swap the displayed slice
    /// without losing the original.
    pub(crate) full_output: Rc<RefCell<String>>,
    /// Filtered output override. `None` renders `full_output` directly, so an
    /// ordinary finished block does not retain a second copy of a potentially
    /// huge raw ANSI log. Allocated only while a filter changes the bytes.
    pub(crate) displayed_output: Rc<RefCell<Option<String>>>,
    /// Lazy-populated ANSI-stripped view of `full_output`, used as the haystack
    /// for find-within-blocks. Avoids re-stripping on every keystroke. Cleared
    /// when `full_output` is rewritten by a filter action; otherwise kept for
    /// the lifetime of the block (finished blocks are append-once in practice).
    pub(crate) stripped_output: Rc<RefCell<Option<String>>>,
    pub(crate) cmd_text: String,
    pub(crate) copy_cmd_btn: gtk4::Button,
    pub(crate) copy_output_btn: gtk4::Button,
    pub(crate) rerun_btn: gtk4::Button,
    pub(crate) header_row: gtk4::Box,
    pub(crate) action_box: gtk4::Box,
    /// Keyboard affordances shown only while this block is selected.
    pub(crate) selection_hint: gtk4::Label,
    /// Persistent selection legend kept separate from transient refusal text.
    /// A second refusal may arrive before the first timeout; restoring from
    /// the label itself would then preserve the first refusal indefinitely.
    pub(crate) selection_hint_steady: Rc<RefCell<String>>,
    /// Invalidates older refusal timeouts after another status or selection
    /// transition takes ownership of the label.
    pub(crate) selection_feedback_generation: Rc<Cell<u64>>,
    /// Toggle the output filter while preserving the current query.
    pub(crate) toggle_filter: Rc<dyn Fn()>,
    /// Fold or unfold this card's output. Stored, like `toggle_filter`, so the
    /// pane can drive it in bulk: triaging a long session should not mean one
    /// chevron click per card.
    set_collapsed: Rc<dyn Fn(bool)>,
    /// The card's folded state. The collapsed summary's visibility is the one
    /// signal that is also correct for image-only cards, whose output VTE stays
    /// hidden while expanded.
    collapsed_summary: gtk4::Button,
    /// Hidden because the pane is narrowed to a subset of the stream.
    ///
    /// Distinct from virtualization (which hides a card's content while keeping
    /// a measured placeholder, so the document keeps its size) and from the
    /// alt-screen hand-off (which hides everything temporarily): a filtered-out
    /// card must contribute no height at all, and must stay hidden when the
    /// alt-screen app gives the viewport back.
    filtered_out: Rc<Cell<bool>>,
    /// Late-bound "hand keyboard focus back to the live prompt" action.
    /// The card is built before it knows which pane owns it, so
    /// [`FinishedBlock::connect_actions`] fills this in. Used when the filter
    /// row closes itself from the keyboard: hiding the focused entry without
    /// this would strand focus, and stranded focus is exactly where Block-only
    /// keys stop working.
    restore_live_focus: LateBoundAction,
    /// Re-fit the output to the pane's current height. See
    /// [`FinishedBlock::refit_output_to_viewport`].
    refit_output: Rc<dyn Fn() -> Option<i32>>,
    /// Cost cache for deriving wrapped rows from the displayed transcript.
    /// Kept separate from the render stamp because equal row counts do not
    /// prove that VTE already contains the right text or geometry.
    visual_rows_cache: Rc<Cell<Option<OutputVisualRowsCacheEntry>>>,
    /// What the output VTE currently holds. A find pass records this with each
    /// surface it scans; a resize, an expand or a filter changes it, and the
    /// recorded native search cursor for that surface is then meaningless.
    render_stamp: Rc<Cell<RenderStamp>>,
    /// Font scale a virtualized card has not adopted yet. Ctrl+scroll emits a
    /// notch every 0.025, and each one used to reset the font metrics of two
    /// VTEs on every retained card — including the ones virtualization had
    /// already hidden, which cannot show the result. Off-screen cards record
    /// the target instead and apply it on their way back into the viewport,
    /// before the map-time render measures anything.
    pending_font_scale: Rc<Cell<Option<f64>>>,
    displayed_generation: Rc<Cell<u64>>,
    /// Warp-style jump affordance for oversized output.
    pub(crate) jump_bottom_btn: gtk4::Button,
    pub(crate) bookmark_star: gtk4::Label,
    pub(crate) status_icon: gtk4::Label,
    /// Header chip naming an untrusted completion; hidden on healthy/background records.
    lifecycle_chip: gtk4::Label,
    /// Column count the output VTE is sized to — needed for re-feed (filter).
    pub(crate) cols: i64,
    /// Visible rows allocated to this full-height finished block.
    pub(crate) viewport_cap: i64,
    /// Whether this block exceeds the configured long-output threshold.
    pub(crate) long_output: bool,
    /// Whole-block retained-memory estimate, including its aligned BlockData.
    estimated_retained_bytes: usize,
}

impl Clone for FinishedBlock {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            is_background: self.is_background,
            widget: self.widget.clone(),
            content: self.content.clone(),
            virtualized_height: self.virtualized_height.clone(),
            virtualized: self.virtualized.clone(),
            compact: self.compact.clone(),
            prompt_text: self.prompt_text.clone(),
            command_vte: self.command_vte.clone(),
            output_vte: self.output_vte.clone(),
            output_scrollbar: self.output_scrollbar.clone(),
            cmd_text: self.cmd_text.clone(),
            full_output: self.full_output.clone(),
            displayed_output: self.displayed_output.clone(),
            stripped_output: self.stripped_output.clone(),
            copy_cmd_btn: self.copy_cmd_btn.clone(),
            copy_output_btn: self.copy_output_btn.clone(),
            rerun_btn: self.rerun_btn.clone(),
            header_row: self.header_row.clone(),
            action_box: self.action_box.clone(),
            selection_hint: self.selection_hint.clone(),
            selection_hint_steady: self.selection_hint_steady.clone(),
            selection_feedback_generation: self.selection_feedback_generation.clone(),
            toggle_filter: self.toggle_filter.clone(),
            set_collapsed: self.set_collapsed.clone(),
            collapsed_summary: self.collapsed_summary.clone(),
            filtered_out: self.filtered_out.clone(),
            restore_live_focus: self.restore_live_focus.clone(),
            refit_output: self.refit_output.clone(),
            visual_rows_cache: self.visual_rows_cache.clone(),
            render_stamp: self.render_stamp.clone(),
            pending_font_scale: self.pending_font_scale.clone(),
            displayed_generation: self.displayed_generation.clone(),
            jump_bottom_btn: self.jump_bottom_btn.clone(),
            bookmark_star: self.bookmark_star.clone(),
            status_icon: self.status_icon.clone(),
            lifecycle_chip: self.lifecycle_chip.clone(),
            cols: self.cols,
            viewport_cap: self.viewport_cap,
            long_output: self.long_output,
            estimated_retained_bytes: self.estimated_retained_bytes,
        }
    }
}

/// Lightweight shell-command syntax highlighter (Warp-style). Emits an ANSI
/// (SGR) string so it can flow through the same `set_active_output_buffer`
/// rendering path as real shell output. Best-effort, dependency-free:
///   - command name (first word, and first word after a pipe/operator): bold cyan
///   - flags (`-x`, `--long`): dim/gray
///   - quoted strings: green
///   - operators (`| & ; > <`): magenta
///   - `$VAR` references: cyan
///
/// Whitespace and all other text are emitted verbatim in the default color, so
/// the reconstructed buffer text matches the command exactly.
pub(crate) fn highlight_command_to_ansi(cmd: &str) -> String {
    const RESET: &str = "\x1b[0m";
    let chars: Vec<char> = cmd.chars().collect();
    let mut out = String::with_capacity(cmd.len() + 32);
    let mut i = 0;
    let mut expect_command = true;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            out.push(c);
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += 1;
            while i < chars.len() {
                if quote == '"' && chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                let done = chars[i] == quote;
                i += 1;
                if done {
                    break;
                }
            }
            out.push_str("\x1b[32m");
            out.extend(chars[start..i].iter());
            out.push_str(RESET);
            expect_command = false;
            continue;
        }
        if matches!(c, '|' | '&' | ';' | '>' | '<') {
            let start = i;
            while i < chars.len() && matches!(chars[i], '|' | '&' | ';' | '>' | '<') {
                i += 1;
            }
            out.push_str("\x1b[35m");
            out.extend(chars[start..i].iter());
            out.push_str(RESET);
            expect_command = true;
            continue;
        }
        let start = i;
        while i < chars.len() {
            let cc = chars[i];
            if cc.is_whitespace() || matches!(cc, '|' | '&' | ';' | '>' | '<' | '"' | '\'') {
                break;
            }
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        if word.starts_with('-') {
            out.push_str("\x1b[90m");
            out.push_str(&word);
            out.push_str(RESET);
        } else if word.starts_with('$') {
            out.push_str("\x1b[36m");
            out.push_str(&word);
            out.push_str(RESET);
        } else if expect_command {
            out.push_str("\x1b[1;36m");
            out.push_str(&word);
            out.push_str(RESET);
            expect_command = false;
        } else {
            out.push_str(&word);
        }
    }
    out
}

/// Filter raw output (ANSI preserved) to the lines matching `query`, honoring
/// regex / case / invert and `context` lines of surroundings (Warp's
/// BlockFilterQuery). Empty query, or an invalid regex, returns `full` verbatim.
fn filter_output_lines(
    full: &str,
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
    invert: bool,
    context: usize,
) -> Result<String, regex::Error> {
    if query.is_empty() {
        return Ok(full.to_string());
    }
    let re = if use_regex {
        Some(
            regex::RegexBuilder::new(query)
                .case_insensitive(!case_sensitive)
                .build()?,
        )
    } else {
        None
    };
    let ascii_query = (!case_sensitive && query.is_ascii()).then(|| {
        query
            .as_bytes()
            .iter()
            .map(|b| b.to_ascii_lowercase())
            .collect::<Vec<_>>()
    });
    let lc_query = if case_sensitive || ascii_query.is_some() {
        String::new()
    } else {
        query.to_lowercase()
    };
    let lines: Vec<&str> = full.lines().collect();
    let matches_line = |line: &str| -> bool {
        let hit = if let Some(ref re) = re {
            re.is_match(line)
        } else if case_sensitive {
            line.contains(query)
        } else if let Some(ref q) = ascii_query {
            contains_case_insensitive(line.as_bytes(), q)
        } else {
            line.to_lowercase().contains(&lc_query)
        };
        hit ^ invert
    };
    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if matches_line(line) {
            let lo = i.saturating_sub(context);
            let hi = (i + context + 1).min(lines.len());
            for slot in keep.iter_mut().take(hi).skip(lo) {
                *slot = true;
            }
        }
    }
    let mut out = String::new();
    for (line, keep) in lines.iter().zip(keep.iter()) {
        if !*keep {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    Ok(out)
}

/// Resolve the text currently shown by a finished output surface. The common
/// unfiltered state borrows `full` directly instead of retaining a duplicate
/// `String`; a filter allocates an override only when it changes the bytes.
pub(crate) fn resolved_finished_output<'a>(
    full: &'a str,
    display_override: &'a Option<String>,
) -> &'a str {
    display_override.as_deref().unwrap_or(full)
}

fn filtered_output_override(full: &str, shown: String) -> Option<String> {
    (shown != full).then_some(shown)
}

fn output_row_count(text: &str) -> i64 {
    let text = output_display_text(text);
    if text.is_empty() {
        1
    } else {
        let trailing_blank_row =
            text.ends_with('\n') || (text.ends_with('\r') && !text.ends_with("\r\n"));
        let rows = text.lines().count().max(1) as i64;
        if trailing_blank_row {
            rows + 1
        } else {
            rows
        }
    }
}

#[cfg(test)]
thread_local! {
    static OUTPUT_VISUAL_ROW_COUNT_CALLS: Cell<usize> = const { Cell::new(0) };
    static OUTPUT_VISUAL_ROWS_CACHE_HITS: Cell<usize> = const { Cell::new(0) };
    static OUTPUT_VISUAL_ROWS_CACHE_MISSES: Cell<usize> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutputVisualRowsCacheEntry {
    effective_cols: i64,
    displayed_generation: u64,
    rows: i64,
}

/// Rows occupied after VTE wraps the snapshot at `cols`. Finished cards need
/// this rather than the logical line count, otherwise long stack-trace lines
/// are still pushed into the VTE's private scrollback.
pub(crate) fn output_visual_row_count(text: &str, cols: i64) -> i64 {
    #[cfg(test)]
    OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    use unicode_width::UnicodeWidthChar;

    let cols = cols.max(1) as usize;
    // Count what the terminal leaves on screen, not the byte stream used to
    // produce it. Programs such as apt repeatedly repaint a progress row with
    // CR + EL and wrap ordinary text in SGR/OSC sequences. Counting those
    // control bytes (and every overwritten progress update) can turn a short
    // result into a false "long output" block. Long blocks are fitted to the
    // pane height, so that misclassification shows up as a large blank tail.
    // `strip_ansi` applies the horizontal cursor/erase semantics as well as
    // removing escape sequences, which makes this estimate match the VTE
    // snapshot closely enough for the short/long decision. Ordinary text is
    // byte-for-byte identical after replay, so borrow it directly and avoid an
    // equally large allocation. ESC, CR, and BS are the conservative slow-path
    // identity boundary already used by `strip_ansi_with_clear_detect`.
    let rendered;
    let text = if memchr::memchr3(0x1b, b'\r', b'\x08', text.as_bytes()).is_none() {
        text
    } else {
        rendered = strip_ansi(text);
        rendered.as_str()
    };
    let text = output_display_text(text);
    if text.is_empty() {
        return 1;
    }

    text.split('\n')
        .map(|line| {
            let mut width = 0usize;
            for ch in line.trim_end_matches('\r').chars() {
                width += match ch {
                    '\t' => 8 - (width % 8),
                    _ => UnicodeWidthChar::width(ch).unwrap_or(0),
                };
            }
            width.max(1).div_ceil(cols) as i64
        })
        .sum::<i64>()
        .max(1)
}

fn cached_output_visual_row_count(
    cache: &Cell<Option<OutputVisualRowsCacheEntry>>,
    text: &str,
    effective_cols: i64,
    displayed_generation: u64,
) -> i64 {
    if let Some(entry) = cache.get().filter(|entry| {
        entry.effective_cols == effective_cols && entry.displayed_generation == displayed_generation
    }) {
        #[cfg(test)]
        OUTPUT_VISUAL_ROWS_CACHE_HITS.with(|hits| hits.set(hits.get().saturating_add(1)));
        return entry.rows;
    }

    #[cfg(test)]
    OUTPUT_VISUAL_ROWS_CACHE_MISSES.with(|misses| misses.set(misses.get().saturating_add(1)));
    let rows = output_visual_row_count(text, effective_cols);
    cache.set(Some(OutputVisualRowsCacheEntry {
        effective_cols,
        displayed_generation,
        rows,
    }));
    rows
}

/// Start a new displayed-text generation and invalidate its derived row count.
/// Clear before wrapping so `u64::MAX -> 0` cannot alias an old generation-zero
/// entry if a future caller advances without immediately measuring.
fn advance_displayed_generation(
    generation: &Cell<u64>,
    visual_rows_cache: &Cell<Option<OutputVisualRowsCacheEntry>>,
) -> u64 {
    visual_rows_cache.set(None);
    let next = generation.get().wrapping_add(1);
    generation.set(next);
    next
}

fn output_display_text(text: &str) -> &str {
    let text = if let Some(stripped) = text.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = text.strip_prefix('\n') {
        stripped
    } else if let Some(stripped) = text.strip_prefix('\r') {
        stripped
    } else {
        text
    };

    if let Some(stripped) = text.strip_suffix("\r\n") {
        stripped
    } else if let Some(stripped) = text.strip_suffix('\n') {
        stripped
    } else if let Some(stripped) = text.strip_suffix('\r') {
        stripped
    } else {
        text
    }
}

fn line_count_text(rows: i64) -> String {
    if rows == 1 {
        "1 line".to_string()
    } else {
        format!("{rows} lines")
    }
}

/// Copy for the compact placeholder shown when a block's output is folded.
/// Keeping this as a small pure helper makes the collapsed state useful even
/// after a per-block filter changes the number of displayed rows.
fn collapsed_output_summary(rows: i64) -> String {
    format!("▸ {} hidden — click to show", line_count_text(rows))
}

/// Human duration for the header badge. Minute-plus durations keep their
/// seconds ("1m32s") — a bare "2m" can't distinguish a 61s build from a 179s
/// one, which is exactly the range users compare across runs.
pub(crate) fn format_block_duration(dur_ms: u64) -> String {
    if dur_ms < 1000 {
        format!("{dur_ms}ms")
    } else if dur_ms < 60_000 {
        format!("{:.1}s", dur_ms as f64 / 1000.0)
    } else if dur_ms < 3_600_000 {
        let m = dur_ms / 60_000;
        let s = (dur_ms % 60_000) / 1000;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s:02}s")
        }
    } else {
        let h = dur_ms / 3_600_000;
        let m = (dur_ms % 3_600_000) / 60_000;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m:02}m")
        }
    }
}

pub(crate) use jterm_core::exit_status::signal_name_for_exit;

/// Gregorian date for a count of days since 1970-01-01 (may be negative).
/// Howard Hinnant's civil-from-days; avoids pulling a chrono dependency for
/// one label.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// (label, tooltip) for the header timestamp. Blocks finished today show
/// wall-clock "HH:MM:SS"; blocks restored from earlier days get a
/// "MM-DD HH:MM" label so old history can't masquerade as fresh output. The
/// tooltip always carries the full local date-time.
pub(crate) fn format_block_timestamp(
    end_ms: u64,
    now_ms: u64,
    tz_offset_secs: i64,
) -> (String, String) {
    let local = end_ms as i64 / 1000 + tz_offset_secs;
    let day = local.div_euclid(86_400);
    let tod = local.rem_euclid(86_400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (year, month, dom) = civil_from_days(day);
    let today = (now_ms as i64 / 1000 + tz_offset_secs).div_euclid(86_400);
    let label = if day == today {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{month:02}-{dom:02} {h:02}:{m:02}")
    };
    let tooltip = format!("{year:04}-{month:02}-{dom:02} {h:02}:{m:02}:{s:02}");
    (label, tooltip)
}

/// Rows consumed by a finished block outside its output VTE: metadata header,
/// command row, and card chrome. Together with the compact live input rows this
/// leaves a long block filling the rest of the pane without growing the outer
/// document by hundreds of rows.
const FINISHED_BLOCK_NON_OUTPUT_ROWS: i64 = 3;

/// A finished block never grows the outer history beyond what its pane can show
/// at once. Anything longer keeps a bounded viewport plus its own visible
/// scrollbar, which is what makes long output workable with a mouse: the card
/// stays anchored under the pointer while the wheel or the slider walks its
/// content, instead of the whole history sliding past. Manual expansion grows
/// one block only as far as the configured expanded-row ceiling; exceptionally
/// large snapshots remain usable through the same inner scrollbar.
fn finished_output_cap(
    output_rows: i64,
    fitted_cap: i64,
    manually_expanded: bool,
    max_expanded_cap: i64,
) -> i64 {
    let output_rows = output_rows.max(1);
    if manually_expanded {
        max_expanded_cap.max(fitted_cap).max(1).min(output_rows)
    } else {
        fitted_cap.max(1).min(output_rows)
    }
}

/// True when the whole snapshot fits the block's current viewport, so the VTE
/// can take its natural height and needs no inner scrollbar.
fn output_fits_viewport(output_rows: i64, cap: i64) -> bool {
    output_rows.max(1) <= cap.max(1)
}

/// Identity of the snapshot picture that is currently on screen.
///
/// The fitted cap itself is not part of that picture. A three-row result looks
/// identical with a three-row or a twenty-four-row cap, so treating the cap as
/// identity needlessly clears and re-feeds VTE during map/resize churn. Record
/// only the columns, visible rows, whether all content fits, and text generation.
fn output_render_stamp(cols: i64, output_rows: i64, cap: i64, generation: u64) -> RenderStamp {
    (
        cols.max(1),
        output_rows.max(1).min(cap.max(1)),
        output_fits_viewport(output_rows, cap),
        generation,
    )
}

/// Test-only view of [`output_render_stamp`], so the find module can pin what
/// a re-render actually changes without duplicating the packing rule.
#[cfg(test)]
pub(crate) fn output_render_stamp_for_test(
    cols: i64,
    output_rows: i64,
    cap: i64,
    generation: u64,
) -> RenderStamp {
    output_render_stamp(cols, output_rows, cap, generation)
}

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

/// Columns a finished-block render must assume for row and height math.
///
/// A snapshot VTE's grid follows its *allocation*, not `set_size`: once the
/// pane is narrower than the block's recorded width, VTE re-wraps at the
/// allocated columns no matter what the render requested. Counting rows at the
/// recorded width then requests one height while the post-feed settle pass
/// measures another, and that disagreement is re-applied on every remap — with
/// virtualization toggling cards at the viewport boundary, the document
/// geometry ping-pongs between the two heights (the narrow-pane two-frame
/// flicker). Below the recorded width, follow the allocation; at or above it,
/// keep the recorded columns so restored output preserves its original line
/// breaks. Falls back to the recorded columns while the widget has no
/// allocation yet (first map), where the settle pass corrects any residue.
fn effective_render_cols(vte: &vte4::Terminal, recorded_cols: i64) -> i64 {
    clamp_render_cols(recorded_cols, vte.width() as i64, vte.char_width())
}

/// Pure core of [`effective_render_cols`]: clamp the recorded columns by what
/// `width_px` can hold at `cell_width_px`, keeping VTE's two-column floor.
fn clamp_render_cols(recorded_cols: i64, width_px: i64, cell_width_px: i64) -> i64 {
    let recorded = recorded_cols.max(1);
    if cell_width_px <= 0 || width_px <= 0 {
        return recorded;
    }
    recorded.min((width_px / cell_width_px).max(2))
}

fn block_edge_scroll_target(
    current: f64,
    relative_top: f64,
    block_height: f64,
    page_size: f64,
    lower: f64,
    upper: f64,
    bottom: bool,
) -> f64 {
    let max_value = (upper - page_size).max(lower);
    let absolute_top = current + relative_top;
    let target = if bottom {
        absolute_top + block_height - page_size
    } else {
        absolute_top
    };
    target.clamp(lower, max_value)
}

/// Move one adjustment by a wheel delta. Returns false when it is already at
/// the requested edge, letting a nested scroll surface hand off only there.
fn scroll_adjustment(adj: &gtk4::Adjustment, dy: f64) -> bool {
    let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
    if dy < 0.0 && adj.value() <= adj.lower() + f64::EPSILON {
        return false;
    }
    if dy > 0.0 && adj.value() >= max_value - f64::EPSILON {
        return false;
    }
    let step = adj.step_increment().max(1.0);
    let target = (adj.value() + dy * step).clamp(adj.lower(), max_value);
    adj.set_value(target);
    true
}

/// Move one adjustment by a wheel delta at VTE's native wheel speed (a tenth
/// of a page per unit, minimum one row) — `scroll_adjustment` moves by the
/// adjustment's own step, which for a VTE is a single row and feels stuck on
/// long output. Returns false at the requested edge so the caller can hand
/// the wheel off to the outer history only there.
pub(crate) fn scroll_adjustment_by_wheel(adj: &gtk4::Adjustment, dy: f64) -> bool {
    let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
    if dy < 0.0 && adj.value() <= adj.lower() + f64::EPSILON {
        return false;
    }
    if dy > 0.0 && adj.value() >= max_value - f64::EPSILON {
        return false;
    }
    let step = (adj.page_size() / 10.0).max(1.0);
    let target = (adj.value() + dy * step).clamp(adj.lower(), max_value);
    adj.set_value(target);
    true
}

pub(crate) fn forward_outer_scroll(outer: &gtk4::ScrolledWindow, dy: f64) {
    let outer_adj = outer.vadjustment();
    let step = outer_adj.step_increment().max(outer_adj.page_size() * 0.1);
    let max_value = (outer_adj.upper() - outer_adj.page_size()).max(outer_adj.lower());
    let target = (outer_adj.value() + dy * step).clamp(outer_adj.lower(), max_value);
    outer_adj.set_value(target);
}

fn schedule_block_edge_scroll(widget: &gtk4::Box, outer: &gtk4::ScrolledWindow, bottom: bool) {
    let widget = widget.downgrade();
    let outer = outer.downgrade();
    glib::idle_add_local_once(move || {
        let (Some(widget), Some(outer)) = (widget.upgrade(), outer.upgrade()) else {
            return;
        };
        let Some(bounds) = widget.compute_bounds(&outer) else {
            return;
        };
        let adj = outer.vadjustment();
        let target = block_edge_scroll_target(
            adj.value(),
            bounds.y() as f64,
            bounds.height() as f64,
            adj.page_size(),
            adj.lower(),
            adj.upper(),
            bottom,
        );
        adj.set_value(target);
    });
}

pub(crate) fn estimated_cell_height_px(config: &Config) -> i32 {
    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let base_size = parts
        .last()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(14.0);
    (base_size
        * config.default_font_scale
        * (96.0 / 72.0)
        * 1.2
        * super::alt_screen::BLOCK_CELL_HEIGHT_SCALE)
        .ceil()
        .max(1.0) as i32
}

/// Card height for its visible terminal rows at a known cell height. Every card
/// has one metadata-header row; background cards have no command row, and
/// command cards with no output hide the output VTE entirely. Keeping those
/// structural rows explicit is important for virtualization: an extra phantom
/// row makes a card at the viewport boundary alternate between its measured and
/// placeholder heights.
/// Non-terminal vertical chrome in a finished card. The ten-pixel difference
/// is exactly the density switch's imperative geometry: outer top/bottom
/// margins shrink by six pixels and header top/bottom margins by four.
const FINISHED_CARD_ROOMY_VCHROME_PX: i32 = 34;
const FINISHED_CARD_COMPACT_VCHROME_PX: i32 = 24;

const fn finished_card_vchrome_px(compact: bool) -> i32 {
    if compact {
        FINISHED_CARD_COMPACT_VCHROME_PX
    } else {
        FINISHED_CARD_ROOMY_VCHROME_PX
    }
}

fn finished_block_height_for_rows(
    cell_height_px: i32,
    command_rows: i64,
    output_rows: i64,
    compact: bool,
) -> i32 {
    let rows = 1i64
        .saturating_add(command_rows.max(0))
        .saturating_add(output_rows.max(0))
        .clamp(1, i32::MAX as i64) as i32;
    rows.saturating_mul(cell_height_px.max(1))
        .saturating_add(finished_card_vchrome_px(compact))
}

/// Virtualization metadata must follow terminal visual rows rather than logical
/// newlines. Wide glyphs and long stack-trace lines can wrap many times.
/// Values `finalize_block` already derived from the very bytes it is about to
/// hand to [`FinishedBlock::new_with_pool`].
///
/// The row count is a unicode-width walk (plus a `strip_ansi` replay when the
/// text still has escapes in it). Recomputing it inside the constructor walked
/// a 1.3 MB transcript a second time for a number the caller was already
/// holding. `None` keeps the old self-sufficient behavior for restore paths
/// and tests.
#[derive(Clone, Copy, Default)]
pub(crate) struct FinishedBlockPrecomputed {
    pub(crate) output_rows: Option<i64>,
}

pub(crate) fn estimated_finished_block_height_for_text(
    config: &Config,
    command: &str,
    output: &str,
    cols: i64,
) -> i32 {
    let output_rows = if output.trim().is_empty() {
        0
    } else {
        output_visual_row_count(output, cols).max(1)
    };
    estimated_finished_block_height_for_rows(config, command, output_rows, cols)
}

/// The same estimate from an output row count the caller already holds.
///
/// `output_visual_row_count` is a per-character unicode-width walk over the
/// whole transcript, preceded by a full `strip_ansi` whenever the text still
/// carries escapes. `finalize_block` needs that count anyway to build the card,
/// so it derives it once and threads it through here instead of paying for a
/// second identical walk on the way to the same number. `output_rows` follows
/// this function's own convention: 0 for output that trims to nothing.
pub(crate) fn estimated_finished_block_height_for_rows(
    config: &Config,
    command: &str,
    output_rows: i64,
    cols: i64,
) -> i32 {
    let command_rows = if command.trim().is_empty() {
        0
    } else {
        output_visual_row_count(command, cols).max(1)
    };
    let fallback_cap = (config.finished_block_viewport_rows as i64).max(3);
    let visible_output_rows = if output_rows == 0 {
        0
    } else {
        finished_output_cap(
            output_rows,
            fallback_cap,
            false,
            config.finished_block_max_expanded_rows as i64,
        )
    };
    finished_block_height_for_rows(
        estimated_cell_height_px(config),
        command_rows,
        visible_output_rows,
        config.block_compact,
    )
}

/// Keyboard affordances shown on the active edge of a block selection. The
/// selection synchronizer chooses the truthful capability row for the current
/// shape. Destructive Delete remains available and undoable, but is omitted
/// from this high-frequency hint; Escape remains visible for every selection.
pub(crate) const SELECTION_HINT_RUN: &str = "Esc cancel  ·  ↵ recall  ·  Ctrl+↵ run";
pub(crate) const SELECTION_HINT_RECALL: &str = "Esc cancel  ·  ↵ recall";
pub(crate) const SELECTION_HINT_REMOVE: &str = "Esc cancel";
pub(crate) const SELECTION_HINT_MIN_CHARS: i32 = 14;

/// Natural-width cap for the right-hand metadata run (timestamp, duration,
/// exit badge). Wide enough that these never ellipsize at ordinary pane
/// widths, and small enough that a narrow split makes them yield instead of
/// forcing the header past the pane's own width.
const HEADER_META_MAX_CHARS: i32 = 22;

/// Left gutter shared by a card's output rows and the summaries that stand in
/// for them.
///
/// It matches `.block-prompt-chevron`'s `margin-left`, so the chevron — the
/// card's column zero — and the output's column zero share one edge, the way a
/// prompt and its output do in a real terminal. Before this the output VTE sat
/// at zero, hard against the status stripe and left of the chevron, while the
/// collapsed summary and the image strip sat at 18px, so folding a card shifted
/// its text sideways.
///
/// It costs the output VTE these pixels, which at typical cell widths is about
/// one column. A finished card is already clamped to whatever width the pane
/// offers it (`effective_render_cols`) and already re-wraps relative to the
/// live surface, so this narrows an existing clamp rather than introducing one.
const BLOCK_GUTTER_PX: i32 = 10;

/// Late-bound action a card can be handed after construction. `None` until the
/// pane that owns the card supplies it.
type LateBoundAction = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
/// The same, held weakly: used where the holder must not keep the action (and
/// through it the card's state) alive.
type LateBoundWeakAction = Rc<RefCell<Option<std::rc::Weak<dyn Fn()>>>>;

/// Fade a card's quick-action strip in or out without changing its allocation.
///
/// The strip must keep its width in both states: the header's hexpanding
/// spacer sits immediately before the metadata run, so a strip that appears
/// and disappears drags the timestamp, duration and exit badge sideways with
/// it. Insensitive while faded so the invisible buttons take neither pointer
/// nor keyboard focus. `can_target` must move with the visual state too: an
/// opacity-zero child that remains pickable creates a transparent dead zone in
/// the card header for touch and other no-hover input.
pub(crate) fn reveal_block_actions(action_box: &gtk4::Box, revealed: bool) {
    action_box.set_opacity(if revealed { 1.0 } else { 0.0 });
    action_box.set_can_target(revealed);
    action_box.set_sensitive(revealed);
}

/// Whether a key pressed inside a card's filter entry should close the filter.
///
/// Escape is the universal dismissal, and Alt+Shift+F is the shortcut that
/// opened the row — it lives on the pane's live-VTE controller, so while the
/// entry holds focus it never reaches the pane and has to be answered here.
/// Everything else (including Ctrl chords and plain typing) belongs to the
/// entry.
fn filter_row_key_closes(keyval: gtk4::gdk::Key, modifiers: gtk4::gdk::ModifierType) -> bool {
    use gtk4::gdk::{Key, ModifierType};

    if keyval == Key::Escape {
        return !modifiers.intersects(
            ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK,
        );
    }
    matches!(keyval, Key::f | Key::F)
        && modifiers.contains(ModifierType::ALT_MASK)
        && modifiers.contains(ModifierType::SHIFT_MASK)
        && !modifiers.contains(ModifierType::CONTROL_MASK)
}

fn flash_button_icon(btn: &gtk4::Button, icon_name: &'static str, tooltip: &'static str) {
    let old_icon_name = btn.icon_name().map(|name| name.to_string());
    let old_tooltip = btn.tooltip_text().map(|s| s.to_string());
    btn.set_icon_name(icon_name);
    btn.set_tooltip_text(Some(tooltip));
    let btn_for_restore = btn.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(900), move || {
        if let Some(icon_name) = old_icon_name {
            btn_for_restore.set_icon_name(&icon_name);
        }
        btn_for_restore.set_tooltip_text(old_tooltip.as_deref());
    });
}

/// SGR reset + home + clear screen + clear scrollback, fed ahead of every
/// snapshot. `reset()` acts synchronously but `feed()` data is applied
/// asynchronously: GTK can map a card several times within one main-loop turn
/// (notebook tab switches re-map pages while virtualization toggles content
/// visibility), so a later `reset()` runs before an earlier feed has been
/// processed and the queued snapshots concatenate — output repeated once per
/// map in the burst. Clearing in-stream keeps the wipe ordered with the data
/// it must precede, making re-renders idempotent.
const FINISHED_SNAPSHOT_CLEAR: &[u8] = b"\x1b[0m\x1b[H\x1b[2J\x1b[3J";

/// The exact byte stream a finished-block render feeds: the in-stream clear
/// followed by the snapshot text (see [`FINISHED_SNAPSHOT_CLEAR`]).
fn finished_snapshot_stream(display_text: &str) -> Vec<u8> {
    let mut stream = Vec::with_capacity(FINISHED_SNAPSHOT_CLEAR.len() + display_text.len());
    stream.extend_from_slice(FINISHED_SNAPSHOT_CLEAR);
    stream.extend_from_slice(display_text.as_bytes());
    stream
}

/// Render a finished snapshot with enough temporary capture capacity for VTE's
/// real terminal semantics. The post-feed settle pass expands short/full-height
/// blocks to the actual retained buffer span, covering ANSI cursor movement,
/// carriage-return redraws, combining/wide glyphs, tabs, and soft wrapping.
/// `(effective_cols, visible_rows, fits, generation)` — see
/// [`stamp_change_needs_refeed`] for which half is content and which geometry.
/// Identity of what a card's output VTE currently holds: wrap columns, visible
/// rows, whether the whole document fits, and which displayed text generation
/// produced it. Two equal stamps mean the parsed ring and the window onto it
/// are both unchanged; any difference means a re-feed or a re-window happened,
/// which moves every match position inside that VTE.
pub(crate) type RenderStamp = (i64, i64, bool, u64);

/// The stamp a surface reports when it has no per-record snapshot to
/// invalidate. It is the value a freshly constructed card starts from, and it
/// is never produced by `output_render_stamp` (which clamps both row counts to
/// at least 1), so a real card can never accidentally compare equal to it.
pub(crate) const NEUTRAL_RENDER_STAMP: RenderStamp = (0, 0, false, 0);

/// Whether a render-stamp change means the bytes in VTE are wrong, or only the
/// window onto them.
///
/// Columns decide how the transcript wraps and the generation identifies which
/// text is displayed; those two are the content. The other two are geometry,
/// and geometry alone never invalidates a parsed ring.
fn stamp_change_needs_refeed(previous: RenderStamp, next: RenderStamp) -> bool {
    previous.0 != next.0 || previous.3 != next.3
}

/// Re-window a finished VTE that already holds the right bytes.
///
/// How many rows a card SHOWS is not a property of what was fed into it: the
/// transcript is already parsed and sitting in VTE's ring, and the visible grid
/// is a window onto that ring. `render_bytes_into_finished_vte` was being used
/// for this anyway, which reset the terminal and re-parsed the whole snapshot —
/// up to a 1.3 MB re-parse per card, per re-fit, and the re-fit sweep runs on a
/// window resize. Measured on anvil, which had the identical bug: 20 window
/// resizes over one 200k-line block cost 970ms of CPU and left the card BLANK;
/// re-windowing costs 600ms and keeps the output on screen.
///
/// The scrollback dance mirrors `render_bytes_into_finished_vte`: arm a
/// generous limit first so no `set_size` in either direction can trim the ring,
/// then settle on the real one, which keeps screen + scrollback at exactly the
/// same total the feed path left behind.
fn rewindow_finished_vte(
    vte: &vte4::Terminal,
    cols: i64,
    visible_rows: i64,
    requested_scrollback: i64,
    expand_to_buffer: bool,
    expected_tail: Option<&str>,
) {
    let (cols, visible_rows, scrollback) =
        bounded_finished_vte_geometry(cols, visible_rows.max(1), requested_scrollback.max(64));
    let cell_height = vte.char_height() as i32;
    if cell_height > 0 {
        vte.set_height_request(finished_vte_height_px(visible_rows, cell_height));
    }
    vte.set_scrollback_lines(bounded_finished_vte_max_rows(cols));
    vte.set_size(cols, visible_rows);
    vte.set_scrollback_lines(scrollback);
    // The tail gate still matters. This call feeds nothing, but an EARLIER
    // feed — the map-time render, a filter render — applies asynchronously, and
    // a re-fit can land in the same frame as it. Passing `None` makes
    // `feed_tail_applied` return true unconditionally, so the settle would
    // measure a half-parsed ring and shrink the card to the rows applied so
    // far, permanently. The bytes VTE should hold are unchanged by a
    // re-window, so the feed's own tail is still the right proof.
    if expand_to_buffer {
        settle_finished_terminal_after_feed(vte, expected_tail);
    } else {
        settle_finished_terminal_at_top(vte, expected_tail);
    }
    if let Some(adj) = vte.vadjustment() {
        adj.set_value(adj.lower());
    }
}

pub(crate) fn render_bytes_into_finished_vte(
    vte: &vte4::Terminal,
    text: &str,
    cols: i64,
    output_rows: i64,
    viewport_cap: i64,
    capture_rows: i64,
    expand_to_buffer: bool,
) {
    let display_text = output_display_text(text);
    // The pixel height request below is based on this same row count. Capping
    // the VTE grid at 32 while requesting a taller widget created the large
    // blank tail visible in long cards.
    let requested_visible_rows = output_rows.min(viewport_cap).max(1);
    let overflow_rows = output_rows
        .saturating_sub(requested_visible_rows)
        .saturating_add(64);
    let requested_scrollback = capture_rows.max(overflow_rows).max(64);
    let (cols, visible_rows, scrollback) =
        bounded_finished_vte_geometry(cols, requested_visible_rows, requested_scrollback);
    let cell_height = vte.char_height() as i32;
    if cell_height > 0 {
        vte.set_height_request(finished_vte_height_px(visible_rows, cell_height));
    }
    vte.set_scroll_on_output(false);
    vte.set_size(cols, visible_rows);
    vte.set_scrollback_lines(scrollback);
    vte.reset(true, true);
    vte.set_size(cols, visible_rows);
    vte.set_scrollback_lines(scrollback);
    vte.feed(&finished_snapshot_stream(display_text));
    let settle_tail = snapshot_settle_tail(display_text);
    if expand_to_buffer {
        settle_finished_terminal_after_feed(vte, settle_tail.as_deref());
    } else {
        // feed() settles asynchronously. Keep capped snapshots anchored at the
        // first retained row without invoking the full-height settle path.
        settle_finished_terminal_at_top(vte, settle_tail.as_deref());
    }
    if let Some(adj) = vte.vadjustment() {
        adj.set_value(adj.lower());
    }
}

/// VTE treats a bare LF as “move down, retain column”. Captured command text
/// uses ordinary logical newlines, so convert only bare LF bytes to CRLF before
/// feeding the read-only command snapshot.
fn terminalize_line_breaks(bytes: &[u8]) -> Vec<u8> {
    let extra_crs = bytes
        .iter()
        .enumerate()
        .filter(|&(i, &b)| b == b'\n' && (i == 0 || bytes[i - 1] != b'\r'))
        .count();
    if extra_crs == 0 {
        return bytes.to_vec();
    }
    let mut terminal_bytes = Vec::with_capacity(bytes.len() + extra_crs);
    for (i, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            terminal_bytes.push(b'\r');
        }
        terminal_bytes.push(byte);
    }
    terminal_bytes
}

fn finished_command_bytes(display_cmd: &str) -> Vec<u8> {
    let highlighted = match display_cmd {
        "" => b"(empty)".to_vec(),
        command => highlight_command_to_ansi(command).into_bytes(),
    };
    terminalize_line_breaks(&highlighted)
}

/// Outer margins and density class of a finished card.
///
/// Construction and the live setter share this so a pane cannot end up with
/// half its cards at one density and half at the other; the CSS side of the
/// same switch keys off the `block-compact` class set here.
fn apply_card_density(outer: &gtk4::Box, compact: bool) {
    if compact {
        outer.add_css_class("block-compact");
        outer.set_margin_top(1);
        outer.set_margin_bottom(1);
        outer.set_margin_start(4);
        outer.set_margin_end(4);
    } else {
        outer.remove_css_class("block-compact");
        outer.set_margin_top(4);
        outer.set_margin_bottom(4);
        outer.set_margin_start(8);
        outer.set_margin_end(8);
    }
}

/// Header-strip margins for the same two densities. See [`apply_card_density`].
fn apply_header_density(header_row: &gtk4::Box, compact: bool) {
    if compact {
        header_row.set_margin_start(8);
        header_row.set_margin_end(6);
        header_row.set_margin_top(3);
        header_row.set_margin_bottom(1);
    } else {
        header_row.set_margin_start(12);
        header_row.set_margin_end(8);
        header_row.set_margin_top(6);
        header_row.set_margin_bottom(2);
    }
}

fn apply_review_body_density(body: &gtk4::Widget, compact: bool) {
    let side = if compact { 8 } else { 12 };
    body.set_margin_start(side);
    body.set_margin_end(side);
    body.set_margin_top(2);
    body.set_margin_bottom(if compact { 7 } else { 11 });
}

fn apply_agent_body_density(body: &gtk4::Widget, compact: bool) {
    let side = if compact { 8 } else { 12 };
    body.set_margin_start(side);
    body.set_margin_end(side);
    body.set_margin_top(2);
    body.set_margin_bottom(if compact { 6 } else { 10 });
}

/// Update one mounted assistant notice without knowing which UI subsystem
/// created it. Correction, suggestion and Agent cards deliberately share these
/// stable CSS roles; walking only a `block-assistant` root leaves ordinary
/// finished cards and the independently-sized organism card alone.
pub(crate) fn apply_inline_assistant_density(root: &gtk4::Widget, compact: bool) -> bool {
    if !root.has_css_class("block-assistant") {
        return false;
    }
    let Ok(outer) = root.clone().downcast::<gtk4::Box>() else {
        return false;
    };
    apply_card_density(&outer, compact);

    // The hand-built suggestion and Agent-session bodies predate the shared
    // command-review roles. They are the sole non-header Box directly under
    // their respective roots.
    if outer.has_css_class("command-suggestion") || outer.has_css_class("block-agent") {
        let mut child = outer.first_child();
        while let Some(widget) = child {
            let next = widget.next_sibling();
            if !widget.has_css_class("block-header") && widget.is::<gtk4::Box>() {
                if outer.has_css_class("command-suggestion") {
                    apply_review_body_density(&widget, compact);
                } else {
                    apply_agent_body_density(&widget, compact);
                }
            }
            child = next;
        }
    }

    fn walk(widget: &gtk4::Widget, compact: bool) {
        if widget.has_css_class("block-header") || widget.has_css_class("command-review-header") {
            if let Some(header) = widget.downcast_ref::<gtk4::Box>() {
                apply_header_density(header, compact);
            }
        }
        if widget.has_css_class("command-review-body") {
            apply_review_body_density(widget, compact);
        } else if widget.has_css_class("agent-msg-body") {
            apply_agent_body_density(widget, compact);
        }

        let mut child = widget.first_child();
        while let Some(current) = child {
            let next = current.next_sibling();
            walk(&current, compact);
            child = next;
        }
    }
    walk(root, compact);
    true
}

/// Keep finished-card widgets, fixed virtualization placeholders and their
/// parallel metadata document on one density in one indexed pass.
pub(crate) fn apply_finished_card_density(
    finished: &[FinishedBlock],
    block_data: &mut VecDeque<BlockData>,
    compact: bool,
) {
    debug_assert_eq!(finished.len(), block_data.len());
    for (card, data) in finished.iter().zip(block_data.iter_mut()) {
        let height = card.set_compact(compact);
        if !card.is_filtered_out() {
            data.estimated_height = height;
        }
    }
}

impl FinishedBlock {
    fn with_cached_stripped_output<R>(
        full_output: &Rc<RefCell<String>>,
        stripped_output: &Rc<RefCell<Option<String>>>,
        f: impl FnOnce(&str) -> R,
    ) -> R {
        if stripped_output.borrow().is_none() {
            let s = strip_ansi(&full_output.borrow());
            *stripped_output.borrow_mut() = Some(s);
        }
        let guard = stripped_output.borrow();
        f(guard.as_deref().unwrap_or(""))
    }

    /// Returns the ANSI-stripped view of `full_output`, populating the cache on
    /// first call. Caller passes a closure to handle the cached string by ref to
    /// avoid an extra clone — `stripped_output` lives in a `RefCell` so we can't
    /// hand out a `Ref` that outlives the borrow.
    pub(crate) fn with_stripped_output<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        Self::with_cached_stripped_output(&self.full_output, &self.stripped_output, f)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: u64,
        prompt: &str,
        cmd: &str,
        cmd_ansi: Option<&str>,
        output: &str,
        exit_code: Option<i32>,
        config: &Config,
        duration_ms: Option<u64>,
        end_time_ms: Option<u64>,
        cwd: Option<&str>,
        cols: i64,
    ) -> Self {
        Self::new_with_pool(
            id,
            prompt,
            cmd,
            cmd_ansi,
            output,
            exit_code,
            config,
            duration_ms,
            end_time_ms,
            cwd,
            cols,
            &[],
            output.len(),
            None,
            FinishedBlockPrecomputed::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_pool(
        id: u64,
        prompt: &str,
        cmd: &str,
        cmd_ansi: Option<&str>,
        output: &str,
        exit_code: Option<i32>,
        config: &Config,
        duration_ms: Option<u64>,
        end_time_ms: Option<u64>,
        cwd: Option<&str>,
        cols: i64,
        images: &[gtk4::gdk::Texture],
        plain_output_bytes: usize,
        recycled: Option<gtk4::Box>,
        precomputed: FinishedBlockPrecomputed,
    ) -> Self {
        let is_background = cmd.trim().is_empty();
        let has_output = !output.trim().is_empty();
        let display_cmd = jterm_core::review_input::safe_multiline_display(
            cmd,
            jterm_core::review_input::MAX_REVIEW_INPUT_BYTES,
        );
        let cmd_bytes = finished_command_bytes(&display_cmd);
        let image_pixel_bytes = images.iter().fold(0usize, |total, texture| {
            let width = texture.width().max(0) as usize;
            let height = texture.height().max(0) as usize;
            total.saturating_add(width.saturating_mul(height).saturating_mul(4))
        });
        let raw_output_bytes = output.len();

        // A command that repaints in place without the alternate screen (top,
        // watch, multi-line progress) emits one frame per refresh, each behind a
        // cursor-home. Fed verbatim into the scrollback-backed output VTE those
        // frames stack into an ever-growing block. Collapse such streams to their
        // final on-screen frame — a colour-preserving snapshot with CRLF breaks —
        // so the finished block mirrors what the live VTE showed. Ordinary output
        // has no vertical repaint and is fed unchanged.
        let collapsed;
        let repaint_collapsed = output_has_vertical_repaint(output);
        let output = if repaint_collapsed {
            collapsed = collapse_repaint_output(output, cols.max(1) as usize);
            collapsed.as_str()
        } else {
            output
        };
        let estimated_retained_bytes = estimated_completed_block_retained_bytes(
            prompt.len(),
            cmd.len(),
            cmd_ansi.map_or(0, str::len),
            cmd_bytes.len().max(
                terminal_grid_units_upper_bound(&cmd_bytes, cols.max(1) as usize)
                    .min(MAX_FINISHED_VTE_GRID_CELLS),
            ),
            raw_output_bytes,
            output.len(),
            plain_output_bytes.max(
                terminal_grid_units_upper_bound(output.as_bytes(), cols.max(1) as usize)
                    .min(MAX_FINISHED_VTE_GRID_CELLS),
            ),
            cwd.map_or(0, str::len),
            image_pixel_bytes,
        )
        .saturating_add(if images.is_empty() {
            0
        } else {
            super::kitty_graphics::MAX_PENDING_BYTES_PER_BLOCK
        });

        // A collapsed transcript is a DIFFERENT string from the one the caller
        // measured — `collapse_repaint_output` clips every row at `cols` and
        // pops trailing blank rows, neither of which `strip_ansi` does — so the
        // precomputed count is only valid when no collapse happened. Measuring
        // the collapsed frame here is cheap: it is already bounded to `cols`.
        let output_rows = precomputed
            .output_rows
            .filter(|_| !repaint_collapsed)
            .unwrap_or_else(|| output_visual_row_count(output, cols));
        let fallback_viewport_cap = (config.finished_block_viewport_rows as i64).max(3);
        let viewport_cap =
            fitted_output_rows_for_viewport(None, fallback_viewport_cap, output_rows);
        let max_expanded_cap = (config.finished_block_max_expanded_rows as i64)
            .max(fallback_viewport_cap)
            .max(3);
        let current_viewport_cap = Rc::new(Cell::new(viewport_cap));
        let long_output = output_rows > viewport_cap;
        // The row count above is the only walk this constructor needs. The
        // `_for_text` estimator would repeat the same unicode-width pass over a
        // transcript that can be megabytes, right at the moment the prompt is
        // waiting to come back. Reuse the count, mapping it onto that
        // estimator's own convention: output that trims to nothing contributes
        // zero rows, anything else contributes at least one. Getting that
        // mapping wrong gives a background card a phantom row and makes a card
        // sitting on the virtualization boundary alternate heights.
        let estimator_output_rows = if output.trim().is_empty() {
            0
        } else {
            output_rows.max(1)
        };
        let virtualized_height = Rc::new(Cell::new(estimated_finished_block_height_for_rows(
            config,
            &display_cmd,
            estimator_output_rows,
            cols,
        )));
        let virtualized = Rc::new(Cell::new(false));
        let compact = Rc::new(Cell::new(config.block_compact));
        let capture_rows = output_rows
            .max(config.truncation_threshold_lines as i64)
            .max(4096);

        let outer = if let Some(reused) = recycled {
            while let Some(child) = reused.first_child() {
                reused.remove(&child);
            }
            // Filtering and alt-screen hand-off hide the outer shell itself.
            // The new card's `filtered_out` model starts false, so a matching
            // pane filter deliberately no-ops when it applies false below; make
            // the recycled GTK property agree with that fresh model first.
            reused.set_visible(true);
            reused.remove_css_class("block-hovered");
            reused.remove_css_class("block-selected");
            reused.remove_css_class("block-selection-active");
            reused.remove_css_class("block-bookmarked");
            // Every outcome stripe class, from the same list the outcome
            // itself picks from: a pooled card that kept a stale stripe would
            // wear two, and CSS would resolve that by source order rather than
            // by what this card actually did.
            for stripe in BlockOutcome::STRIPE_CSS_CLASSES {
                reused.remove_css_class(stripe);
            }
            reused.remove_css_class("block-compact");
            reused
        } else {
            let b = gtk4::Box::new(Orientation::Vertical, 0);
            b.add_css_class("block-finished");
            b
        };
        // Pooled cards must not retain expansion flags from an earlier use.
        // The output VTE owns the explicit height; the card itself never absorbs
        // spare vertical space from the document box.
        outer.set_hexpand(true);
        outer.set_vexpand(false);
        // A pooled card may have been virtualized with a fixed placeholder
        // height. New block state starts non-virtualized, so reset that GTK
        // request explicitly instead of inheriting the previous card's geometry.
        outer.set_height_request(-1);
        apply_card_density(&outer, config.block_compact);

        let content = gtk4::Box::new(Orientation::Vertical, 0);
        content.set_hexpand(true);
        content.set_vexpand(false);
        outer.append(&content);

        let outcome = BlockOutcome::classify(Some(cmd), exit_code);
        // Status stripe: green on success, red on failure, cyan for idle output,
        // amber when the shell never told us how the command ended.
        outer.add_css_class(outcome.stripe_css_class());

        // Add hover highlighting to show block is interactive (and reveal the
        // quick-action buttons). The action box is created below; it's wired into
        // these handlers after construction.
        let hover_ctrl = gtk4::EventControllerMotion::new();

        // ── Header row ──────────────────────────────────────────────────────
        let header_row = gtk4::Box::new(Orientation::Horizontal, 8);
        header_row.add_css_class("block-header");
        header_row.set_tooltip_text(Some(if is_background {
            "Click to select · Shift-click range · Ctrl+Shift-click toggle"
        } else {
            "Click to select · Shift-click range · Ctrl+Shift-click toggle · Enter recalls"
        }));
        apply_header_density(&header_row, config.block_compact);

        // Bookmark marker, hidden until the block is bookmarked. Use a normal
        // Unicode glyph so the default UI does not depend on a Nerd Font.
        let bookmark_star = gtk4::Label::new(Some("★"));
        bookmark_star.add_css_class("block-bookmark-star");
        bookmark_star.set_halign(gtk4::Align::Start);
        bookmark_star.set_visible(false);
        bookmark_star.update_property(&[gtk4::accessible::Property::Label("Bookmarked block")]);
        header_row.append(&bookmark_star);

        // Status icon: success, failure, unknown, or asynchronous/background output.
        let status_icon = gtk4::Label::new(Some(outcome.status_glyph()));
        status_icon.add_css_class(outcome.status_css_class());
        status_icon.update_property(&[gtk4::accessible::Property::Label(
            outcome.accessible_label(),
        )]);
        if outcome == BlockOutcome::Unknown {
            status_icon.set_tooltip_text(Some(UNKNOWN_EXIT_TOOLTIP));
        }
        status_icon.set_halign(gtk4::Align::Start);
        header_row.append(&status_icon);
        if is_background {
            let chip = gtk4::Label::new(Some("Background output"));
            chip.add_css_class("block-background-chip");
            chip.set_halign(gtk4::Align::Start);
            header_row.append(&chip);
        }

        // Completion provenance must be visible without hunting for a card-level
        // tooltip, which header children can shadow. The full explanation stays
        // on this dedicated, accessible chip.
        let lifecycle_chip = gtk4::Label::new(None);
        lifecycle_chip.add_css_class("block-lifecycle-chip");
        lifecycle_chip.set_halign(gtk4::Align::Start);
        lifecycle_chip.set_max_width_chars(14);
        lifecycle_chip.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        lifecycle_chip.set_visible(false);
        header_row.append(&lifecycle_chip);

        // Context chips (Warp-style): cwd pill + git-branch pill.
        if let Some(cwd_path) = cwd {
            let shortened =
                jterm_core::review_input::safe_inline_display(&shorten_path(cwd_path), 512);
            let cwd_chip = gtk4::Label::new(Some(&format!("cwd · {shortened}")));
            cwd_chip.add_css_class("block-chip");
            cwd_chip.set_halign(gtk4::Align::Start);
            cwd_chip.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
            cwd_chip.set_max_width_chars(40);
            header_row.append(&cwd_chip);

            if let Some(branch) = git_branch_for(cwd_path) {
                let git_chip = gtk4::Label::new(Some(&format!("git · {branch}")));
                git_chip.add_css_class("block-chip-git");
                git_chip.set_halign(gtk4::Align::Start);
                git_chip.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                git_chip.set_max_width_chars(28);
                header_row.append(&git_chip);
            }
        }

        // Selected blocks behave like a lightweight navigation mode. Keep the
        // available keyboard actions visible instead of making users memorize
        // them.
        //
        // Placed BEFORE the spacer on purpose: appearing here eats the
        // spacer's slack, while appearing after it would push the whole
        // timestamp/duration/exit column sideways every time the selection
        // moved.
        let selection_hint = gtk4::Label::new(None);
        selection_hint.add_css_class("block-selection-hint");
        selection_hint.set_visible(false);
        selection_hint.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        // `max_width_chars` caps the label's NATURAL width, so a value below
        // the hint's own length ellipsizes it in every pane, not just narrow
        // ones — the old 38 hid "Esc cancel" permanently. Ask for the whole
        // hint and let ellipsize handle genuinely narrow splits.
        // Multi-selection adds a short count ("12 selected") to this run.
        // Keep Escape first so a narrow split preserves the exit affordance
        // when Pango ellipsizes the trailing actions.
        selection_hint.set_width_chars(SELECTION_HINT_MIN_CHARS);
        selection_hint.set_max_width_chars(52);
        selection_hint.set_accessible_role(gtk4::AccessibleRole::Status);
        header_row.append(&selection_hint);

        // Spacer
        let spacer = gtk4::Box::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header_row.append(&spacer);

        // Timestamp label
        if let Some(et_ms) = end_time_ms {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(et_ms);
            let (label, tooltip) =
                format_block_timestamp(et_ms, now_ms, chrono_local_offset_secs());
            let ts_label = gtk4::Label::new(Some(&label));
            ts_label.add_css_class("block-header-label");
            ts_label.set_tooltip_text(Some(&tooltip));
            // Every other chip in this row ellipsizes; these did not, so in a
            // narrow split the metadata run pushed the whole header wider than
            // the pane instead of yielding. The tooltip already carries the
            // full value.
            ts_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            ts_label.set_max_width_chars(HEADER_META_MAX_CHARS);
            header_row.append(&ts_label);
        }

        // Duration badge
        if let Some(dur_ms) = duration_ms {
            let dur_label = gtk4::Label::new(Some(&format_block_duration(dur_ms)));
            dur_label.add_css_class("block-meta-badge");
            dur_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            dur_label.set_max_width_chars(HEADER_META_MAX_CHARS);
            header_row.append(&dur_label);
        }

        // Exit code badge. A successful command shows none; an unknown status
        // gets its own badge rather than silently looking like a success.
        match outcome {
            BlockOutcome::Failure(code) => {
                let badge = match signal_name_for_exit(code) {
                    Some(sig) => {
                        let badge = gtk4::Label::new(Some(&format!("exit:{code} {sig}")));
                        badge.set_tooltip_text(Some(&format!(
                            "128 + signal number: terminated by {sig}"
                        )));
                        badge
                    }
                    None => gtk4::Label::new(Some(&format!("exit:{code}"))),
                };
                badge.add_css_class("block-exit-bad");
                badge.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                badge.set_max_width_chars(HEADER_META_MAX_CHARS);
                header_row.append(&badge);
            }
            BlockOutcome::Interrupted(code) => {
                // Keep the raw code: a script that exits 130 on its own is
                // classified as interrupted here, and the number is how the
                // user tells the two apart.
                let signal = BlockOutcome::interrupt_signal(code).unwrap_or("signal");
                let badge = gtk4::Label::new(Some(&format!("exit:{code} · interrupted")));
                badge.set_tooltip_text(Some(&format!(
                    "128 + signal number: stopped by {signal}, not a command failure"
                )));
                badge.add_css_class("block-exit-interrupted");
                badge.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                badge.set_max_width_chars(HEADER_META_MAX_CHARS);
                header_row.append(&badge);
            }
            BlockOutcome::Unknown => {
                let badge = gtk4::Label::new(Some("exit:?"));
                badge.set_tooltip_text(Some(UNKNOWN_EXIT_TOOLTIP));
                badge.add_css_class("block-exit-unknown");
                badge.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                badge.set_max_width_chars(HEADER_META_MAX_CHARS);
                header_row.append(&badge);
            }
            BlockOutcome::Success | BlockOutcome::Background => {}
        }

        // Quick-action buttons, revealed on hover. The strip stays ALLOCATED at
        // all times and only fades: hiding it let the header's hexpanding
        // spacer grow, which slid the timestamp/duration/exit column sideways
        // by the strip's whole width every time the pointer crossed a card —
        // the metadata shimmered under the mouse in a way that made a long
        // history hard to read. Insensitive while faded, so an invisible
        // button can take neither a click nor Tab focus.
        let action_box = gtk4::Box::new(Orientation::Horizontal, 2);
        reveal_block_actions(&action_box, false);
        // Small gap between the meta badges (timestamp/duration/exit) on the
        // right and the action button group, so they read as separate units
        // rather than one undifferentiated cluster.
        action_box.set_margin_start(6);
        let copy_cmd_btn = gtk4::Button::from_icon_name("edit-copy-symbolic");
        copy_cmd_btn.set_tooltip_text(Some("Copy command"));
        copy_cmd_btn.update_property(&[gtk4::accessible::Property::Label("Copy command")]);
        let copy_output_btn = gtk4::Button::from_icon_name("text-x-generic-symbolic");
        copy_output_btn.set_tooltip_text(Some("Copy output"));
        copy_output_btn.update_property(&[gtk4::accessible::Property::Label("Copy output")]);
        let rerun_btn = gtk4::Button::from_icon_name("insert-text-symbolic");
        rerun_btn.set_tooltip_text(Some("Insert command at prompt"));
        rerun_btn.update_property(&[gtk4::accessible::Property::Label(
            "Insert command at prompt",
        )]);
        copy_cmd_btn.set_visible(!is_background);
        rerun_btn.set_visible(!is_background);
        let filter_btn = gtk4::Button::from_icon_name("edit-find-symbolic");
        filter_btn.set_tooltip_text(Some("Filter output"));
        filter_btn.update_property(&[gtk4::accessible::Property::Label("Filter output")]);
        let jump_bottom_btn = gtk4::Button::from_icon_name("go-bottom-symbolic");
        jump_bottom_btn.set_tooltip_text(Some("Jump to bottom of this block"));
        jump_bottom_btn.update_property(&[gtk4::accessible::Property::Label(
            "Jump to bottom of this block",
        )]);
        jump_bottom_btn.set_visible(long_output);
        // Expand button: kept for the capped-height path. Full-height finished
        // blocks hide it because their viewport already contains every row.
        let expand_btn = gtk4::Button::from_icon_name("view-fullscreen-symbolic");
        expand_btn.set_tooltip_text(Some("Expand block"));
        expand_btn.update_property(&[gtk4::accessible::Property::Label("Expand block")]);
        for btn in [
            &copy_cmd_btn,
            &copy_output_btn,
            &rerun_btn,
            &filter_btn,
            &jump_bottom_btn,
            &expand_btn,
        ] {
            btn.add_css_class("block-action-btn");
            btn.add_css_class("flat");
            action_box.append(btn);
        }
        header_row.append(&action_box);

        let outer_for_enter = outer.downgrade();
        let action_box_for_enter = action_box.downgrade();
        hover_ctrl.connect_enter(move |_, _, _| {
            let (Some(outer_for_enter), Some(action_box_for_enter)) =
                (outer_for_enter.upgrade(), action_box_for_enter.upgrade())
            else {
                return;
            };
            outer_for_enter.add_css_class("block-hovered");
            reveal_block_actions(&action_box_for_enter, true);
        });
        let outer_for_leave = outer.downgrade();
        let action_box_for_leave = action_box.downgrade();
        hover_ctrl.connect_leave(move |_| {
            let (Some(outer_for_leave), Some(action_box_for_leave)) =
                (outer_for_leave.upgrade(), action_box_for_leave.upgrade())
            else {
                return;
            };
            outer_for_leave.remove_css_class("block-hovered");
            // Only the active edge of a multi-selection owns persistent actions.
            if !outer_for_leave.has_css_class("block-selection-active") {
                reveal_block_actions(&action_box_for_leave, false);
            }
        });
        outer.add_controller(hover_ctrl);

        // Collapse toggle button
        let collapse_btn = gtk4::Button::from_icon_name("pan-down-symbolic");
        collapse_btn.add_css_class("block-collapse-btn");
        collapse_btn.add_css_class("flat");
        collapse_btn.update_property(&[gtk4::accessible::Property::Label("Hide output")]);
        header_row.append(&collapse_btn);

        content.append(&header_row);

        // ── VTE-rendered command + output ─────────────────────────────────
        // Command VTE: full-height read-only renderer for the executed command.
        let cmd_rows = cmd_bytes.iter().filter(|&&b| b == b'\n').count() as i64 + 1;
        let command_vte =
            create_finished_terminal(config, cols, cmd_rows.max(1), cmd_rows.max(1), false);
        // Defer feeds until the widget is actually mapped — VTE's internal
        // grid resize from set_size() doesn't take effect until the widget is
        // realized, so feeding immediately wraps content at a smaller default
        // width (the ls-output misalignment bug). connect_map fires once the
        // widget has been allocated, when the grid actually matches set_size.
        // One-shot: re-mapping during scroll must not re-feed.
        {
            let cmd_bytes_for_map = cmd_bytes.clone();
            let cols_for_map = cols.max(1);
            let cmd_rows_for_map = cmd_rows.max(1);
            let fed = Cell::new(false);
            command_vte.connect_map(move |w| {
                if fed.get() {
                    return;
                }
                fed.set(true);
                // A pane narrower than the recorded width wraps the command
                // onto more rows; size the grid and the pixel request for the
                // wrapped count or the settle pass fights the allocation.
                let eff_cols = effective_render_cols(w, cols_for_map);
                let requested_cmd_rows = if eff_cols < cols_for_map {
                    output_visual_row_count(&String::from_utf8_lossy(&cmd_bytes_for_map), eff_cols)
                        .max(cmd_rows_for_map)
                } else {
                    cmd_rows_for_map
                };
                let (eff_cols, cmd_rows_for_map, _) =
                    bounded_finished_vte_geometry(eff_cols, requested_cmd_rows, 0);
                w.set_size(eff_cols, cmd_rows_for_map);
                w.set_scrollback_lines(0);
                w.feed(&cmd_bytes_for_map);
                let tail = snapshot_settle_tail(&String::from_utf8_lossy(&cmd_bytes_for_map));
                settle_finished_terminal_after_feed(w, tail.as_deref());
                let ch = w.char_height() as i32;
                if ch > 0 {
                    w.set_height_request(finished_vte_height_px(cmd_rows_for_map, ch));
                }
            });
        }

        // Output taller than the pane keeps a bounded viewport and scrolls
        // inside its own card; wheel events forward to the outer history only
        // once the inner buffer reaches an edge.
        let full_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let displayed_output: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let output_vte = create_finished_terminal(config, cols, output_rows, viewport_cap, false);
        let (_, initial_visible_rows, _) =
            bounded_finished_vte_geometry(cols, output_rows.min(viewport_cap).max(1), 0);
        output_vte.set_height_request(finished_vte_height_px(
            initial_visible_rows,
            estimated_cell_height_px(config),
        ));
        // Tracks whether the user has toggled this block to its complete height.
        // The default cap is recomputed whenever virtualization remaps the card.
        let expanded: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Identity of the snapshot most recently fed into the output VTE.
        // Virtualization
        // only hides a card's content — the VTE keeps its buffer while
        // unmapped — so a remap whose geometry is unchanged must not re-feed:
        // every feed re-requests the estimated height and re-runs the async
        // settle pass, and that transient height flip moves the outer
        // document, re-clamps the scroll, and re-toggles boundary cards —
        // the self-sustaining flicker loop on narrow panes. Cols start at 0
        // (below any real value) so the first map always renders.
        let render_stamp: Rc<Cell<RenderStamp>> = Rc::new(Cell::new(NEUTRAL_RENDER_STAMP));
        let pending_font_scale: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
        {
            // Virtualization hides `content`, so its map is exactly the moment a
            // card comes back into the viewport. GTK maps a parent before its
            // children, so the scale is in place before the output VTE's own
            // map handler measures `char_height` and sizes the card.
            let pending = pending_font_scale.clone();
            let command_vte_for_font = command_vte.downgrade();
            let output_vte_for_font = output_vte.downgrade();
            content.connect_map(move |_| {
                let Some(scale) = pending.take() else {
                    return;
                };
                if let Some(vte) = command_vte_for_font.upgrade() {
                    vte.set_font_scale(scale);
                }
                if let Some(vte) = output_vte_for_font.upgrade() {
                    vte.set_font_scale(scale);
                }
            });
        }
        // Bumped whenever `displayed_output` is replaced (per-block filter), so
        // a stale stamp can never suppress rendering fresh text.
        let displayed_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        // Construction already scanned the initial transcript. Seed the cache
        // so first map at the recorded width, and later same-width remaps, can
        // reuse those wrapped rows.
        let visual_rows_cache = Rc::new(Cell::new(Some(OutputVisualRowsCacheEntry {
            effective_cols: cols.max(1),
            displayed_generation: 0,
            rows: output_rows,
        })));
        {
            let cols_for_map = cols.max(1);
            let fallback_cap_for_map = viewport_cap;
            let current_cap_for_map = current_viewport_cap.clone();
            let full_for_map = full_output.clone();
            let displayed_for_map = displayed_output.clone();
            let expanded_for_map = expanded.clone();
            let expand_btn_for_map = expand_btn.downgrade();
            let jump_btn_for_map = jump_bottom_btn.downgrade();
            let stamp_for_map = render_stamp.clone();
            let generation_for_map = displayed_generation.clone();
            let visual_rows_cache_for_map = visual_rows_cache.clone();
            let max_expanded_cap_for_map = max_expanded_cap;
            output_vte.connect_map(move |w| {
                let (Some(expand_btn_for_map), Some(jump_btn_for_map)) =
                    (expand_btn_for_map.upgrade(), jump_btn_for_map.upgrade())
                else {
                    return;
                };
                let full = full_for_map.borrow();
                let displayed = displayed_for_map.borrow();
                let text = resolved_finished_output(full.as_str(), &displayed);
                let eff_cols = effective_render_cols(w, cols_for_map);
                let rows = cached_output_visual_row_count(
                    &visual_rows_cache_for_map,
                    text,
                    eff_cols,
                    generation_for_map.get(),
                );
                let fitted_cap = fitted_output_rows_for_widget(w, fallback_cap_for_map, rows);
                current_cap_for_map.set(fitted_cap);
                let manually_expanded = expanded_for_map.get();
                let cap = finished_output_cap(
                    rows,
                    fitted_cap,
                    manually_expanded,
                    max_expanded_cap_for_map,
                );
                let stamp = output_render_stamp(eff_cols, rows, cap, generation_for_map.get());
                if stamp_for_map.replace(stamp) == stamp {
                    return;
                }
                let (_, visible_rows, _) =
                    bounded_finished_vte_geometry(eff_cols, rows.min(cap).max(1), 0);
                let fit_to_content = output_fits_viewport(rows, cap);
                let can_expand = rows > fitted_cap;
                expand_btn_for_map.set_visible(can_expand);
                jump_btn_for_map.set_visible(can_expand);
                render_bytes_into_finished_vte(
                    w,
                    text,
                    eff_cols,
                    rows,
                    cap,
                    capture_rows,
                    fit_to_content,
                );
                // Capped snapshots keep grid and pixel request identical.
                // Full-document snapshots let the post-feed VTE measurement
                // set both values, including shrinking an overestimate.
                if !fit_to_content {
                    let ch = w.char_height() as i32;
                    if ch > 0 {
                        w.set_height_request(finished_vte_height_px(visible_rows, ch));
                    }
                }
            });
        }

        // Geometry is finalized on map, so install the handler for every
        // block and let the map callback decide whether expansion is useful.
        expand_btn.set_visible(long_output);
        {
            let expand_for_btn = expanded.clone();
            let output_vte_for_btn = output_vte.downgrade();
            let full_for_btn = full_output.clone();
            let displayed_for_btn = displayed_output.clone();
            let current_cap_for_btn = current_viewport_cap.clone();
            let cols_for_btn = cols.max(1);
            let stamp_for_btn = render_stamp.clone();
            let generation_for_btn = displayed_generation.clone();
            let visual_rows_cache_for_btn = visual_rows_cache.clone();
            let max_expanded_cap_for_btn = max_expanded_cap;
            expand_btn.connect_clicked(move |btn| {
                let Some(output_vte_for_btn) = output_vte_for_btn.upgrade() else {
                    return;
                };
                let now_expanded = !expand_for_btn.get();
                expand_for_btn.set(now_expanded);
                let full = full_for_btn.borrow();
                let displayed = displayed_for_btn.borrow();
                let text = resolved_finished_output(full.as_str(), &displayed);
                let eff_cols = effective_render_cols(&output_vte_for_btn, cols_for_btn);
                let rows = cached_output_visual_row_count(
                    &visual_rows_cache_for_btn,
                    text,
                    eff_cols,
                    generation_for_btn.get(),
                );
                let fitted_cap = fitted_output_rows_for_widget(
                    &output_vte_for_btn,
                    current_cap_for_btn.get(),
                    rows,
                );
                current_cap_for_btn.set(fitted_cap);
                let cap =
                    finished_output_cap(rows, fitted_cap, now_expanded, max_expanded_cap_for_btn);
                stamp_for_btn.set(output_render_stamp(
                    eff_cols,
                    rows,
                    cap,
                    generation_for_btn.get(),
                ));
                let (_, visible_rows, _) =
                    bounded_finished_vte_geometry(eff_cols, rows.min(cap).max(1), 0);
                let fit_to_content = output_fits_viewport(rows, cap);
                render_bytes_into_finished_vte(
                    &output_vte_for_btn,
                    text,
                    eff_cols,
                    rows,
                    cap,
                    capture_rows,
                    fit_to_content,
                );
                if !fit_to_content {
                    let ch = output_vte_for_btn.char_height() as i32;
                    if ch > 0 {
                        output_vte_for_btn
                            .set_height_request(finished_vte_height_px(visible_rows, ch));
                    }
                }
                btn.set_icon_name(if now_expanded {
                    "view-restore-symbolic"
                } else {
                    "view-fullscreen-symbolic"
                });
                btn.set_tooltip_text(Some(if now_expanded {
                    "Collapse to viewport height"
                } else {
                    "Expand block"
                }));
                btn.update_property(&[gtk4::accessible::Property::Label(if now_expanded {
                    "Collapse to viewport height"
                } else {
                    "Expand block"
                })]);
            });
        }

        // `connect_map` fits a block only as it re-enters the viewport, so a
        // window or split resize would leave every card that never unmapped
        // sized to the old geometry — a shrunk pane keeps oversized blocks, a
        // grown one keeps needlessly short ones. This re-runs the same fit for
        // cards that are already on screen. It reports the card's new height so
        // the caller can keep virtualization metadata in step, and `None` when
        // the pane's geometry left this block's cap unchanged.
        let refit_output: Rc<dyn Fn() -> Option<i32>> = {
            let output_vte = output_vte.downgrade();
            let full_for_refit = full_output.clone();
            let displayed_for_refit = displayed_output.clone();
            let current_cap_for_refit = current_viewport_cap.clone();
            let expanded_for_refit = expanded.clone();
            let expand_btn_for_refit = expand_btn.downgrade();
            let jump_btn_for_refit = jump_bottom_btn.downgrade();
            let cols_for_refit = cols.max(1);
            let cmd_for_refit = cmd.to_string();
            let stamp_for_refit = render_stamp.clone();
            let generation_for_refit = displayed_generation.clone();
            let visual_rows_cache_for_refit = visual_rows_cache.clone();
            let compact_for_refit = compact.clone();
            let max_expanded_cap_for_refit = max_expanded_cap;
            Rc::new(move || {
                let (Some(output_vte), Some(expand_btn), Some(jump_btn)) = (
                    output_vte.upgrade(),
                    expand_btn_for_refit.upgrade(),
                    jump_btn_for_refit.upgrade(),
                ) else {
                    return None;
                };
                // Virtualized cards are unmapped; their `connect_map` handler
                // fits them against the current pane when they come back.
                if !output_vte.is_mapped() {
                    return None;
                }
                let full = full_for_refit.borrow();
                let displayed = displayed_for_refit.borrow();
                let text = resolved_finished_output(full.as_str(), &displayed);
                let eff_cols = effective_render_cols(&output_vte, cols_for_refit);
                let rows = cached_output_visual_row_count(
                    &visual_rows_cache_for_refit,
                    text,
                    eff_cols,
                    generation_for_refit.get(),
                );
                let fitted_cap =
                    fitted_output_rows_for_widget(&output_vte, current_cap_for_refit.get(), rows);
                current_cap_for_refit.set(fitted_cap);
                // Pane sizing is authoritative over a manual expansion: a block
                // expanded for the old geometry must not outlive it.
                if expanded_for_refit.replace(false) {
                    expand_btn.set_icon_name("view-fullscreen-symbolic");
                    expand_btn.set_tooltip_text(Some("Expand block"));
                    expand_btn
                        .update_property(&[gtk4::accessible::Property::Label("Expand block")]);
                }
                let can_expand = rows > fitted_cap;
                expand_btn.set_visible(can_expand);
                jump_btn.set_visible(can_expand);
                let cap = finished_output_cap(rows, fitted_cap, false, max_expanded_cap_for_refit);
                let stamp = output_render_stamp(eff_cols, rows, cap, generation_for_refit.get());
                let previous_stamp = stamp_for_refit.replace(stamp);
                if previous_stamp == stamp {
                    return None;
                }
                let (_, visible_rows, _) =
                    bounded_finished_vte_geometry(eff_cols, rows.min(cap).max(1), 0);
                let fit_to_content = output_fits_viewport(rows, cap);
                // Same columns and same displayed generation means VTE already
                // holds exactly these bytes wrapped exactly this way; only the
                // number of rows on screen moved.
                if stamp_change_needs_refeed(previous_stamp, stamp) {
                    render_bytes_into_finished_vte(
                        &output_vte,
                        text,
                        eff_cols,
                        rows,
                        cap,
                        capture_rows,
                        fit_to_content,
                    );
                } else {
                    let settle_tail = snapshot_settle_tail(output_display_text(text));
                    rewindow_finished_vte(
                        &output_vte,
                        eff_cols,
                        rows.min(cap).max(1),
                        capture_rows.max(rows),
                        fit_to_content,
                        settle_tail.as_deref(),
                    );
                }
                let cell_height = (output_vte.char_height() as i32).max(1);
                if !fit_to_content {
                    output_vte
                        .set_height_request(finished_vte_height_px(visible_rows, cell_height));
                }
                let command_rows = if is_background {
                    0
                } else {
                    output_visual_row_count(&cmd_for_refit, eff_cols).max(1)
                };
                Some(finished_block_height_for_rows(
                    cell_height,
                    command_rows,
                    if has_output { visible_rows } else { 0 },
                    compact_for_refit.get(),
                ))
            })
        };

        // Command row: Warp-style accent prompt chevron + the command VTE.
        let cmd_row = gtk4::Box::new(Orientation::Horizontal, 0);
        let chevron = gtk4::Label::new(Some("\u{276f}")); // ❯
        chevron.add_css_class("block-prompt-chevron");
        chevron.set_valign(gtk4::Align::Start);
        cmd_row.append(&chevron);
        cmd_row.append(&command_vte);

        content.append(&cmd_row);
        cmd_row.set_visible(!is_background);
        // Always use a read-only VTE, including short output. The previous Label
        // fast path stripped ANSI SGR bytes, so `ls` and `git status` lost the
        // colors users see in regular VTE mode.
        let output_box = gtk4::Box::new(Orientation::Horizontal, 0);
        output_box.set_hexpand(true);
        output_box.set_margin_start(BLOCK_GUTTER_PX);
        output_box.append(&output_vte);
        let output_scrollbar =
            gtk4::Scrollbar::new(Orientation::Vertical, output_vte.vadjustment().as_ref());
        output_scrollbar.add_css_class("block-output-scrollbar");
        output_scrollbar.set_tooltip_text(Some("Scroll within this block"));
        output_scrollbar.set_visible(false);
        output_box.append(&output_scrollbar);
        // Drive visibility from the adjustment itself rather than from each
        // sizing site. VTE applies `feed()` asynchronously and re-measures the
        // block on map, expand, filter, and theme changes; the adjustment is the
        // one place that always knows whether content overflows the viewport.
        if let Some(adj) = output_vte.vadjustment() {
            let scrollbar = output_scrollbar.downgrade();
            let sync_visibility = move |adj: &gtk4::Adjustment| {
                let Some(scrollbar) = scrollbar.upgrade() else {
                    return;
                };
                let overflows = adj.upper() - adj.lower() > adj.page_size() + f64::EPSILON;
                scrollbar.set_visible(overflows);
            };
            sync_visibility(&adj);
            adj.connect_changed(sync_visibility);
        }
        let output_widget: gtk4::Widget = output_box.clone().upcast::<gtk4::Widget>();
        content.append(&output_box);

        // Kitty graphics (anvil parity): append each decoded texture as a
        // Picture under the text output. Pictures preserve aspect ratio inside
        // a max-height bound so a tall plot doesn't push the next block
        // off-screen; one shared box lets the collapse chevron hide them
        // together with the text output.
        let images_box: Option<gtk4::Box> = if images.is_empty() {
            None
        } else {
            let ib = gtk4::Box::new(Orientation::Vertical, 4);
            ib.add_css_class("block-images");
            ib.set_margin_start(BLOCK_GUTTER_PX);
            ib.set_margin_end(8);
            ib.set_margin_bottom(4);
            for tex in images {
                let pic = gtk4::Picture::for_paintable(tex);
                pic.set_can_shrink(true);
                pic.set_content_fit(gtk4::ContentFit::Contain);
                pic.set_halign(gtk4::Align::Start);
                // Cap displayed height so plots/screenshots stay within ~25
                // rows of block real estate; the outer history scrolls past.
                pic.set_size_request(-1, tex.height().clamp(64, 600));
                ib.append(&pic);
            }
            content.append(&ib);
            Some(ib)
        };

        // Folding used to leave only a tiny chevron in the header. That made a
        // collapsed block look like it had no output at all, especially once it
        // had scrolled away from the pointer. Keep a compact, keyboard-focusable
        // summary in the document instead; it both preserves the output's scale
        // and is a large, obvious target to restore it.
        let collapsed_summary = gtk4::Button::with_label(&collapsed_output_summary(output_rows));
        collapsed_summary.add_css_class("block-output-summary");
        collapsed_summary.add_css_class("flat");
        collapsed_summary.set_halign(gtk4::Align::Start);
        collapsed_summary.set_margin_start(BLOCK_GUTTER_PX);
        collapsed_summary.set_margin_end(8);
        collapsed_summary.set_margin_bottom(4);
        collapsed_summary.set_tooltip_text(Some("Show block output"));
        collapsed_summary.set_visible(false);
        content.append(&collapsed_summary);

        // Ctrl+click on a URL inside the output VTE → open in browser.
        // VTE's `match_add_regex` (registered in create_finished_terminal) makes
        // `check_match_at` return the matching URL at the pointer position;
        // VTE handles word/line double/triple-click selection natively.
        {
            let click = gtk4::GestureClick::new();
            click.set_button(1);
            let vte_for_click = output_vte.downgrade();
            click.connect_pressed(move |controller, n_press, x, y| {
                if n_press != 1 {
                    return;
                }
                let Some(vte_for_click) = vte_for_click.upgrade() else {
                    return;
                };
                let state = controller.current_event_state();
                if !state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                    return;
                }
                let (uri, _tag) = vte_for_click.check_match_at(x, y);
                if let Some(uri) = uri {
                    let s = uri.to_string();
                    if !s.is_empty() {
                        open_uri(&s);
                        controller.set_state(gtk4::EventSequenceState::Claimed);
                    }
                }
            });
            output_vte.add_controller(click);
        }

        let has_images = images_box.is_some();
        // Output-only controls are noise for commands such as `cd`,
        // `mkdir`, and successful redirects. Image-only commands (`kitten
        // icat`) still keep the collapse chevron so their Pictures fold away.
        copy_output_btn.set_visible(has_output);
        filter_btn.set_visible(has_output);
        collapse_btn.set_visible(has_output || has_images);
        if !has_output {
            output_widget.set_visible(false);
        } else {
            collapse_btn.set_tooltip_text(Some(&format!(
                "Toggle output ({})",
                line_count_text(output_rows)
            )));
        }
        // Header chevron and the inline summary share one folded-state update,
        // so either target consistently restores the same output surface.
        let set_collapsed: Rc<dyn Fn(bool)> = {
            let output_widget = output_widget.downgrade();
            let collapsed_summary = collapsed_summary.downgrade();
            let collapse_btn = collapse_btn.downgrade();
            let images_box = images_box.as_ref().map(|ib| ib.downgrade());
            Rc::new(move |collapsed| {
                let (Some(output_widget), Some(collapsed_summary), Some(collapse_btn)) = (
                    output_widget.upgrade(),
                    collapsed_summary.upgrade(),
                    collapse_btn.upgrade(),
                ) else {
                    return;
                };
                // Image-only blocks keep their empty output VTE hidden even
                // while expanded; only the Pictures fold and unfold.
                output_widget.set_visible(!collapsed && has_output);
                if let Some(ib) = images_box.as_ref().and_then(|ib| ib.upgrade()) {
                    ib.set_visible(!collapsed);
                }
                collapsed_summary.set_visible(collapsed);
                collapse_btn.set_icon_name(if collapsed {
                    "pan-end-symbolic"
                } else {
                    "pan-down-symbolic"
                });
                collapse_btn.set_tooltip_text(Some(if collapsed {
                    "Show output"
                } else {
                    "Hide output"
                }));
                collapse_btn.update_property(&[gtk4::accessible::Property::Label(if collapsed {
                    "Show output"
                } else {
                    "Hide output"
                })]);
            })
        };
        {
            let set_collapsed = set_collapsed.clone();
            // The summary's visibility is the one folded-state signal that
            // also works for image-only blocks, whose output VTE stays hidden
            // even while expanded.
            let collapsed_summary = collapsed_summary.downgrade();
            collapse_btn.connect_clicked(move |_| {
                if let Some(collapsed_summary) = collapsed_summary.upgrade() {
                    set_collapsed(!collapsed_summary.is_visible());
                }
            });
        }
        {
            let set_collapsed = set_collapsed.clone();
            collapsed_summary.connect_clicked(move |_| set_collapsed(false));
        }

        // Per-block output filter (Warp's BlockFilterQuery): the funnel button in
        // the action box toggles a compact row that narrows the output to lines
        // matching the query, honoring regex / case / invert / context-lines.
        let filter_enabled = Rc::new(Cell::new(false));
        // Late-bound by `connect_actions`; see the field's documentation.
        let restore_live_focus: LateBoundAction = Rc::new(RefCell::new(None));
        // The filter row's own key controller needs the toggle that has not
        // been built yet. Weak, so the entry's controller cannot keep the
        // toggle (and through it the card's state) alive after eviction.
        let filter_toggle_handle: LateBoundWeakAction = Rc::new(RefCell::new(None));
        let toggle_filter = {
            // The filter editor is built on the FIRST toggle, not at card
            // construction. A search entry, three toggles, a spin button and a
            // status label are ~15-20 GtkWidgets per card — a third of the
            // whole history's widget population at the default block cap —
            // and they existed solely so a keystroke could make them visible.
            // Everything the builder needs is captured weakly (widgets) or by
            // Rc (state), exactly as the eager version captured it, so the
            // toggle closure the filter button owns still cannot keep the card
            // alive after eviction.
            type FilterRowHandles = (
                glib::WeakRef<gtk4::Box>,
                glib::WeakRef<gtk4::SearchEntry>,
                Rc<dyn Fn()>,
            );
            type FilterRowBuilder = dyn Fn(&gtk4::Box, &gtk4::Box) -> Option<FilterRowHandles>;
            let output_vte = output_vte.downgrade();
            let expand_btn = expand_btn.downgrade();
            let filter_btn_weak = filter_btn.downgrade();
            let jump_bottom_btn = jump_bottom_btn.downgrade();
            let collapsed_summary = collapsed_summary.downgrade();
            let restore_live_focus_for_row = restore_live_focus.clone();
            let filter_toggle_for_row = filter_toggle_handle.clone();
            let build_filter_row: Rc<FilterRowBuilder> = {
                let filter_btn = filter_btn_weak.clone();
                let full_output = full_output.clone();
                let displayed_output = displayed_output.clone();
                let expanded = expanded.clone();
                let current_viewport_cap = current_viewport_cap.clone();
                let filter_enabled = filter_enabled.clone();
                let render_stamp = render_stamp.clone();
                let displayed_generation = displayed_generation.clone();
                let visual_rows_cache = visual_rows_cache.clone();
                Rc::new(move |content: &gtk4::Box, cmd_row: &gtk4::Box| {
                    let filter_row = gtk4::Box::new(Orientation::Horizontal, 4);
                    filter_row.add_css_class("block-filter-row");
                    filter_row.set_visible(false);
                    filter_row.set_margin_start(12);
                    filter_row.set_margin_end(8);
                    filter_row.set_margin_top(2);
                    filter_row.set_margin_bottom(2);

                    let filter_entry = gtk4::SearchEntry::new();
                    filter_entry.set_placeholder_text(Some("Filter output…"));
                    filter_entry.set_hexpand(true);
                    let regex_tg = gtk4::ToggleButton::with_label(".*");
                    regex_tg.set_tooltip_text(Some("Regular expression"));
                    let case_tg = gtk4::ToggleButton::with_label("Aa");
                    case_tg.set_tooltip_text(Some("Case sensitive"));
                    let invert_tg = gtk4::ToggleButton::with_label("!");
                    invert_tg.set_tooltip_text(Some("Invert match (hide matching lines)"));
                    let ctx_spin = gtk4::SpinButton::with_range(0.0, 9.0, 1.0);
                    ctx_spin.set_tooltip_text(Some("Lines of context around each match"));
                    ctx_spin.set_value(0.0);
                    let filter_status = gtk4::Label::new(None);
                    filter_status.add_css_class("block-filter-status");
                    filter_status.set_halign(gtk4::Align::Start);
                    for w in [&regex_tg, &case_tg, &invert_tg] {
                        w.add_css_class("flat");
                        w.add_css_class("block-filter-toggle");
                    }
                    // The filter row grabs focus when it opens, and until now
                    // nothing in it answered a key: the only way out was the
                    // header button, so a keyboard user who opened the filter
                    // was stuck inside it. Escape closes it — and so does the
                    // same Alt+Shift+F that opened it, which otherwise reaches
                    // the pane controller only while the live VTE has focus.
                    // Closing keeps the query text (re-opening restores it) and
                    // hands focus back to the prompt rather than stranding it
                    // on a hidden entry.
                    {
                        let toggle_handle = filter_toggle_for_row.clone();
                        let restore_focus = restore_live_focus_for_row.clone();
                        let keys = gtk4::EventControllerKey::new();
                        keys.connect_key_pressed(move |_, keyval, _, state| {
                            if !filter_row_key_closes(keyval, state) {
                                return glib::Propagation::Proceed;
                            }
                            let toggle = toggle_handle
                                .borrow()
                                .as_ref()
                                .and_then(std::rc::Weak::upgrade);
                            let Some(toggle) = toggle else {
                                return glib::Propagation::Proceed;
                            };
                            toggle();
                            if let Some(restore) = restore_focus.borrow().as_ref() {
                                restore();
                            }
                            glib::Propagation::Stop
                        });
                        filter_entry.add_controller(keys);
                    }
                    filter_row.append(&filter_entry);
                    filter_row.append(&regex_tg);
                    filter_row.append(&case_tg);
                    filter_row.append(&invert_tg);
                    filter_row.append(&ctx_spin);
                    filter_row.append(&filter_status);

                    content.append(&filter_row);
                    content.reorder_child_after(&filter_row, Some(cmd_row));

                    let apply = {
                        let output_vte = output_vte.clone();
                        let full_output = full_output.clone();
                        let displayed_output = displayed_output.clone();
                        let filter_enabled = filter_enabled.clone();
                        let filter_entry = filter_entry.downgrade();
                        let regex_tg = regex_tg.downgrade();
                        let case_tg = case_tg.downgrade();
                        let invert_tg = invert_tg.downgrade();
                        let ctx_spin = ctx_spin.downgrade();
                        let filter_status = filter_status.downgrade();
                        let expand_btn = expand_btn.clone();
                        let expanded = expanded.clone();
                        let current_viewport_cap = current_viewport_cap.clone();
                        let render_stamp = render_stamp.clone();
                        let displayed_generation = displayed_generation.clone();
                        let visual_rows_cache = visual_rows_cache.clone();
                        let filter_btn = filter_btn.clone();
                        let jump_bottom_btn = jump_bottom_btn.clone();
                        let collapsed_summary = collapsed_summary.clone();
                        let max_expanded_cap_for_filter = max_expanded_cap;
                        move || {
                            let (
                                Some(output_vte),
                                Some(filter_entry),
                                Some(regex_tg),
                                Some(case_tg),
                                Some(invert_tg),
                                Some(ctx_spin),
                                Some(filter_status),
                                Some(expand_btn),
                                Some(filter_btn),
                                Some(jump_bottom_btn),
                                Some(collapsed_summary),
                            ) = (
                                output_vte.upgrade(),
                                filter_entry.upgrade(),
                                regex_tg.upgrade(),
                                case_tg.upgrade(),
                                invert_tg.upgrade(),
                                ctx_spin.upgrade(),
                                filter_status.upgrade(),
                                expand_btn.upgrade(),
                                filter_btn.upgrade(),
                                jump_bottom_btn.upgrade(),
                                collapsed_summary.upgrade(),
                            )
                            else {
                                return;
                            };
                            let q = filter_entry.text().to_string();
                            let full = full_output.borrow();
                            let full_rows = output_row_count(&full);
                            let (display_override, invalid_regex) =
                                if !filter_enabled.get() || q.is_empty() {
                                    (None, false)
                                } else {
                                    match filter_output_lines(
                                        full.as_str(),
                                        &q,
                                        regex_tg.is_active(),
                                        case_tg.is_active(),
                                        invert_tg.is_active(),
                                        ctx_spin.value() as usize,
                                    ) {
                                        Ok(shown) => {
                                            (filtered_output_override(full.as_str(), shown), false)
                                        }
                                        Err(_) => (None, true),
                                    }
                                };
                            let shown = resolved_finished_output(full.as_str(), &display_override);
                            let shown_rows = output_row_count(shown);
                            let eff_cols = effective_render_cols(&output_vte, cols);
                            let display_changed =
                                displayed_output.borrow().as_deref() != display_override.as_deref();
                            // New displayed text gets a generation of its own, so a
                            // same-width remap reuses only rows derived from these bytes.
                            // Re-applying identical state keeps the cached row count and
                            // render stamp intact instead of re-feeding the full snapshot.
                            let generation = if display_changed {
                                advance_displayed_generation(
                                    &displayed_generation,
                                    &visual_rows_cache,
                                )
                            } else {
                                displayed_generation.get()
                            };
                            let shown_visual_rows = cached_output_visual_row_count(
                                &visual_rows_cache,
                                shown,
                                eff_cols,
                                generation,
                            );
                            let fitted_cap = fitted_output_rows_for_widget(
                                &output_vte,
                                current_viewport_cap.get(),
                                shown_visual_rows,
                            );
                            current_viewport_cap.set(fitted_cap);
                            let can_expand = shown_visual_rows > fitted_cap;
                            // A narrow filter result must not leave the block logically
                            // expanded; clearing the query should return to its default mode.
                            if !can_expand && expanded.replace(false) {
                                expand_btn.set_icon_name("view-fullscreen-symbolic");
                                expand_btn.set_tooltip_text(Some("Expand block"));
                                expand_btn.update_property(&[gtk4::accessible::Property::Label(
                                    "Expand block",
                                )]);
                            }
                            let manually_expanded = expanded.get();
                            let active_cap = finished_output_cap(
                                shown_visual_rows,
                                fitted_cap,
                                manually_expanded,
                                max_expanded_cap_for_filter,
                            );
                            let stamp = output_render_stamp(
                                eff_cols,
                                shown_visual_rows,
                                active_cap,
                                generation,
                            );
                            let fit_to_content =
                                output_fits_viewport(shown_visual_rows, active_cap);
                            if render_stamp.replace(stamp) != stamp {
                                render_bytes_into_finished_vte(
                                    &output_vte,
                                    shown,
                                    eff_cols,
                                    shown_visual_rows,
                                    active_cap,
                                    capture_rows,
                                    fit_to_content,
                                );
                                if !fit_to_content {
                                    let ch = output_vte.char_height() as i32;
                                    if ch > 0 {
                                        let (_, visible_rows, _) = bounded_finished_vte_geometry(
                                            eff_cols,
                                            shown_visual_rows.min(active_cap).max(1),
                                            0,
                                        );
                                        output_vte.set_height_request(finished_vte_height_px(
                                            visible_rows,
                                            ch,
                                        ));
                                    }
                                }
                            }
                            let has_query = filter_enabled.get() && !q.trim().is_empty();
                            if invalid_regex {
                                filter_btn.add_css_class("block-action-active");
                                filter_status.set_visible(true);
                                filter_status.set_text("Invalid regular expression");
                                filter_status.add_css_class("block-filter-empty");
                            } else if has_query {
                                filter_btn.add_css_class("block-action-active");
                                filter_status.set_visible(true);
                                let hidden = full_rows.saturating_sub(shown_rows);
                                if shown.trim().is_empty() {
                                    filter_status.set_text("No matches");
                                    filter_status.add_css_class("block-filter-empty");
                                } else {
                                    filter_status.remove_css_class("block-filter-empty");
                                    filter_status.set_text(&format!(
                                        "{} shown, {} hidden",
                                        line_count_text(shown_rows),
                                        hidden
                                    ));
                                }
                            } else {
                                filter_btn.remove_css_class("block-action-active");
                                filter_status.remove_css_class("block-filter-empty");
                                filter_status.set_visible(false);
                            }
                            collapsed_summary.set_label(&collapsed_output_summary(shown_rows));
                            expand_btn.set_visible(can_expand);
                            jump_bottom_btn.set_visible(shown_visual_rows > fitted_cap);
                            // Keep `displayed_output` in sync so a later unmap → remap
                            // (block scrolls out of view, then back) re-feeds the
                            // filtered text, not the full output.
                            if display_changed {
                                *displayed_output.borrow_mut() = display_override;
                            }
                        }
                    };
                    let apply = Rc::new(apply);
                    let pending_apply: Rc<RefCell<Option<glib::SourceId>>> =
                        Rc::new(RefCell::new(None));
                    let apply_generation = Rc::new(Cell::new(0_u64));
                    // Explicit option/context/filter-row actions apply immediately and
                    // invalidate an older keystroke timeout.
                    let apply_now = {
                        let pending_apply = pending_apply.clone();
                        let apply_generation = apply_generation.clone();
                        let apply = apply.clone();
                        Rc::new(move || {
                            apply_generation.set(apply_generation.get().wrapping_add(1));
                            if let Some(source) = pending_apply.borrow_mut().take() {
                                source.remove();
                            }
                            apply();
                        })
                    };
                    let schedule_apply = {
                        let pending_apply = pending_apply.clone();
                        let apply_generation = apply_generation.clone();
                        let apply = apply.clone();
                        Rc::new(move || {
                            let generation = apply_generation.get().wrapping_add(1);
                            apply_generation.set(generation);
                            if let Some(source) = pending_apply.borrow_mut().take() {
                                source.remove();
                            }

                            let pending_slot = pending_apply.clone();
                            let pending_clear = pending_apply.clone();
                            let apply_generation = apply_generation.clone();
                            let apply = apply.clone();
                            let source = glib::timeout_add_local(
                                FINISHED_OUTPUT_FILTER_DEBOUNCE,
                                move || {
                                    if apply_generation.get() == generation {
                                        // A stale callback must not clear a newer
                                        // timeout stored in the shared slot.
                                        pending_clear.borrow_mut().take();
                                        apply();
                                    }
                                    glib::ControlFlow::Break
                                },
                            );
                            *pending_slot.borrow_mut() = Some(source);
                        })
                    };
                    {
                        let schedule_apply = schedule_apply.clone();
                        filter_entry.connect_search_changed(move |_| schedule_apply());
                    }
                    for tg in [&regex_tg, &case_tg, &invert_tg] {
                        let apply_now = apply_now.clone();
                        tg.connect_toggled(move |_| apply_now());
                    }
                    {
                        let apply_now = apply_now.clone();
                        ctx_spin.connect_value_changed(move |_| apply_now());
                    }
                    {
                        let pending_apply = pending_apply.clone();
                        let apply_generation = apply_generation.clone();
                        filter_entry.connect_destroy(move |_| {
                            apply_generation.set(apply_generation.get().wrapping_add(1));
                            if let Some(source) = pending_apply.borrow_mut().take() {
                                source.remove();
                            }
                        });
                    }

                    Some((filter_row.downgrade(), filter_entry.downgrade(), apply_now))
                })
            };
            let built: Rc<RefCell<Option<FilterRowHandles>>> = Rc::new(RefCell::new(None));
            let content_for_toggle = content.downgrade();
            let cmd_row_for_toggle = cmd_row.downgrade();
            let filter_btn_for_toggle = filter_btn_weak;
            let filter_enabled_for_toggle = filter_enabled.clone();
            let set_collapsed_for_filter = set_collapsed.clone();
            let toggle: Rc<dyn Fn()> = Rc::new(move || {
                let (Some(content), Some(cmd_row), Some(button)) = (
                    content_for_toggle.upgrade(),
                    cmd_row_for_toggle.upgrade(),
                    filter_btn_for_toggle.upgrade(),
                ) else {
                    return;
                };
                if built.borrow().is_none() {
                    let handles = build_filter_row(&content, &cmd_row);
                    *built.borrow_mut() = handles;
                }
                let built = built.borrow();
                let Some((row, entry, apply_now)) = built.as_ref() else {
                    return;
                };
                let (Some(filter_row), Some(entry)) = (row.upgrade(), entry.upgrade()) else {
                    return;
                };
                let show = !filter_row.is_visible();
                filter_enabled_for_toggle.set(show);
                filter_row.set_visible(show);
                if show {
                    set_collapsed_for_filter(false);
                    button.add_css_class("block-action-active");
                    entry.grab_focus();
                } else {
                    button.remove_css_class("block-action-active");
                }
                apply_now();
            });
            *filter_toggle_handle.borrow_mut() = Some(Rc::downgrade(&toggle));
            let toggle_for_button = toggle.clone();
            filter_btn.connect_clicked(move |_| toggle_for_button());
            toggle
        };

        FinishedBlock {
            id,
            is_background,
            widget: outer,
            content,
            virtualized_height,
            virtualized,
            compact,
            prompt_text: prompt.to_string(),
            command_vte,
            output_vte,
            full_output,
            displayed_output,
            stripped_output: Rc::new(RefCell::new(None)),
            cmd_text: cmd.to_string(),
            output_scrollbar,
            copy_cmd_btn,
            copy_output_btn,
            rerun_btn,
            header_row,
            action_box,
            selection_hint,
            selection_hint_steady: Rc::new(RefCell::new(String::new())),
            selection_feedback_generation: Rc::new(Cell::new(0)),
            toggle_filter,
            set_collapsed,
            collapsed_summary,
            filtered_out: Rc::new(Cell::new(false)),
            restore_live_focus,
            refit_output,
            visual_rows_cache,
            render_stamp,
            pending_font_scale,
            displayed_generation,
            jump_bottom_btn,
            bookmark_star,
            status_icon,
            lifecycle_chip,
            cols,
            viewport_cap,
            long_output,
            estimated_retained_bytes,
        }
    }

    pub(crate) const fn estimated_retained_bytes(&self) -> usize {
        self.estimated_retained_bytes
    }

    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    /// Show lifecycle provenance only when the completion is not fully trusted.
    /// Background output has no command completion to qualify.
    pub(crate) fn set_lifecycle(&self, health: BlockLifecycleHealth, notice: Option<&str>) {
        let badge = super::unified_chrome::lifecycle_badge(health).filter(|_| !self.is_background);
        match badge {
            Some(badge) => {
                self.lifecycle_chip.set_text(badge);
                self.lifecycle_chip.set_tooltip_text(notice);
                self.lifecycle_chip
                    .update_property(&[gtk4::accessible::Property::Label(notice.unwrap_or(badge))]);
                self.lifecycle_chip.set_visible(true);
            }
            None => {
                self.lifecycle_chip.set_visible(false);
                self.lifecycle_chip.set_tooltip_text(None);
            }
        }
    }

    /// Switch this card between the normal and compact densities in place and
    /// return the height its virtualization placeholder must contribute.
    pub(crate) fn set_compact(&self, compact: bool) -> i32 {
        let previous = self.compact.replace(compact);
        if previous != compact {
            let delta = finished_card_vchrome_px(compact)
                .saturating_sub(finished_card_vchrome_px(previous));
            let height = self.virtualized_height.get().saturating_add(delta).max(1);
            self.virtualized_height.set(height);
            if self.virtualized.get() {
                self.widget.set_height_request(height);
            }
        }
        apply_card_density(&self.widget, compact);
        apply_header_density(&self.header_row, compact);
        self.virtualized_height.get().max(1)
    }

    /// Unmap expensive VTE content while preserving the card's measured height.
    /// Returning the placeholder height lets the caller keep virtualization
    /// metadata synchronized with the actual GTK allocation.
    pub(crate) fn set_virtualized(&self, virtualized: bool) -> i32 {
        self.set_virtualized_with_measurement(virtualized, true)
    }

    /// Density switches already translated the saved height before GTK has
    /// allocated the new margins. Their immediate visibility reconciliation
    /// must not sample the still-old allocation back over that new model.
    pub(crate) fn set_virtualized_preserving_height(&self, virtualized: bool) -> i32 {
        self.set_virtualized_with_measurement(virtualized, false)
    }

    fn set_virtualized_with_measurement(&self, virtualized: bool, measure_allocation: bool) -> i32 {
        if self.virtualized.replace(virtualized) == virtualized {
            return self.virtualized_height.get().max(1);
        }

        if virtualized {
            let allocated = self.widget.height();
            if measure_allocation && allocated > 1 {
                self.virtualized_height.set(allocated);
            }
            let height = self.virtualized_height.get().max(1);
            self.widget.set_height_request(height);
            self.content.set_visible(false);
            height
        } else {
            self.content.set_visible(true);
            self.widget.set_height_request(-1);
            self.virtualized_height.get().max(1)
        }
    }

    /// Whether a pane-level filter is hiding this card.
    pub(crate) fn is_filtered_out(&self) -> bool {
        self.filtered_out.get()
    }

    /// Hide or show this card for a pane-level filter, reporting whether
    /// anything moved.
    ///
    /// Sets the OUTER widget's visibility, which is what makes the card
    /// contribute nothing to the document — a virtualized card still holds a
    /// measured placeholder, and that is the difference between "off-screen"
    /// and "filtered away".
    pub(crate) fn set_filtered_out(&self, filtered_out: bool) -> bool {
        use gtk4::prelude::WidgetExt as _;

        if self.filtered_out.replace(filtered_out) == filtered_out {
            return false;
        }
        self.widget.set_visible(!filtered_out);
        true
    }

    /// Whether this card's output is folded away.
    pub(crate) fn is_collapsed(&self) -> bool {
        use gtk4::prelude::WidgetExt as _;
        self.collapsed_summary.is_visible()
    }

    /// Fold or unfold this card's output, reporting whether anything moved.
    ///
    /// The pane uses the answer to skip the layout pass entirely when a bulk
    /// collapse found nothing to do.
    pub(crate) fn set_collapsed(&self, collapsed: bool) -> bool {
        if self.is_collapsed() == collapsed {
            return false;
        }
        (self.set_collapsed)(collapsed);
        true
    }

    /// What this card's output VTE currently holds — see [`RenderStamp`].
    ///
    /// A find pass records it per surface. If it changes before the pass
    /// navigates, the native search cursor it was stepping from no longer
    /// exists: `vte.reset` at re-feed drops the selection, and a re-window
    /// moves every row. Stepping anyway silently selects the wrong hit while
    /// the counter keeps counting.
    pub(crate) fn render_stamp(&self) -> RenderStamp {
        self.render_stamp.get()
    }

    /// Adopt a new font scale, or defer it if this card is virtualized.
    ///
    /// Ctrl+scroll emits one notch per 0.025, and each notch reached every
    /// retained card. Resetting a VTE's font metrics forces a re-measure and a
    /// `queue_resize`; doing that for cards virtualization has already hidden
    /// buys nothing, because they will be re-measured on their way back in
    /// anyway. Returns whether the scale was applied now.
    pub(crate) fn set_font_scale(&self, scale: f64) -> bool {
        if !self.content.is_mapped() {
            self.pending_font_scale.set(Some(scale));
            return false;
        }
        self.pending_font_scale.set(None);
        self.command_vte.set_font_scale(scale);
        self.output_vte.set_font_scale(scale);
        true
    }

    /// Re-fit this block's output to the space the pane currently offers,
    /// returning the card's new height when the geometry actually changed.
    /// Cheap to call on a block whose cap is unchanged, and a no-op for
    /// virtualized cards — those refit through `connect_map` on their way back
    /// into the viewport.
    pub(crate) fn refit_output_to_viewport(&self) -> Option<i32> {
        (self.refit_output)()
    }

    /// Scroll this block's top or bottom edge into the outer history canvas.
    pub(crate) fn scroll_to_edge(&self, outer: &gtk4::ScrolledWindow, bottom: bool) {
        schedule_block_edge_scroll(&self.widget, outer, bottom);
    }

    /// Forward wheel events on the output VTE to the outer ScrolledWindow once
    /// the VTE's internal scrollback can't move further in the wheel direction.
    /// Without this the user's scroll "sticks" at a long block's edge: VTE
    /// silently swallows wheels that no longer scroll its own buffer, and the
    /// page never resumes. Closes the perceptual gap with a single-scrollback
    /// VTE pane (terminator/xterm).
    /// `debouncer` records the user's scroll intent for wheel motion this card
    /// hands on to the history — see [`ScrollDebouncer::record_wheel_intent`].
    pub(crate) fn connect_scroll_forwarding(
        &self,
        outer: &gtk4::ScrolledWindow,
        debouncer: &ScrollDebouncer,
    ) {
        let output_for_jump = self.output_vte.downgrade();
        let widget_for_jump = self.widget.downgrade();
        let outer_for_jump = outer.downgrade();
        self.jump_bottom_btn.connect_clicked(move |_| {
            let (Some(output), Some(widget), Some(outer)) = (
                output_for_jump.upgrade(),
                widget_for_jump.upgrade(),
                outer_for_jump.upgrade(),
            ) else {
                return;
            };
            if let Some(adj) = output.vadjustment() {
                let target = (adj.upper() - adj.page_size()).max(adj.lower());
                if target > adj.lower() + f64::EPSILON {
                    adj.set_value(target);
                    return;
                }
            }
            schedule_block_edge_scroll(&widget, &outer, true);
        });

        let command_scroll =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        command_scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let outer_for_command = outer.downgrade();
        let debouncer_for_command = debouncer.clone();
        command_scroll.connect_scroll(move |_, _dx, dy| {
            let Some(outer_for_command) = outer_for_command.upgrade() else {
                return glib::Propagation::Proceed;
            };
            forward_outer_scroll(&outer_for_command, dy);
            debouncer_for_command.record_wheel_intent(&outer_for_command);
            glib::Propagation::Stop
        });
        self.command_vte.add_controller(command_scroll);

        let scroll_ctrl =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let vte = self.output_vte.downgrade();
        let outer_for_vte = outer.downgrade();
        let debouncer_for_vte = debouncer.clone();
        scroll_ctrl.connect_scroll(move |_, _dx, dy| {
            let (Some(vte), Some(outer_for_vte)) = (vte.upgrade(), outer_for_vte.upgrade()) else {
                return glib::Propagation::Proceed;
            };
            // The cap is determined only after map/resize. Inspect the actual
            // VTE adjustment on every wheel event rather than trusting a stale
            // construction-time flag.
            let Some(inner_adj) = vte.vadjustment() else {
                return glib::Propagation::Proceed;
            };
            let at_top = inner_adj.value() <= inner_adj.lower() + f64::EPSILON;
            let at_bottom =
                inner_adj.value() + inner_adj.page_size() >= inner_adj.upper() - f64::EPSILON;
            let going_up = dy < 0.0;
            let going_down = dy > 0.0;
            if (going_up && !at_top) || (going_down && !at_bottom) {
                // VTE still has room to scroll itself; let it.
                return glib::Propagation::Proceed;
            }
            // Drive the outer ScrolledWindow by one step in the wheel direction.
            forward_outer_scroll(&outer_for_vte, dy);
            debouncer_for_vte.record_wheel_intent(&outer_for_vte);
            glib::Propagation::Stop
        });
        self.output_vte.add_controller(scroll_ctrl);

        // Wheeling over the slider itself should move the block it belongs to,
        // not the history behind it. GtkScrollbar would scroll its own
        // adjustment natively, but it stops dead at the ends; capture the event
        // so the same edge hand-off as the VTE applies.
        let scrollbar_scroll =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        scrollbar_scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let vte_for_scrollbar = self.output_vte.downgrade();
        let outer_for_scrollbar = outer.downgrade();
        let debouncer_for_scrollbar = debouncer.clone();
        scrollbar_scroll.connect_scroll(move |_, _dx, dy| {
            let (Some(vte), Some(outer)) =
                (vte_for_scrollbar.upgrade(), outer_for_scrollbar.upgrade())
            else {
                return glib::Propagation::Proceed;
            };
            if let Some(inner_adj) = vte.vadjustment() {
                if scroll_adjustment(&inner_adj, dy) {
                    return glib::Propagation::Stop;
                }
            }
            forward_outer_scroll(&outer, dy);
            debouncer_for_scrollbar.record_wheel_intent(&outer);
            glib::Propagation::Stop
        });
        self.output_scrollbar.add_controller(scrollbar_scroll);
    }

    /// Wire the hover quick-action buttons (copy command, copy output, re-run).
    /// Kept separate from construction because handlers need the clipboard, PTY,
    /// and active block, which only the owning `TermView` has.
    pub(super) fn connect_actions(
        &self,
        vte: &Terminal,
        verified_submission: &super::VerifiedSubmissionCtx,
        config: &Rc<RefCell<Config>>,
        active: &Rc<RefCell<ActiveBlock>>,
        bracketed_paste: &Rc<Cell<bool>>,
    ) {
        // Supply the late-bound focus hand-back the filter row needs. Weak on
        // the live block so a card outliving its pane cannot resurrect it.
        {
            let active_for_focus = Rc::downgrade(active);
            let restore: Rc<dyn Fn()> = Rc::new(move || {
                if let Some(active) = active_for_focus.upgrade() {
                    active.borrow().grab_focus();
                }
            });
            *self.restore_live_focus.borrow_mut() = Some(restore);
        }

        let vte_for_cmd = vte.downgrade();
        let cmd_for_copy = self.cmd_text.clone();
        self.copy_cmd_btn.connect_clicked(move |btn| {
            let Some(vte_for_cmd) = vte_for_cmd.upgrade() else {
                return;
            };
            vte_for_cmd.clipboard().set_text(&cmd_for_copy);
            flash_button_icon(btn, "object-select-symbolic", "Command copied");
        });

        let vte_for_out = vte.downgrade();
        // Copy the FULL output (ANSI stripped), not just the collapsed first-N
        // lines shown in output_buffer before "Show more" is clicked.
        let full_output_for_copy = self.full_output.clone();
        let stripped_output_for_copy = self.stripped_output.clone();
        self.copy_output_btn.connect_clicked(move |btn| {
            let Some(vte_for_out) = vte_for_out.upgrade() else {
                return;
            };
            let text = Self::with_cached_stripped_output(
                &full_output_for_copy,
                &stripped_output_for_copy,
                |s| s.to_string(),
            );
            vte_for_out.clipboard().set_text(&text);
            flash_button_icon(btn, "object-select-symbolic", "Output copied");
        });

        let verified_submission_for_rerun = verified_submission.clone();
        let config_for_rerun = config.clone();
        let active_for_rerun = Rc::downgrade(active);
        let bracketed_for_rerun = bracketed_paste.clone();
        let cmd_for_rerun = self.cmd_text.clone();
        self.rerun_btn.connect_clicked(move |btn| {
            let suggestion = click_cursor::suggestion_rgb(&config_for_rerun.borrow().palette);
            if verified_submission_for_rerun.try_recall_command(
                &cmd_for_rerun,
                bracketed_for_rerun.get(),
                suggestion,
            ) {
                if let Some(active_for_rerun) = active_for_rerun.upgrade() {
                    active_for_rerun.borrow().grab_focus();
                }
                flash_button_icon(btn, "object-select-symbolic", "Command inserted");
            } else {
                flash_button_icon(
                    btn,
                    "dialog-warning-symbolic",
                    "Wait for an editable prompt",
                );
            }
        });
    }
}

// ─── ActiveBlock ──────────────────────────────────────────────────────────────

/// The live area: a single persistent input-enabled VTE pinned to the viewport
/// height. The shell's prompt, the user's typing, and command
/// output all render natively in this one VTE. When a command finishes, its
/// accumulated output (`raw_output`) is snapshotted into a styled FinishedBlock
/// stacked above this card.
pub(crate) struct ActiveBlock {
    pub(crate) widget: gtk4::Box,
    pub(crate) active_vte: Terminal,
    /// The measured wrapper around the live VTE. It carries the terminal's
    /// requested pixel size — see [`ActiveBlock::set_live_geometry`], which
    /// keeps the grid at the full viewport while the card shows less.
    vte_overlay: gtk4::Overlay,
    /// Sole measured child of the clip: its height IS the live card's height.
    live_spacer: gtk4::Box,
    /// The clip itself; its allocated width is the space the terminal may use.
    live_clip: gtk4::Overlay,
    /// Last applied `(width_px, grid_px, visible_px)`, so a layout pass that
    /// changes nothing does not queue a resize.
    live_geometry: Cell<(i32, i32, i32)>,
    /// High-water row extent of the command in flight. Monotone within one
    /// command so a `\r` progress bar, an `ESC[1A` redraw or a mid-command
    /// `clear` can never make the card shrink under the output already on
    /// screen. `reset_active` — the single funnel every reset path uses —
    /// clears it for the next command.
    live_extent_rows: Rc<Cell<i64>>,
    /// Cursor row this command's output started from. Paired with
    /// `live_extent_rows` so both are re-based by the same reset funnel.
    /// Lowest ring row the prompt drew on, and the highest the cursor has
    /// reached since. Both are `cursor_position()` readings, so they are in
    /// one coordinate system by construction — which the live adjustment is
    /// not, and neither is a literal zero: `vte.reset()` does not rewind
    /// VTE's absolute row counter (`Ring::reset` returns `m_end` unchanged),
    /// so rows keep climbing for the life of the pane.
    live_cursor_origin: Rc<Cell<Option<i64>>>,
    live_cursor_high: Rc<Cell<i64>>,
    /// `preserve_live_scrollback` as it stands now (`reload_config` writes it).
    /// It decides where the prompt lives inside the live grid, which is what
    /// the compact-card layout has to know: with the default reset the prompt
    /// starts at the top of a freshly cleared screen, so the card can show the
    /// grid's first rows and grow into whatever the shell drew below the input.
    /// When the previous command's output is deliberately kept, the prompt is
    /// at the *bottom* of the ring instead, so the grid stays pinned to the
    /// card exactly as it was before.
    preserve_live_scrollback: Cell<bool>,
    /// Pass-through, non-measuring surface for small live widgets that should
    /// inhabit the running terminal without changing its grid.  The live VTE
    /// remains the overlay's measured child, and the scrollbar is stacked
    /// above this surface so an organism can never make it unreachable.
    pub(crate) live_organism_surface: gtk4::Fixed,
    /// Probe-addressed Kitty image layer used by Unified mode. Added before
    /// the organism surface so inline assistant UI always remains readable.
    /// Hidden in Block mode, whose images move into finished cards instead.
    pub(crate) unified_image_surface: gtk4::Fixed,
    /// Pass-through, non-measuring chrome overlay used only by Unified mode.
    /// It exists (hidden) in Block mode so the widget tree stays mode-neutral.
    pub(crate) unified_chrome_surface: gtk4::DrawingArea,
    /// Slim overlay scrollbar bound to the live VTE's own adjustment, so the
    /// still-running command's scrollback is visibly navigable. An overlay —
    /// not a sibling like the finished-block scrollbar — because appearing
    /// mid-command must not narrow the grid and SIGWINCH the child.
    pub(crate) live_scrollbar: gtk4::Scrollbar,
    /// The feature-level visibility requested by the organism runtime.  Alt
    /// screen temporarily overrides it without losing the requested state.
    live_organism_visible: Cell<bool>,
    live_organism_alt_screen: Cell<bool>,
    /// Raw output bytes accumulated during CollectingOutput (anvil's
    /// `out_buf`). Engine-owned shared state constructed in `TermView::new`:
    /// the reader engine appends, clears, and snapshots it directly; this
    /// clone exists only so live-find can read a bounded prefix
    /// ([`Self::output_text_prefix`]).
    raw_output: Rc<RefCell<BoundedByteRing>>,
}

impl ActiveBlock {
    /// `pub(super)`: only `TermView::new` constructs the live block, and the
    /// ring parameter's type is itself private to `block_view`.
    pub(super) fn new(config: &Config, raw_output: Rc<RefCell<BoundedByteRing>>) -> Self {
        let widget = gtk4::Box::new(Orientation::Vertical, 0);
        widget.add_css_class("block-active");
        if config.block_compact {
            widget.add_css_class("block-compact");
        }
        // focusable(false) keeps the holder Box from being a focus target, but we
        // must NOT set can_focus(false): in GTK4 that blocks all descendants
        // (including active_vte) from ever receiving focus.
        widget.set_focusable(false);
        widget.set_hexpand(true);
        // The outer block document owns vertical expansion. The live surface is
        // explicitly sized compact/full by block_view; keeping it non-expanding
        // prevents GTK from adding document slack to its grid.
        widget.set_vexpand(false);

        let active_vte = create_active_terminal(config);
        active_vte.set_hexpand(true);
        active_vte.set_vexpand(false);
        let vte_overlay = gtk4::Overlay::new();
        vte_overlay.set_hexpand(true);
        vte_overlay.set_vexpand(false);
        vte_overlay.set_child(Some(&active_vte));

        let live_organism_surface = gtk4::Fixed::new();
        live_organism_surface.set_hexpand(true);
        live_organism_surface.set_vexpand(true);
        live_organism_surface.set_halign(gtk4::Align::Fill);
        live_organism_surface.set_valign(gtk4::Align::Fill);
        live_organism_surface.set_overflow(gtk4::Overflow::Hidden);
        live_organism_surface.set_can_target(false);
        live_organism_surface.set_focusable(false);
        live_organism_surface.set_visible(false);

        let unified_image_surface = gtk4::Fixed::new();
        unified_image_surface.set_hexpand(true);
        unified_image_surface.set_vexpand(true);
        unified_image_surface.set_halign(gtk4::Align::Fill);
        unified_image_surface.set_valign(gtk4::Align::Fill);
        unified_image_surface.set_overflow(gtk4::Overflow::Hidden);
        unified_image_surface.set_can_target(false);
        unified_image_surface.set_focusable(false);
        unified_image_surface.set_visible(false);
        // Paint above terminal cells but below the organism and chrome. The
        // insertion order is load-bearing: Unified's organism has no other
        // body surface and must never be silently covered by a large image.
        vte_overlay.add_overlay(&unified_image_surface);
        vte_overlay.set_measure_overlay(&unified_image_surface, false);
        vte_overlay.set_clip_overlay(&unified_image_surface, true);
        vte_overlay.add_overlay(&live_organism_surface);
        vte_overlay.set_measure_overlay(&live_organism_surface, false);
        vte_overlay.set_clip_overlay(&live_organism_surface, true);

        let unified_chrome_surface = gtk4::DrawingArea::new();
        unified_chrome_surface.set_hexpand(true);
        unified_chrome_surface.set_vexpand(true);
        unified_chrome_surface.set_halign(gtk4::Align::Fill);
        unified_chrome_surface.set_valign(gtk4::Align::Fill);
        unified_chrome_surface.set_can_target(false);
        unified_chrome_surface.set_focusable(false);
        unified_chrome_surface.set_visible(false);
        vte_overlay.add_overlay(&unified_chrome_surface);
        vte_overlay.set_measure_overlay(&unified_chrome_surface, false);
        vte_overlay.set_clip_overlay(&unified_chrome_surface, true);

        let live_scrollbar =
            gtk4::Scrollbar::new(Orientation::Vertical, active_vte.vadjustment().as_ref());
        live_scrollbar.add_css_class("block-output-scrollbar");
        live_scrollbar.set_tooltip_text(Some("Scroll within the running output"));
        live_scrollbar.set_halign(gtk4::Align::End);
        live_scrollbar.set_visible(false);
        // Add the scrollbar last: GTK paints later overlays above earlier ones.

        // ── Live card clip ────────────────────────────────────────────────
        // The card is only as tall as the running command's output so far, but
        // the terminal underneath keeps the FULL viewport grid: that is the
        // winsize the child was told about (`pty_grid_size`), and anything that
        // addresses rows absolutely — `top`, `watch`, any repaint that clears
        // the screen without switching to the alternate one — would otherwise
        // be drawing into a grid too short to hold it.
        //
        // GTK derives the grid from the VTE's *allocation*: `set_size` cannot
        // hold a taller grid than the space the parent hands out (measured — an
        // explicit `set_size(200, 50)` reverted on the next reallocation), and
        // neither a ScrolledWindow/Viewport nor a plain non-FILL overlay child
        // keeps them apart (both squeeze the terminal to the visible height).
        // `gtk4::Fixed` does: it allocates each child the size the child asked
        // for, whatever height the Fixed itself has. Riding it as a non-measured
        // overlay above a spacer means the card measures the spacer alone while
        // the terminal keeps every row, and `Overflow::Hidden` clips the rows
        // below the card — for input as well as for paint. Both dimensions of
        // the child's size request are required: inside a Fixed a `-1` collapses
        // the child to its minimum (the same recipe the organism surface uses).
        let live_spacer = gtk4::Box::new(Orientation::Vertical, 0);
        live_spacer.set_hexpand(true);
        live_spacer.set_vexpand(false);
        let live_surface = gtk4::Fixed::new();
        live_surface.set_overflow(gtk4::Overflow::Hidden);
        live_surface.set_halign(gtk4::Align::Fill);
        live_surface.set_valign(gtk4::Align::Fill);
        live_surface.put(&vte_overlay, 0.0, 0.0);
        let live_clip = gtk4::Overlay::new();
        live_clip.set_hexpand(true);
        live_clip.set_vexpand(false);
        live_clip.set_overflow(gtk4::Overflow::Hidden);
        live_clip.set_child(Some(&live_spacer));
        live_clip.add_overlay(&live_surface);
        live_clip.set_measure_overlay(&live_surface, false);
        live_clip.set_clip_overlay(&live_surface, true);
        // The scrollbar rides the CLIP, not the terminal: `vte_overlay` is now
        // allocated the whole grid, so a scrollbar inside it would be sized
        // against rows the card is not showing and cut off halfway.
        live_clip.add_overlay(&live_scrollbar);
        widget.append(&live_clip);
        // Same adjustment-driven visibility as the finished-block scrollbar:
        // the adjustment is the one place that always knows whether the live
        // buffer has scrolled past the viewport.
        if let Some(adj) = active_vte.vadjustment() {
            let scrollbar = live_scrollbar.downgrade();
            let sync_visibility = move |adj: &gtk4::Adjustment| {
                let Some(scrollbar) = scrollbar.upgrade() else {
                    return;
                };
                let overflows = adj.upper() - adj.lower() > adj.page_size() + f64::EPSILON;
                scrollbar.set_visible(overflows);
            };
            sync_visibility(&adj);
            adj.connect_changed(sync_visibility);
        }

        // `realize` is too early: the VTE's IM context does not have a mapped
        // surface yet. Taking logical focus there can suppress the real
        // focus-in fcitx/ibus need. `map` is the first valid point to focus.
        active_vte.connect_map(|terminal| {
            terminal.grab_focus();
        });

        ActiveBlock {
            widget,
            active_vte,
            vte_overlay,
            live_spacer,
            live_clip,
            live_geometry: Cell::new((0, 0, 0)),
            live_extent_rows: Rc::new(Cell::new(0)),
            live_cursor_origin: Rc::new(Cell::new(None)),
            live_cursor_high: Rc::new(Cell::new(0)),
            preserve_live_scrollback: Cell::new(config.preserve_live_scrollback),
            live_organism_surface,
            unified_image_surface,
            unified_chrome_surface,
            live_scrollbar,
            live_organism_visible: Cell::new(false),
            live_organism_alt_screen: Cell::new(false),
            raw_output,
        }
    }

    /// Return at most `max_bytes` from the live capture for bounded main-thread
    /// consumers such as incremental find. Raw PTY bytes may not be valid UTF-8;
    /// lossy conversion keeps the returned String safe even when the byte budget
    /// ends in the middle of a code point.
    pub(crate) fn output_text_prefix(&self, max_bytes: usize) -> (String, bool) {
        let mut raw = self.raw_output.borrow_mut();
        if raw.is_empty() {
            return (String::new(), false);
        }
        let bytes = raw.make_contiguous();
        let end = bytes.len().min(max_bytes);
        (
            String::from_utf8_lossy(&bytes[..end]).into_owned(),
            end < bytes.len(),
        )
    }

    /// The column count the live VTE is wrapping at — the single source of truth
    /// for pre-wrapping finished blocks so they align with what the user watched.
    pub(crate) fn grid_cols(&self) -> usize {
        (self.active_vte.column_count().max(20)) as usize
    }

    /// Give the live terminal a `grid_rows`-tall grid and show `visible_rows`
    /// of it.
    ///
    /// The two are equal everywhere except while a command is running, where the
    /// card grows with the output and the grid stays a full viewport (see the
    /// clip construction in [`ActiveBlock::new`]). Returns whether anything
    /// changed, so callers can skip follow-up work on a no-op layout pass.
    ///
    /// The width comes from the clip's own allocation — inside a `gtk4::Fixed`
    /// the terminal is allocated exactly what it requests, so it cannot pick up
    /// the pane width by expanding. Before the first allocation there is no
    /// width to hand out and the request is left alone; the next layout pass
    /// (contents, adjustment or resize tick) applies it.
    pub(crate) fn set_live_geometry(&self, cell_h: i32, grid_rows: i64, visible_rows: i64) -> bool {
        let cell_h = cell_h.max(1);
        let grid_rows = grid_rows.max(1);
        let visible_rows = visible_rows.clamp(1, grid_rows);
        let width_px = self.live_clip.width();
        if width_px <= 0 {
            // Before the first allocation there is no width to hand out, but
            // the card height does not depend on one and the caller has already
            // moved the holder's request: leave the two in step.
            self.live_spacer
                .set_height_request((visible_rows as i32).saturating_mul(cell_h));
            return false;
        }
        // Ask for a sliver more than the grid needs. The terminal takes its row
        // count from the allocation, and a container that hands back a pixel or
        // two less than requested would cost a whole row; anything under one
        // cell cannot add one.
        let grid_px = (grid_rows as i32).saturating_mul(cell_h) + cell_h - 1;
        let visible_px = (visible_rows as i32).saturating_mul(cell_h);
        let geometry = (width_px, grid_px, visible_px);
        if self.live_geometry.get() == geometry {
            return false;
        }
        self.live_geometry.set(geometry);
        self.vte_overlay.set_size_request(width_px, grid_px);
        self.live_spacer.set_height_request(visible_px);
        true
    }

    /// The measured live card. The frame-clock resize tick watches its width:
    /// the terminal is sized by an explicit request now and cannot follow the
    /// pane on its own.
    pub(crate) fn live_clip(&self) -> gtk4::Overlay {
        self.live_clip.clone()
    }

    /// Whether the live surface keeps the previous command's scrollback at the
    /// prompt. Read by `block_layout_active_surface`; written by
    /// `TermView::reload_config` so a runtime config change is not stale here.
    pub(crate) fn preserve_live_scrollback(&self) -> bool {
        self.preserve_live_scrollback.get()
    }

    pub(crate) fn set_preserve_live_scrollback(&self, preserve: bool) {
        self.preserve_live_scrollback.set(preserve);
    }

    /// Shared high-water extent, cloned into `block_layout_active_surface`.
    pub(crate) fn live_extent_rows(&self) -> Rc<Cell<i64>> {
        self.live_extent_rows.clone()
    }

    /// Shared measurement origin, cloned into `block_layout_active_surface`.
    pub(crate) fn live_cursor_origin(&self) -> Rc<Cell<Option<i64>>> {
        self.live_cursor_origin.clone()
    }

    pub(crate) fn live_cursor_high(&self) -> Rc<Cell<i64>> {
        self.live_cursor_high.clone()
    }

    /// Height of the live card in pixels — the part of the grid the user can
    /// see. Live widgets positioned over the terminal (the organism) must stay
    /// inside it or they are clipped away.
    pub(crate) fn live_visible_height_px(&self) -> i32 {
        let (_, _, visible) = self.live_geometry.get();
        if visible > 0 {
            visible
        } else {
            self.active_vte.height().max(0)
        }
    }

    /// Reset the live VTE for the next prompt (anvil block.rs:1028-1044). `reset`
    /// acts immediately, but already-queued feed() bytes are processed async, so the
    /// in-stream clear (fed after them) wipes stale output in the correct order.
    ///
    /// `preserve_scrollback`: when true, keep the VTE's buffer + scrollback intact
    /// (SGR state is soft-reset). When false (the default), finished blocks
    /// remain the sole historical surface and the compact live cell shows only
    /// the current prompt.
    ///
    /// Deliberately does NOT touch the `raw_output` ring: that is engine-owned
    /// state, cleared explicitly by the reader engine around this reset (see
    /// `RenderBackend::reset_active_surface`).
    pub(crate) fn reset_active(&self, preserve_scrollback: bool) {
        // A new command starts a new card: forget how far the last one grew,
        // and the row its predecessor grew from.
        self.live_extent_rows.set(0);
        // Forget where the last card began. The next one re-latches its origin
        // from the prompt's own cursor samples; nothing here can name a row in
        // that coordinate system yet, because the bytes below are applied
        // asynchronously.
        self.live_cursor_origin.set(None);
        self.live_cursor_high.set(0);
        if preserve_scrollback {
            self.active_vte.feed(b"\x1b[0m");
        } else {
            self.active_vte.reset(true, true);
            self.active_vte.feed(b"\x1b[H\x1b[2J\x1b[3J");
        }
    }

    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    /// Switch the live input cell's density. Unified's holder carries
    /// `block-fullscreen` instead and is left alone by its caller.
    pub(crate) fn set_compact(&self, compact: bool) {
        if compact {
            self.widget.add_css_class("block-compact");
        } else {
            self.widget.remove_css_class("block-compact");
        }
    }

    pub(crate) fn grab_focus(&self) {
        self.active_vte.grab_focus();
    }

    pub(crate) fn set_live_organism_visible(&self, visible: bool) {
        self.live_organism_visible.set(visible);
        self.sync_live_organism_visibility();
    }

    pub(crate) fn set_live_organism_alt_screen(&self, alt_screen: bool) {
        let (desired, alt_screen) =
            live_organism_alt_transition(self.live_organism_visible.get(), alt_screen);
        self.live_organism_visible.set(desired);
        self.live_organism_alt_screen.set(alt_screen);
        self.sync_live_organism_visibility();
    }

    pub(crate) fn live_organism_alt_screen(&self) -> bool {
        self.live_organism_alt_screen.get()
    }

    fn sync_live_organism_visibility(&self) {
        self.live_organism_surface
            .set_visible(live_organism_is_visible(
                self.live_organism_visible.get(),
                self.live_organism_alt_screen.get(),
            ));
    }
}

fn live_organism_alt_transition(desired: bool, entering: bool) -> (bool, bool) {
    if entering {
        // Never let rmcup synchronously resurrect pre-TUI coordinates. Exit
        // only removes the override; a later measured heartbeat opts in again.
        (false, true)
    } else {
        (desired, false)
    }
}

fn live_organism_is_visible(desired: bool, alt_screen: bool) -> bool {
    desired && !alt_screen
}

// ─── TermView state machine ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BlockState {
    /// Waiting for first PromptStart or any bytes
    Idle,
    /// Between PromptStart and PromptEnd — collecting prompt text
    CollectingPrompt,
    /// Between PromptEnd and CommandStart — user is typing
    AwaitingCommand,
    /// Between CommandStart and CommandEnd — collecting output
    CollectingOutput,
    /// Inside full-screen app (vim/less/etc.)
    AltScreen,
    /// Between CommandEnd and next PromptStart — still collecting late output
    PostCommand,
    /// Shell has no OSC-133 integration: route all bytes to the raw VTE so output
    /// is never dropped. Entered from Idle when output arrives but no FTCS event
    /// has been seen within the startup grace window. Recovered to block mode if a
    /// PromptStart ever arrives (late-loading integration).
    RawFallback,
}

/// Replace the current shell edit buffer with a recalled command, optionally
/// submitting it. Returns whether the command had to be cut to its first line.
///
/// The encoding is [`build_command_recall`]'s, so this path cannot disagree with
/// the block-recall and clipboard paths about when to frame — and it writes the
/// whole payload in **one** call. Emitting the frame as three separate writes
/// (as this did) meant the body reached the PTY boundary with a frame already
/// open, which is exactly the window an embedded `ESC[201~` needs.
pub(crate) fn write_recalled_command(
    pty: &crate::pty::OwnedPty,
    cmd: &str,
    bracketed_paste: bool,
    execute: bool,
) -> Result<bool, crate::pty::PtyWriteError> {
    let paste = build_command_recall(cmd, bracketed_paste);
    if paste.is_empty() {
        return Ok(false);
    }
    let truncated = paste.risk.truncated_to_first_line;
    let mut payload = paste.bytes;
    if execute {
        // Outside the frame: readline does not execute a newline contained in a
        // bracketed paste, so a CR inside the frame would be swallowed. Keep it
        // in this same queue item: saturation can reject the whole submission,
        // never the command while accepting its Enter (or vice versa).
        payload.push(b'\r');
    }
    pty.write_bytes(&payload)?;
    Ok(truncated)
}

#[cfg(test)]
mod tests {
    use super::{
        block_clipboard_text, collapsed_output_summary, completed_block_retention_plan,
        estimated_completed_block_retained_bytes, exit_code_for_shared_surface,
        filter_output_lines, live_organism_alt_transition, live_organism_is_visible,
        terminal_grid_units_upper_bound, terminalize_line_breaks, BlockData, BlockLifecycleHealth,
        BlockOutcome, FinishedBlock, UNKNOWN_EXIT_NOTE, UNKNOWN_EXIT_SENTINEL,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    fn reset_visual_row_cache_counters() {
        super::OUTPUT_VISUAL_ROWS_CACHE_HITS.with(|hits| hits.set(0));
        super::OUTPUT_VISUAL_ROWS_CACHE_MISSES.with(|misses| misses.set(0));
    }

    fn visual_row_cache_counters() -> (usize, usize) {
        let hits = super::OUTPUT_VISUAL_ROWS_CACHE_HITS.with(Cell::get);
        let misses = super::OUTPUT_VISUAL_ROWS_CACHE_MISSES.with(Cell::get);
        (hits, misses)
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn lifecycle_chip_and_quick_actions_expose_truthful_status() {
        use gtk4::prelude::*;

        gtk4::init().expect("gtk init");
        let config = crate::config::Config::safe_defaults();
        let block = FinishedBlock::new(
            6,
            "$ ",
            "make",
            None,
            "built\n",
            Some(0),
            &config,
            None,
            None,
            None,
            80,
        );

        assert_eq!(
            block.copy_cmd_btn.icon_name().as_deref(),
            Some("edit-copy-symbolic")
        );
        assert_eq!(
            block.copy_output_btn.icon_name().as_deref(),
            Some("text-x-generic-symbolic")
        );
        assert_eq!(
            block.rerun_btn.icon_name().as_deref(),
            Some("insert-text-symbolic")
        );

        block.set_lifecycle(BlockLifecycleHealth::Healthy, None);
        assert!(!block.lifecycle_chip.is_visible());
        for (health, badge) in [
            (BlockLifecycleHealth::Recovered, "recovered"),
            (BlockLifecycleHealth::Degraded, "inferred"),
            (BlockLifecycleHealth::Incomplete, "incomplete"),
        ] {
            block.set_lifecycle(health, Some("completion provenance is not authoritative"));
            assert!(block.lifecycle_chip.is_visible());
            assert_eq!(block.lifecycle_chip.text(), badge);
            assert_eq!(
                block.lifecycle_chip.tooltip_text().as_deref(),
                Some("completion provenance is not authoritative")
            );
        }
        block.set_lifecycle(BlockLifecycleHealth::Healthy, None);
        assert!(!block.lifecycle_chip.is_visible());
        assert_eq!(block.lifecycle_chip.tooltip_text(), None);

        let background = FinishedBlock::new(
            7, "$ ", "", None, "async\n", None, &config, None, None, None, 80,
        );
        background.set_lifecycle(BlockLifecycleHealth::Incomplete, Some("no end marker"));
        assert!(!background.lifecycle_chip.is_visible());
    }

    /// `new_with_pool` reuses the output row count it already resolved instead
    /// of asking `estimated_finished_block_height_for_text` to walk the whole
    /// transcript a second time. That substitution is only safe while the two
    /// estimators agree, and they use different conventions for output that
    /// trims to nothing, so pin the mapping on the cases that distinguish them.
    #[test]
    fn precomputed_rows_reproduce_the_text_estimator_exactly() {
        let config = crate::config::Config::safe_defaults();
        let cases: [&str; 7] = [
            "",
            "   \n\t  ",
            "one line",
            "one\ntwo\nthree\n",
            &"x".repeat(600),
            "\x1b[31mred\x1b[0m\r\nplain\r\n",
            "\u{4f60}\u{597d}\u{4e16}\u{754c}\n",
        ];
        for cols in [40_i64, 80, 100] {
            for output in cases {
                let from_text = super::estimated_finished_block_height_for_text(
                    &config, "echo hi", output, cols,
                );
                // The exact expression `new_with_pool` uses.
                let rows = if output.trim().is_empty() {
                    0
                } else {
                    super::output_visual_row_count(output, cols).max(1)
                };
                let from_rows =
                    super::estimated_finished_block_height_for_rows(&config, "echo hi", rows, cols);
                assert_eq!(
                    from_text, from_rows,
                    "estimators disagree at cols={cols} for {output:?}"
                );
            }
        }
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn block_density_switches_on_widgets_that_already_exist() {
        use gtk4::prelude::*;

        gtk4::init().expect("gtk init");
        let config = crate::config::Config::safe_defaults();
        assert!(
            !config.block_compact,
            "the default density is the roomy one"
        );
        let block = super::FinishedBlock::new(
            4,
            "$ ",
            "echo density",
            None,
            "density\n",
            Some(0),
            &config,
            None,
            None,
            None,
            80,
        );

        // Card margins are GTK properties, not CSS, so the class alone proves
        // nothing. Pin both the compact transition and the exact round-trip
        // back to the construction-time roomy geometry.
        let roomy = (
            block.widget().margin_top(),
            block.widget().margin_start(),
            block.header_row.margin_start(),
            block.header_row.margin_top(),
        );
        assert!(!block.widget().has_css_class("block-compact"));

        // Virtualize before switching: this is the state whose explicit
        // height request and parallel BlockData model used to stay roomy.
        let roomy_placeholder = block.set_virtualized(true);
        let mut block_data = VecDeque::from([block_with_exit(Some(0))]);
        block_data[0].estimated_height = roomy_placeholder;
        super::apply_finished_card_density(std::slice::from_ref(&block), &mut block_data, true);
        let compact_placeholder = roomy_placeholder - 10;
        assert_eq!(block_data[0].estimated_height, compact_placeholder);
        assert_eq!(block.widget().height_request(), compact_placeholder);
        let compact = (
            block.widget().margin_top(),
            block.widget().margin_start(),
            block.header_row.margin_start(),
            block.header_row.margin_top(),
        );
        assert!(block.widget().has_css_class("block-compact"));
        assert!(
            compact.0 < roomy.0
                && compact.1 < roomy.1
                && compact.2 < roomy.2
                && compact.3 < roomy.3,
            "compact must tighten every margin: {compact:?} vs {roomy:?}"
        );

        super::apply_finished_card_density(std::slice::from_ref(&block), &mut block_data, false);
        assert_eq!(block_data[0].estimated_height, roomy_placeholder);
        assert_eq!(block.widget().height_request(), roomy_placeholder);
        assert!(!block.widget().has_css_class("block-compact"));
        assert_eq!(
            (
                block.widget().margin_top(),
                block.widget().margin_start(),
                block.header_row.margin_start(),
                block.header_row.margin_top(),
            ),
            roomy,
            "switching back must restore construction's own margins"
        );

        // A density change can move the viewport boundary before GTK has
        // allocated the new margins. The density-specific visibility pass
        // must virtualize from the translated model, not capture the old
        // roomy allocation on its way out.
        assert_eq!(block.set_virtualized(false), roomy_placeholder);
        super::apply_finished_card_density(std::slice::from_ref(&block), &mut block_data, true);
        let mut visible = std::collections::HashSet::from([0]);
        let mut document_index = super::super::BlockDocumentIndex::default();
        super::super::apply_visible_indices_preserving_heights(
            std::slice::from_ref(&block),
            &mut block_data,
            &mut document_index,
            &mut visible,
            std::collections::HashSet::new(),
        );
        assert!(visible.is_empty());
        assert_eq!(block_data[0].estimated_height, compact_placeholder);
        assert_eq!(block.widget().height_request(), compact_placeholder);

        // A filter removes the card from the document. Its private placeholder
        // still adopts the density for a later reveal, but zero is retained in
        // the metadata stream while it is absent.
        assert!(block.set_filtered_out(true));
        block_data[0].estimated_height = 0;
        super::apply_finished_card_density(std::slice::from_ref(&block), &mut block_data, true);
        assert_eq!(block_data[0].estimated_height, 0);
        assert_eq!(block.widget().height_request(), compact_placeholder);

        // The live input cell carries density as a class; its height comes
        // from `BLOCK_ACTIVE_COMPACT_VCHROME_PX` via that class.
        let raw_output = Rc::new(RefCell::new(super::BoundedByteRing::new(1024)));
        let live = super::ActiveBlock::new(&config, raw_output);
        assert!(!live.widget().has_css_class("block-compact"));
        live.set_compact(true);
        assert!(live.widget().has_css_class("block-compact"));
        live.set_compact(false);
        assert!(!live.widget().has_css_class("block-compact"));

        // Existing correction/review, suggestion and Agent notice trees are
        // not FinishedBlocks. Their stable assistant roles are enough to
        // update the imperative outer/header/body margins in place.
        for (role, body_class, compact_bottom) in [
            ("command-review-standalone", Some("command-review-body"), 7),
            ("command-suggestion", None, 7),
            ("block-agent", None, 6),
        ] {
            let assistant = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            assistant.add_css_class("block-finished");
            assistant.add_css_class("block-assistant");
            assistant.add_css_class(role);
            assistant.set_margin_top(4);
            assistant.set_margin_start(8);
            let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            header.add_css_class("block-header");
            header.set_margin_top(6);
            header.set_margin_start(12);
            assistant.append(&header);
            let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            if let Some(body_class) = body_class {
                body.add_css_class(body_class);
            }
            body.set_margin_start(12);
            body.set_margin_bottom(11);
            assistant.append(&body);

            assert!(super::apply_inline_assistant_density(
                assistant.upcast_ref(),
                true
            ));
            assert!(assistant.has_css_class("block-compact"));
            assert_eq!(assistant.margin_top(), 1);
            assert_eq!(header.margin_start(), 8);
            assert_eq!(header.margin_top(), 3);
            assert_eq!(body.margin_start(), 8);
            assert_eq!(body.margin_bottom(), compact_bottom);

            assert!(super::apply_inline_assistant_density(
                assistant.upcast_ref(),
                false
            ));
            assert!(!assistant.has_css_class("block-compact"));
            assert_eq!(assistant.margin_top(), 4);
            assert_eq!(header.margin_start(), 12);
            assert_eq!(body.margin_start(), 12);
        }
    }

    /// The pointer crossing a card must not move its metadata. The quick-action
    /// strip sits after the header's hexpanding spacer, so revealing it used to
    /// steal that width and slide the timestamp, duration and exit badge
    /// sideways by the strip's whole width — on every card the mouse passed
    /// over.
    #[test]
    #[ignore = "requires DISPLAY"]
    fn revealing_a_cards_actions_does_not_move_its_metadata() {
        use gtk4::prelude::*;

        gtk4::init().expect("gtk init");
        let config = crate::config::Config::safe_defaults();
        let card = super::FinishedBlock::new(
            1,
            "$ ",
            "cargo test",
            None,
            "ok\r\n",
            Some(1),
            &config,
            Some(1234),
            Some(1_700_000_000_000),
            Some("/home/user/project"),
            80,
        );

        let faded = card.header_row.measure(gtk4::Orientation::Horizontal, -1);
        super::reveal_block_actions(&card.action_box, true);
        let revealed = card.header_row.measure(gtk4::Orientation::Horizontal, -1);

        assert_eq!(
            (faded.0, faded.1),
            (revealed.0, revealed.1),
            "the action strip must keep its allocation in both states"
        );
        assert!(card.action_box.is_sensitive());
        assert!(card.action_box.can_target());
        super::reveal_block_actions(&card.action_box, false);
        assert!(
            !card.action_box.is_sensitive(),
            "a faded strip must take neither a click nor Tab focus"
        );
        assert!(
            !card.action_box.can_target(),
            "a faded strip must not leave a transparent pointer dead zone"
        );
    }

    /// The selection hint appears and disappears as the active edge moves. It
    /// belongs on the spacer's left, where showing it eats slack; on the right
    /// it would push the whole metadata run sideways instead.
    #[test]
    #[ignore = "requires DISPLAY"]
    fn the_selection_hint_sits_on_the_spacers_left() {
        use gtk4::prelude::*;

        gtk4::init().expect("gtk init");
        let config = crate::config::Config::safe_defaults();
        let card = super::FinishedBlock::new(
            1,
            "$ ",
            "cargo test",
            None,
            "ok\r\n",
            Some(0),
            &config,
            Some(5),
            Some(1_700_000_000_000),
            None,
            80,
        );

        let mut hint_index = None;
        let mut spacer_index = None;
        let mut index = 0;
        let mut child = card.header_row.first_child();
        while let Some(widget) = child {
            if widget == card.selection_hint.clone().upcast::<gtk4::Widget>() {
                hint_index = Some(index);
            } else if spacer_index.is_none() && widget.hexpands() {
                spacer_index = Some(index);
            }
            child = widget.next_sibling();
            index += 1;
        }

        let hint_index = hint_index.expect("the header carries a selection hint");
        let spacer_index = spacer_index.expect("the header carries an expanding spacer");
        assert!(
            hint_index < spacer_index,
            "hint at {hint_index} must precede the spacer at {spacer_index}"
        );
        assert_eq!(
            card.selection_hint.width_chars(),
            super::SELECTION_HINT_MIN_CHARS,
            "a narrow header must reserve the complete Escape affordance"
        );
    }

    /// The counter proof for the same change: when the caller already holds the
    /// row count, building the card must not walk the transcript again.
    /// `finalize_block` pays for exactly one walk; a second one here doubled the
    /// cost of finishing a large command at the moment the prompt is waiting.
    #[test]
    #[ignore = "requires DISPLAY"]
    fn a_precomputed_card_does_not_rewalk_its_transcript() {
        gtk4::init().expect("gtk init");
        let config = crate::config::Config::safe_defaults();
        let output: String = (1..=400).map(|i| format!("line {i}\r\n")).collect();
        let cols = 100_i64;
        let rows = super::output_visual_row_count(&output, cols);

        super::OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| calls.set(0));
        let _card = super::FinishedBlock::new_with_pool(
            1,
            "$ ",
            "seq 400",
            None,
            &output,
            Some(0),
            &config,
            Some(12),
            None,
            None,
            cols,
            &[],
            output.len(),
            None,
            super::FinishedBlockPrecomputed {
                output_rows: Some(rows),
            },
        );
        let calls = super::OUTPUT_VISUAL_ROW_COUNT_CALLS.with(Cell::get);

        // The command line is still measured (it is short); the transcript is
        // not. Before this change the same construction walked the output once
        // more inside the height estimate.
        assert!(
            calls <= 1,
            "constructing a precomputed card walked the transcript {calls} times"
        );
    }

    /// Opening a card's filter row grabs focus into its entry. Until the entry
    /// answered a key itself, the only way back out was the mouse: Escape was
    /// inert and Alt+Shift+F lives on the pane's live-VTE controller, which a
    /// focused entry never reaches.
    #[test]
    fn the_filter_entry_answers_only_the_two_keys_that_close_it() {
        use gtk4::gdk::{Key, ModifierType};

        let alt_shift = ModifierType::ALT_MASK | ModifierType::SHIFT_MASK;
        assert!(super::filter_row_key_closes(
            Key::Escape,
            ModifierType::empty()
        ));
        assert!(super::filter_row_key_closes(Key::f, alt_shift));
        assert!(super::filter_row_key_closes(Key::F, alt_shift));

        // Typing, and every chord the entry itself owns, stays with the entry.
        assert!(!super::filter_row_key_closes(Key::f, ModifierType::empty()));
        assert!(!super::filter_row_key_closes(
            Key::a,
            ModifierType::CONTROL_MASK
        ));
        assert!(!super::filter_row_key_closes(
            Key::f,
            ModifierType::ALT_MASK
        ));
        assert!(!super::filter_row_key_closes(
            Key::f,
            alt_shift | ModifierType::CONTROL_MASK
        ));
        assert!(!super::filter_row_key_closes(
            Key::Escape,
            ModifierType::CONTROL_MASK
        ));
        assert!(!super::filter_row_key_closes(
            Key::Return,
            ModifierType::empty()
        ));
    }

    #[test]
    fn retention_plan_accepts_an_exact_byte_limit() {
        let plan = completed_block_retention_plan(&[(11, 40), (12, 60)], 10, 100);

        assert_eq!(plan.evict_prefix, 0);
        assert_eq!(plan.retained_count, 2);
        assert_eq!(plan.retained_estimated_bytes, 100);
        assert_eq!(plan.byte_budget_evictions, 0);
        assert!(!plan.newest_exceeds_byte_budget);
    }

    #[test]
    fn retention_plan_evicts_oldest_when_one_byte_over_limit() {
        let plan = completed_block_retention_plan(&[(11, 41), (12, 60)], 10, 100);

        assert_eq!(plan.evict_prefix, 1);
        assert_eq!(plan.retained_count, 1);
        assert_eq!(plan.retained_estimated_bytes, 60);
        assert_eq!(plan.byte_budget_evictions, 1);
        assert!(!plan.newest_exceeds_byte_budget);
    }

    #[test]
    fn retention_plan_keeps_a_huge_newest_block() {
        let plan = completed_block_retention_plan(&[(99, 101)], 10, 100);

        assert_eq!(plan.evict_prefix, 0);
        assert_eq!(plan.retained_count, 1);
        assert_eq!(plan.retained_estimated_bytes, 101);
        assert_eq!(plan.byte_budget_evictions, 0);
        assert!(plan.newest_exceeds_byte_budget);
    }

    #[test]
    fn retention_plan_enforces_count_without_claiming_a_byte_eviction() {
        let plan = completed_block_retention_plan(&[(1, 10), (2, 10), (3, 10)], 2, 100);

        assert_eq!(plan.evict_prefix, 1);
        assert_eq!(plan.retained_count, 2);
        assert_eq!(plan.retained_estimated_bytes, 20);
        assert_eq!(plan.byte_budget_evictions, 0);
        assert!(!plan.newest_exceeds_byte_budget);
    }

    #[test]
    fn retained_estimate_charges_actual_raw_ansi_length() {
        let plain = estimated_completed_block_retained_bytes(2, 3, 0, 12, 5, 5, 5, 4, 0);
        let escape_heavy = estimated_completed_block_retained_bytes(2, 3, 0, 12, 500, 5, 5, 4, 0);

        assert_eq!(escape_heavy - plain, 2_315);
    }

    #[test]
    fn retained_estimate_charges_cursor_expanded_materialized_and_plain_output() {
        let raw_only = estimated_completed_block_retained_bytes(0, 0, 0, 0, 100, 100, 100, 0, 0);
        let expanded =
            estimated_completed_block_retained_bytes(0, 0, 0, 0, 100, 10_000, 20_000, 0, 0);

        assert_eq!(expanded - raw_only, 706_300);
    }

    #[test]
    fn retained_estimate_charges_bounded_vte_cells_not_only_utf8_bytes() {
        let eight_mib = 8 * 1024 * 1024;
        let retained = estimated_completed_block_retained_bytes(
            0, 0, 0, 0, eight_mib, eight_mib, eight_mib, 0, 0,
        );

        assert!(retained > eight_mib * super::ORIGINAL_OUTPUT_RETENTION_EQUIVALENT);
        assert!(retained < super::MAX_COMPLETED_BLOCK_RETAINED_BYTES);
    }

    #[test]
    fn terminal_grid_estimate_charges_tabs_to_the_right_margin() {
        assert_eq!(terminal_grid_units_upper_bound(b"\tX", 80), 81);
        assert_eq!(terminal_grid_units_upper_bound(b"\t\t", 80), 160);
        assert_eq!(terminal_grid_units_upper_bound(b"abc", 80), 3);
    }

    #[test]
    fn restored_legacy_block_uses_safe_tab_width_when_cols_are_unknown() {
        let mut block = block_with_exit(Some(0));
        block.output = "\tX".repeat(128);
        block.cols = 80;
        let known_width = block.estimated_restored_retained_bytes();
        block.cols = 0;
        let legacy_unknown_width = block.estimated_restored_retained_bytes();

        assert!(legacy_unknown_width >= known_width);
        assert!(legacy_unknown_width < super::MAX_COMPLETED_BLOCK_RETAINED_BYTES);
    }

    #[test]
    fn retention_plan_treats_addition_overflow_as_over_budget() {
        let plan = completed_block_retention_plan(&[(1, usize::MAX), (2, 1)], 2, usize::MAX);

        assert_eq!(plan.evict_prefix, 1);
        assert_eq!(plan.retained_count, 1);
        assert_eq!(plan.retained_estimated_bytes, 1);
        assert_eq!(plan.byte_budget_evictions, 1);
    }

    #[test]
    fn alt_screen_exit_never_restores_stale_organism_visibility() {
        let entered = live_organism_alt_transition(true, true);
        assert_eq!(entered, (false, true));
        assert!(!live_organism_is_visible(entered.0, entered.1));

        let exited = live_organism_alt_transition(entered.0, false);
        assert_eq!(exited, (false, false));
        assert!(!live_organism_is_visible(exited.0, exited.1));

        // A post-rmcup geometry pass may explicitly opt in again.
        assert!(live_organism_is_visible(true, exited.1));
    }

    fn block_with_exit(exit_code: Option<i32>) -> BlockData {
        BlockData {
            id: 1,
            prompt: String::new(),
            cmd: "cargo test".to_string(),
            cmd_markup: None,
            output: "running".to_string(),
            exit_code,
            lifecycle_schema: super::BLOCK_LIFECYCLE_SCHEMA,
            completion_provenance: super::CompletionProvenance::ShellReported.into(),
            start_mark_seen: true,
            estimated_height: 0,
            line_count: 1,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            cols: 80,
            command_exact: false,
            command_truncated: false,
        }
    }

    /// The presentation rule this round adds: a command whose exit status the
    /// shell never reported is its own outcome, not a success.
    #[test]
    fn an_unreported_exit_status_is_neither_success_nor_failure() {
        assert_eq!(
            BlockOutcome::classify(Some("cargo test"), None),
            BlockOutcome::Unknown,
            "an unknown status used to arrive here as Some(0), i.e. as a success"
        );
        assert_eq!(
            BlockOutcome::classify(Some("cargo test"), Some(0)),
            BlockOutcome::Success
        );
        assert_eq!(
            BlockOutcome::classify(Some("cargo test"), Some(1)),
            BlockOutcome::Failure(1)
        );
        // 130 used to arrive here as Failure(130). It is now its own outcome —
        // see `interrupts_are_not_failures`.
        assert_eq!(
            BlockOutcome::classify(Some("cargo test"), Some(130)),
            BlockOutcome::Interrupted(130)
        );
        // Background output belongs to no command, so it keeps its own look
        // whatever status is attached.
        assert_eq!(BlockOutcome::classify(None, None), BlockOutcome::Background);
        assert_eq!(
            BlockOutcome::classify(Some(""), Some(1)),
            BlockOutcome::Background
        );
        assert_eq!(
            BlockOutcome::classify(None, Some(127)),
            BlockOutcome::Background,
            "a raw non-zero status cannot turn background output into a failure"
        );
    }

    /// Stopping a command is not the same as a command going wrong. Forge
    /// produces the case itself: the live card's Stop button writes `\x03`, so
    /// every command a user stops through the UI came back as a red card with
    /// an `exit:130 SIGINT` badge and a scrollbar failure tick.
    #[test]
    fn interrupts_are_not_failures() {
        for (code, signal) in [(130, "SIGINT"), (141, "SIGPIPE"), (143, "SIGTERM")] {
            let outcome = BlockOutcome::classify(Some("tail -f log"), Some(code));
            assert_eq!(
                outcome,
                BlockOutcome::Interrupted(code),
                "{signal} must not read as a command failure"
            );
            assert!(!outcome.is_failure());
            assert_eq!(
                outcome.reported_exit_code(),
                Some(code),
                "the raw status stays available for an exact-code filter"
            );
            assert_ne!(outcome.stripe_css_class(), "block-failed");
            assert_ne!(outcome.status_css_class(), "block-status-bad");
        }

        // Faults stay red: these are things the user needs to see.
        for code in [
            1, 2, 127, 131, /* SIGQUIT */
            134, /* SIGABRT */
            137, /* SIGKILL */
            139, /* SIGSEGV */
        ] {
            let outcome = BlockOutcome::classify(Some("./crash"), Some(code));
            assert_eq!(
                outcome,
                BlockOutcome::Failure(code),
                "exit {code} is a real failure"
            );
            assert!(outcome.is_failure());
        }
    }

    /// The pool clears stripe classes by list; the list must cover every value
    /// the outcome can actually produce, or a recycled card wears two stripes.
    #[test]
    fn every_outcome_stripe_class_is_cleared_by_the_pool() {
        for outcome in [
            BlockOutcome::Background,
            BlockOutcome::Success,
            BlockOutcome::Failure(1),
            BlockOutcome::Interrupted(130),
            BlockOutcome::Unknown,
        ] {
            assert!(
                BlockOutcome::STRIPE_CSS_CLASSES.contains(&outcome.stripe_css_class()),
                "{outcome:?} uses a stripe class the pool never removes"
            );
        }
    }

    #[test]
    fn exported_markdown_says_unknown_instead_of_zero() {
        let unknown = block_with_exit(None).to_markdown();
        assert!(unknown.contains("**Exit Code:** unknown"), "{unknown}");
        assert!(block_with_exit(Some(0))
            .to_markdown()
            .contains("**Exit Code:** 0"));
    }

    #[test]
    fn block_json_uses_shared_lifecycle_vocabulary_and_background_omits_it() {
        let mut inferred = block_with_exit(None);
        inferred.completion_provenance = super::CompletionProvenance::BoundaryInferred.into();
        inferred.start_mark_seen = true;
        let json: serde_json::Value = serde_json::from_str(&inferred.to_json()).unwrap();
        assert_eq!(json["completion_provenance"], "boundary_inferred");
        assert_eq!(json["lifecycle_health"], "degraded");

        inferred.cmd.clear();
        let json: serde_json::Value = serde_json::from_str(&inferred.to_json()).unwrap();
        assert!(json.get("completion_provenance").is_none());
        assert!(json.get("start_mark_seen").is_none());
        assert!(json.get("lifecycle_health").is_none());
    }

    /// The `i32`-only shared surfaces (command-history JSONL, AI block context,
    /// jagent observation) must receive something that cannot be read as a
    /// success, plus the note that explains it.
    #[test]
    fn shared_surfaces_get_a_sentinel_and_a_note_for_an_unknown_status() {
        assert_eq!(exit_code_for_shared_surface(Some(7)), (7, None));
        assert_eq!(
            exit_code_for_shared_surface(None),
            (UNKNOWN_EXIT_SENTINEL, Some(UNKNOWN_EXIT_NOTE))
        );
        assert!(
            !(0..=255).contains(&UNKNOWN_EXIT_SENTINEL),
            "the sentinel must not collide with a real wait status"
        );
    }

    #[test]
    fn snapshot_feed_wipes_previous_content_in_stream() {
        // Regression: feed() applies asynchronously while reset() acts
        // immediately, so when GTK maps a card several times in one main-loop
        // turn every queued snapshot survives its following reset and the
        // copies concatenate ("/home/tester/home/tester…"). The wipe must travel
        // inside the fed byte stream, ordered before the snapshot it protects.
        let stream = super::finished_snapshot_stream("/home/tester");
        assert!(stream.starts_with(super::FINISHED_SNAPSHOT_CLEAR));
        assert_eq!(
            &stream[super::FINISHED_SNAPSHOT_CLEAR.len()..],
            b"/home/tester"
        );
        // The clear must reach cursor home, screen, and scrollback — dropping
        // any of the three reintroduces stacked copies on remap bursts.
        let clear = std::str::from_utf8(super::FINISHED_SNAPSHOT_CLEAR).unwrap();
        for required in ["\x1b[H", "\x1b[2J", "\x1b[3J"] {
            assert!(clear.contains(required), "missing {required:?}");
        }
    }

    #[test]
    fn cap_changes_that_do_not_change_the_picture_skip_a_refeed() {
        assert_eq!(
            super::output_render_stamp(137, 3, 3, 0),
            super::output_render_stamp(137, 3, 24, 0),
        );

        let base = super::output_render_stamp(137, 40, 24, 0);
        assert_ne!(base, super::output_render_stamp(135, 40, 24, 0));
        assert_ne!(base, super::output_render_stamp(137, 40, 12, 0));
        assert_ne!(base, super::output_render_stamp(137, 40, 40, 0));
        assert_ne!(base, super::output_render_stamp(137, 40, 24, 1));
    }

    #[test]
    fn only_a_content_change_earns_a_re_feed() {
        let base = super::output_render_stamp(137, 40, 24, 0);
        // Cap moved: for a long block this changes the stamp (the visible rows
        // ARE the cap), but the ring already holds the right bytes.
        assert!(!super::stamp_change_needs_refeed(
            base,
            super::output_render_stamp(137, 40, 12, 0)
        ));
        assert!(!super::stamp_change_needs_refeed(
            base,
            super::output_render_stamp(137, 40, 200, 0)
        ));
        // Different wrap width: the ring's line breaks are wrong.
        assert!(super::stamp_change_needs_refeed(
            base,
            super::output_render_stamp(135, 40, 24, 0)
        ));
        // Different displayed text (a filter was applied).
        assert!(super::stamp_change_needs_refeed(
            base,
            super::output_render_stamp(137, 40, 24, 1)
        ));
        // The construction-time zero stamp always feeds.
        assert!(super::stamp_change_needs_refeed((0, 0, false, 0), base));
    }
    #[test]
    fn duration_badge_keeps_seconds_past_the_minute_mark() {
        use super::format_block_duration;
        assert_eq!(format_block_duration(250), "250ms");
        assert_eq!(format_block_duration(2500), "2.5s");
        assert_eq!(format_block_duration(59_940), "59.9s");
        assert_eq!(format_block_duration(60_000), "1m");
        assert_eq!(format_block_duration(61_000), "1m01s");
        assert_eq!(format_block_duration(179_000), "2m59s");
        assert_eq!(format_block_duration(3_600_000), "1h");
        assert_eq!(format_block_duration(3_840_000), "1h04m");
    }

    #[test]
    fn civil_from_days_round_trips_known_dates() {
        use super::civil_from_days;
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn restored_blocks_from_earlier_days_carry_a_date_label() {
        use super::format_block_timestamp;
        // Same local day: wall-clock only.
        let (label, tooltip) = format_block_timestamp(0, 0, 0);
        assert_eq!(label, "00:00:00");
        assert_eq!(tooltip, "1970-01-01 00:00:00");
        // Viewed a day later: the label must expose the date.
        let (label, _) = format_block_timestamp(0, 86_400_000, 0);
        assert_eq!(label, "01-01 00:00");
        // A negative zone offset can move the local date behind UTC.
        let (label, tooltip) = format_block_timestamp(3_600_000, 90_000_000, -7200);
        assert_eq!(label, "12-31 23:00");
        assert_eq!(tooltip, "1969-12-31 23:00:00");
    }

    #[test]
    fn whole_block_copy_preserves_terminal_grouping() {
        assert_eq!(block_clipboard_text("echo ok", "ok", false), "echo ok\nok");
        assert_eq!(block_clipboard_text("echo ok", "ok", true), "ok");
        assert_eq!(block_clipboard_text("pwd", "", false), "pwd");
    }

    /// Recall is insertion-only: no readline/zle binding is a portable
    /// whole-buffer clear. Multiline text is framed only when the shell can
    /// strip the bracketed-paste markers.
    #[test]
    fn multiline_recall_uses_full_text_with_bracketed_paste() {
        let paste = super::build_command_recall("printf one\nprintf two\n", true);
        assert_eq!(paste.echo_text, "printf one\nprintf two");
        assert_eq!(
            paste.bytes,
            b"\x1b[200~printf one\nprintf two\x1b[201~".to_vec()
        );
        assert!(!paste.risk.truncated_to_first_line);
    }

    #[test]
    fn multiline_recall_falls_back_without_bracketed_paste() {
        let paste = super::build_command_recall("printf one\nprintf two", false);
        assert_eq!(paste.echo_text, "printf one");
        assert_eq!(paste.bytes, b"printf one".to_vec());
        assert!(paste.risk.truncated_to_first_line);
    }

    /// The injection this round fixes: a recalled command carrying an embedded
    /// frame terminator must never reach the PTY with the terminator intact.
    #[test]
    fn recall_strips_an_embedded_paste_terminator() {
        let paste = super::build_command_recall("docs\x1b[201~\rrm -rf ~", true);
        assert!(paste.risk.had_embedded_paste_marker);
        let terminators = paste
            .bytes
            .windows(b"\x1b[201~".len())
            .filter(|window| *window == b"\x1b[201~")
            .count();
        assert_eq!(
            terminators,
            1,
            "only the closing frame may carry a terminator: {:?}",
            String::from_utf8_lossy(&paste.bytes)
        );
        assert!(paste.bytes.ends_with(b"\x1b[201~"));
        assert_eq!(paste.echo_text, "docs\nrm -rf ~");
    }

    #[test]
    fn terminalize_command_line_breaks_return_to_the_command_column() {
        assert_eq!(
            terminalize_line_breaks(b"cd /tmp\npython3 demo.py"),
            b"cd /tmp\r\npython3 demo.py"
        );
        assert_eq!(
            terminalize_line_breaks(b"\x1b[36mrun\x1b[0m\r\nnext"),
            b"\x1b[36mrun\x1b[0m\r\nnext"
        );
    }

    #[test]
    fn filter_output_lines_matches_ascii_case_insensitive() {
        assert_eq!(
            filter_output_lines("alpha\nERROR: nope\nomega", "error", false, false, false, 0)
                .unwrap(),
            "ERROR: nope"
        );
    }

    #[test]
    fn filter_output_lines_preserves_unicode_case_insensitive_search() {
        assert_eq!(
            filter_output_lines("alpha\n你好世界\nomega", "你好", false, false, false, 0).unwrap(),
            "你好世界"
        );
    }

    #[test]
    fn filter_output_lines_reports_invalid_regex() {
        assert!(filter_output_lines("alpha", "[", true, false, false, 0).is_err());
    }

    #[test]
    fn display_override_lifecycle_preserves_filter_and_restores_full_output() {
        let full = "alpha\n\x1b[31mERROR: nope\x1b[0m\nomega";
        let mut display_override = None;
        assert_eq!(
            super::resolved_finished_output(full, &display_override),
            full
        );

        let filtered = filter_output_lines(full, "ERROR", false, true, false, 0).unwrap();
        display_override = super::filtered_output_override(full, filtered);
        assert_eq!(
            super::resolved_finished_output(full, &display_override),
            "\x1b[31mERROR: nope\x1b[0m"
        );

        display_override = super::filtered_output_override(full, full.to_string());
        assert!(display_override.is_none());
        assert_eq!(
            super::resolved_finished_output(full, &display_override),
            full
        );
    }

    #[test]
    #[ignore = "manual 8 MiB retained-allocation comparison"]
    fn default_display_override_drops_an_eight_mib_duplicate() {
        const OUTPUT_BYTES: usize = 8 * 1024 * 1024;
        let full = "x".repeat(OUTPUT_BYTES);
        let legacy_duplicate = full.clone();
        let legacy_retained_capacity = full.capacity().saturating_add(legacy_duplicate.capacity());

        let display_override: Option<String> = None;
        let optimized_retained_capacity = full.capacity()
            + display_override
                .as_ref()
                .map_or(0, |output| output.capacity());
        eprintln!(
            "8 MiB display retention: legacy={legacy_retained_capacity} bytes, \
             override={optimized_retained_capacity} bytes, saved={} bytes",
            legacy_retained_capacity.saturating_sub(optimized_retained_capacity)
        );

        assert_eq!(
            super::resolved_finished_output(&full, &display_override).len(),
            OUTPUT_BYTES
        );
        assert!(
            legacy_retained_capacity >= optimized_retained_capacity.saturating_add(OUTPUT_BYTES),
            "legacy={legacy_retained_capacity} optimized={optimized_retained_capacity}"
        );
        std::hint::black_box(legacy_duplicate);
    }

    #[test]
    fn collapsed_summary_uses_singular_and_plural_line_counts() {
        assert_eq!(
            collapsed_output_summary(1),
            "▸ 1 line hidden — click to show"
        );
        assert_eq!(
            collapsed_output_summary(42),
            "▸ 42 lines hidden — click to show"
        );
    }

    #[test]
    fn visual_row_count_includes_terminal_wrapping() {
        assert_eq!(super::output_visual_row_count("123456789\nabc", 4), 4);
        assert_eq!(super::output_visual_row_count("界界界", 4), 2);
    }

    #[test]
    fn long_output_cap_fills_space_above_compact_input() {
        assert_eq!(
            super::fitted_output_rows_for_viewport(Some(60), 30, 200),
            51
        );
        assert_eq!(super::fitted_output_rows_for_viewport(Some(60), 30, 40), 40);
        assert_eq!(super::fitted_output_rows_for_viewport(None, 30, 200), 30);
        assert_eq!(super::fitted_output_rows_for_viewport(Some(8), 30, 200), 3);
    }

    #[test]
    fn long_output_scrolls_inside_its_own_block() {
        // Taller than the pane: capped, so the block keeps a private scrollbar.
        assert_eq!(super::finished_output_cap(200, 30, false, 5_000), 30);
        assert!(!super::output_fits_viewport(200, 30));
        // A normal expansion can still show the complete snapshot.
        assert_eq!(super::finished_output_cap(200, 30, true, 5_000), 200);
        assert!(super::output_fits_viewport(200, 200));
        // Extremely long snapshots stop at the configured expanded ceiling and
        // keep their private scrollbar instead of creating an unbounded card.
        assert_eq!(super::finished_output_cap(20_000, 30, true, 5_000), 5_000);
        assert!(!super::output_fits_viewport(20_000, 5_000));
    }

    #[test]
    fn card_height_follows_visible_rows() {
        // The re-fit path reports a card height from VTE's measured cell
        // height, the virtualization estimate from the configured font. Both
        // go through this formula: if they diverge, a resize shifts every
        // block below it in the virtualized document.
        assert_eq!(
            super::finished_block_height_for_rows(20, 1, 10, false),
            12 * 20 + 34
        );
        assert_eq!(
            super::finished_block_height_for_rows(20, 1, 1, false),
            3 * 20 + 34
        );
        assert_eq!(
            super::finished_block_height_for_rows(20, 1, 1, true),
            3 * 20 + 24
        );
        assert_eq!(
            super::finished_block_height_for_rows(0, 1, 1, false),
            3 + 34
        );
    }

    #[test]
    fn card_height_omits_rows_for_hidden_surfaces() {
        let background = super::finished_block_height_for_rows(20, 0, 1, false);
        let command_with_output = super::finished_block_height_for_rows(20, 1, 1, false);
        let command_without_output = super::finished_block_height_for_rows(20, 1, 0, false);

        assert_eq!(background, 2 * 20 + 34);
        assert_eq!(command_without_output, 2 * 20 + 34);
        assert_eq!(command_with_output - background, 20);
    }

    #[test]
    fn estimated_background_height_does_not_reserve_a_command_row() {
        let (config, _, _) = crate::config::load_safe_config();
        let cell_height = super::estimated_cell_height_px(&config);
        let background =
            super::estimated_finished_block_height_for_text(&config, "", "one line", 80);
        let command =
            super::estimated_finished_block_height_for_text(&config, "printf one", "one line", 80);
        let command_without_output =
            super::estimated_finished_block_height_for_text(&config, "cd /tmp", "", 80);

        assert_eq!(command - background, cell_height);
        assert_eq!(command_without_output, background);
    }

    #[test]
    fn short_output_takes_its_natural_height() {
        assert_eq!(super::finished_output_cap(12, 30, false, 5_000), 12);
        assert!(super::output_fits_viewport(12, 12));
    }

    #[test]
    fn render_cols_follow_a_pane_narrower_than_the_recorded_width() {
        // Recorded at 46 cols, pane allocates 31 cols' worth of pixels: row
        // and height math must use 31 or the post-feed settle pass disagrees
        // with the requested height — the narrow-pane two-frame flicker.
        assert_eq!(super::clamp_render_cols(46, 31 * 10, 10), 31);
        // Pane at least as wide as the recorded width keeps the recorded
        // columns so restored output preserves its original line breaks.
        assert_eq!(super::clamp_render_cols(46, 80 * 10, 10), 46);
        assert_eq!(super::clamp_render_cols(46, 46 * 10, 10), 46);
        // No allocation yet (first map) or no font metrics: fall back to the
        // recorded columns; the settle pass corrects any residue.
        assert_eq!(super::clamp_render_cols(46, 0, 10), 46);
        assert_eq!(super::clamp_render_cols(46, 310, 0), 46);
        // VTE's grid never drops below two columns.
        assert_eq!(super::clamp_render_cols(46, 5, 10), 2);
    }

    #[test]
    fn narrow_pane_wraps_wide_glyph_rows_like_vte() {
        // 10 double-width CJK glyphs: terminal width 20 cells. The narrow-pane
        // flicker reproduced with exactly this kind of content — row math must
        // count cells, not chars, at the clamped column width.
        let line = "已最新已最新已最新已";
        assert_eq!(super::output_visual_row_count(line, 31), 1);
        assert_eq!(super::output_visual_row_count(line, 12), 2);
        assert_eq!(super::output_visual_row_count(line, 4), 5);
    }

    #[test]
    fn narrowed_render_cols_grow_the_row_count_the_height_must_follow() {
        // The flicker scenario end to end: a 40-column line recorded at 46
        // cols fits one row; the same snapshot in a pane allocating 31
        // columns needs two. Row math must use the clamped columns or the
        // requested height disagrees with what VTE renders after allocation —
        // the two frames the oscillation alternated between.
        let line = "x".repeat(40);
        let recorded = 46;
        let clamped = super::clamp_render_cols(recorded, 31 * 10, 10);
        assert_eq!(clamped, 31);
        assert_eq!(super::output_visual_row_count(&line, recorded), 1);
        assert_eq!(super::output_visual_row_count(&line, clamped), 2);
    }

    #[test]
    fn visual_row_count_ignores_ansi_and_overwritten_progress_rows() {
        let apt_like = concat!(
            "\r0% [Working]",
            "\r\x1b[K\x1b[32mHit:1 repo\x1b[0m\r\n",
            "\r50% [Working]",
            "\r\x1b[KDone\r\n",
        );
        assert_eq!(super::output_visual_row_count(apt_like, 20), 2);
    }

    #[test]
    fn plain_visual_row_fast_path_matches_control_replay() {
        for text in [
            "plain output\nsecond line\n",
            "tabs\tand wide 界🙂 glyphs\n",
            "combining e\u{301} and nul \0 stay byte-identical\n",
        ] {
            assert!(memchr::memchr3(0x1b, b'\r', b'\x08', text.as_bytes()).is_none());
            let forced_replay = format!("\x1b[0m{text}");
            for cols in [4, 31, 80] {
                assert_eq!(
                    super::output_visual_row_count(text, cols),
                    super::output_visual_row_count(&forced_replay, cols),
                );
            }
        }
    }

    #[test]
    fn visual_row_cache_hits_same_key_and_refreshes_generation_or_columns() {
        let text = "123456789\n界界界";
        let cache = Cell::new(Some(super::OutputVisualRowsCacheEntry {
            effective_cols: 4,
            displayed_generation: 7,
            rows: 6,
        }));
        reset_visual_row_cache_counters();
        super::OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| calls.set(0));

        assert_eq!(super::cached_output_visual_row_count(&cache, text, 4, 7), 6);
        assert_eq!(visual_row_cache_counters(), (1, 0));
        super::OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| assert_eq!(calls.get(), 0));

        let generation_rows = super::cached_output_visual_row_count(&cache, text, 4, 8);
        assert_eq!(generation_rows, super::output_visual_row_count(text, 4));
        assert_eq!(visual_row_cache_counters(), (1, 1));

        reset_visual_row_cache_counters();
        super::OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| calls.set(0));
        assert_eq!(
            super::cached_output_visual_row_count(&cache, "ignored on a cache hit", 4, 8),
            generation_rows,
        );
        assert_eq!(visual_row_cache_counters(), (1, 0));
        super::OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| assert_eq!(calls.get(), 0));

        let narrower_rows = super::cached_output_visual_row_count(&cache, text, 2, 8);
        assert!(narrower_rows > generation_rows);
        assert_eq!(visual_row_cache_counters(), (1, 1));
        super::OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| assert_eq!(calls.get(), 1));
    }

    #[test]
    fn displayed_generation_wrap_invalidates_generation_zero_cache_entry() {
        let generation = Cell::new(u64::MAX);
        let cache = Cell::new(Some(super::OutputVisualRowsCacheEntry {
            effective_cols: 2,
            displayed_generation: 0,
            rows: 999,
        }));

        let wrapped = super::advance_displayed_generation(&generation, &cache);
        assert_eq!(wrapped, 0);
        assert_eq!(cache.get(), None);
        assert_eq!(
            super::cached_output_visual_row_count(&cache, "abcd", 2, wrapped),
            2
        );
    }

    /// Run with:
    /// `cargo test --release finished_output_visual_rows_cache_microbenchmark -- --ignored --nocapture`
    #[test]
    #[ignore = "manual microbenchmark"]
    fn finished_output_visual_rows_cache_microbenchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        const REMAPS: usize = 100;
        const PATTERN: &str = "plain terminal output with wide 界 glyphs and tabs\t0123456789\n";

        for target_bytes in [1usize << 20, 8usize << 20] {
            let text = PATTERN.repeat(target_bytes.div_ceil(PATTERN.len()));
            let generation = 19;
            let cols = 80;
            let initial_rows = super::output_visual_row_count(&text, cols);
            let cache = Cell::new(None);

            reset_visual_row_cache_counters();
            let misses_started = Instant::now();
            for _ in 0..REMAPS {
                cache.set(None);
                black_box(super::cached_output_visual_row_count(
                    black_box(&cache),
                    black_box(&text),
                    black_box(cols),
                    black_box(generation),
                ));
            }
            let misses = misses_started.elapsed();
            assert_eq!(visual_row_cache_counters(), (0, REMAPS));

            cache.set(Some(super::OutputVisualRowsCacheEntry {
                effective_cols: cols,
                displayed_generation: generation,
                rows: initial_rows,
            }));
            reset_visual_row_cache_counters();
            let hits_started = Instant::now();
            for _ in 0..REMAPS {
                black_box(super::cached_output_visual_row_count(
                    black_box(&cache),
                    black_box(&text),
                    black_box(cols),
                    black_box(generation),
                ));
            }
            let hits = hits_started.elapsed();
            assert_eq!(visual_row_cache_counters(), (REMAPS, 0));

            eprintln!(
                "finished-output remap {} MiB x {REMAPS}: cache-miss={misses:?}, \
                 cache-hit={hits:?}, speedup={:.1}x",
                target_bytes >> 20,
                misses.as_secs_f64() / hits.as_secs_f64(),
            );
        }
    }

    #[test]
    fn filter_output_lines_includes_context_without_extra_alloc_join() {
        assert_eq!(
            filter_output_lines("one\ntwo\nthree\nfour", "three", false, true, false, 1).unwrap(),
            "two\nthree\nfour"
        );
    }

    /// TEMP diagnostic harness (needs a display): renders four identical short
    /// `ls`-style blocks and dumps each output VTE's geometry so the
    /// spurious-scrollbar / inconsistent-rows bug can be observed directly.
    #[test]
    #[ignore = "diagnostic; requires DISPLAY"]
    fn diag_short_ls_block_geometry() {
        use gtk4::prelude::*;
        use vte4::TerminalExt;

        gtk4::init().expect("gtk init");
        let (mut config, _, _) = crate::config::load_config();
        // Pin the font: every row count below is really a column count, and the
        // column count comes from the cell width. A different default font — or
        // a developer's own — rewraps the fixture and moves the geometry this
        // harness exists to observe.
        config.font_desc = "Monospace 14".to_string();

        // Synthetic `ls -C` capture: 6 colored rows, leading + trailing CRLF,
        // as the PTY delivers them.
        let ls = "\r\n\
            Cargo.lock           deny.toml   Makefile             \x1b[01;34msrc\x1b[0m\r\n\
            Cargo.toml           \x1b[01;34mdocs\x1b[0m        \x1b[01;34mpackaging\x1b[0m            \x1b[01;34mtarget\x1b[0m\r\n\
            CHANGELOG.md         flake.lock  README.md            \x1b[01;34mtests\x1b[0m\r\n\
            config.toml.example  flake.nix   rust-toolchain.toml\r\n\
            CONTRIBUTING.md      LICENSE-APACHE  \x1b[01;34mscripts\x1b[0m\r\n\
            \x1b[01;34mdata\x1b[0m         LICENSE-MIT     SECURITY.md\r\n";

        let win = gtk4::Window::new();
        win.set_default_size(1000, 1300);
        let scroll = gtk4::ScrolledWindow::new();
        let list = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        scroll.set_child(Some(&list));
        win.set_child(Some(&scroll));

        // Busy sibling terminal: VTE shares one process scheduler across all
        // terminals in the process, so a streaming tab (claude) can make it
        // yield mid-way through a finished block's snapshot feed.
        let busy = crate::block_view::create_active_terminal(&config);
        list.append(&busy);
        {
            let noise: String = (0..2000)
                .map(|i| format!("noise line {i} {}\r\n", "x".repeat(80)))
                .collect();
            let noise = std::rc::Rc::new(noise.into_bytes());
            let busy = busy.clone();
            gtk4::glib::timeout_add_local(std::time::Duration::from_millis(2), move || {
                busy.feed(&noise);
                gtk4::glib::ControlFlow::Continue
            });
        }

        let blocks: Vec<super::FinishedBlock> = (0..4)
            .map(|i| {
                let fb = super::FinishedBlock::new(
                    i,
                    "",
                    "ls",
                    None,
                    ls,
                    Some(0),
                    &config,
                    Some(60),
                    None,
                    None,
                    100,
                );
                list.append(fb.widget());
                fb
            })
            .collect();

        // A long output exercises the capped path: bounded viewport, inner
        // scrollbar, anchored at its first row.
        let long_text: String = (1..=120).map(|i| format!("long line {i}\r\n")).collect();
        let long_block = super::FinishedBlock::new(
            99,
            "",
            "seq 120",
            None,
            &long_text,
            Some(0),
            &config,
            Some(60),
            None,
            None,
            100,
        );
        list.append(long_block.widget());

        win.present();
        let ctx = gtk4::glib::MainContext::default();
        let pump = |ms: u64| {
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_millis(ms) {
                while ctx.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        // Simulate the race that produced the 2-row cards: measure each block
        // mid-feed (single loop iterations between fits, so VTE may not have
        // applied the whole snapshot yet). The tail-gated settle must recover.
        for _ in 0..6 {
            ctx.iteration(false);
            for fb in &blocks {
                crate::block_view::fit_finished_terminal_to_content(&fb.output_vte);
            }
        }
        pump(200);
        // Re-render storm: virtualization unmaps/remaps cards around every
        // insertion; each map re-feeds the snapshot while the previous feed's
        // settle idles are still queued.
        reset_visual_row_cache_counters();
        for _ in 0..3 {
            for fb in &blocks {
                fb.widget().set_visible(false);
            }
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_millis(8) {
                ctx.iteration(false);
            }
            for fb in &blocks {
                fb.widget().set_visible(true);
            }
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_millis(8) {
                ctx.iteration(false);
            }
        }
        pump(1500);
        let (cache_hits, cache_misses) = visual_row_cache_counters();
        // Hiding enough cards can remove the outer scrollbar and change the
        // effective column count. Such transitions are deliberate cache misses;
        // the pure cache test pins exact column invalidation deterministically.
        eprintln!("remap row cache: hits={cache_hits} misses={cache_misses}");
        assert!(
            cache_hits > 0,
            "stable-width legs of the remap storm must hit the row cache"
        );

        let mut violations: Vec<String> = Vec::new();
        for (i, fb) in blocks.iter().enumerate() {
            let vte = &fb.output_vte;
            let adj = vte.vadjustment().unwrap();
            eprintln!(
                "block {i}: grid={}x{} cell_h={} height_req={} alloc_h={} margins={}+{} adj: lower={} upper={} page={} value={} scrollbar_visible={}",
                vte.column_count(),
                vte.row_count(),
                vte.char_height(),
                vte.height_request(),
                vte.height(),
                vte.margin_top(),
                vte.margin_bottom(),
                adj.lower(),
                adj.upper(),
                adj.page_size(),
                adj.value(),
                fb.output_scrollbar.get_visible(),
            );
            // A 6-row snapshot must land as a 6-row card: no inner overflow
            // (spurious scrollbar), no bottom-anchored partial view.
            if vte.row_count() != 6 {
                violations.push(format!("block {i}: grid rows {}", vte.row_count()));
            }
            if adj.upper() - adj.lower() > adj.page_size() + 0.5 {
                violations.push(format!("block {i}: buffer overflows viewport"));
            }
            if (adj.value() - adj.lower()).abs() > 0.5 {
                violations.push(format!("block {i}: not anchored at top"));
            }
            if fb.output_scrollbar.get_visible() {
                violations.push(format!("block {i}: scrollbar on fitting output"));
            }
        }
        {
            let vte = &long_block.output_vte;
            let adj = vte.vadjustment().unwrap();
            eprintln!(
                "long block: grid={}x{} adj: lower={} upper={} page={} value={} scrollbar_visible={}",
                vte.column_count(),
                vte.row_count(),
                adj.lower(),
                adj.upper(),
                adj.page_size(),
                adj.value(),
                long_block.output_scrollbar.get_visible(),
            );
            if adj.upper() - adj.lower() <= adj.page_size() + 0.5 {
                violations.push("long block: expected inner overflow".into());
            }
            if (adj.value() - adj.lower()).abs() > 0.5 {
                violations.push("long block: not anchored at top".into());
            }
            if !long_block.output_scrollbar.get_visible() {
                violations.push("long block: scrollbar missing".into());
            }
        }
        win.close();
        while ctx.iteration(false) {}
        assert!(violations.is_empty(), "geometry violations: {violations:?}");
    }

    #[test]
    fn snapshot_settle_tail_finds_last_visible_line() {
        use crate::block_view::snapshot_settle_tail;
        assert_eq!(
            snapshot_settle_tail(
                "Cargo.lock  deny.toml\r\n\x1b[01;34mdata\x1b[0m  SECURITY.md\r\n"
            ),
            Some("data  SECURITY.md".to_string())
        );
        // Trailing blank lines are skipped; ANSI is stripped before matching.
        assert_eq!(
            snapshot_settle_tail("one\r\n\x1b[32mtwo\x1b[0m\r\n\r\n   \r\n"),
            Some("two".to_string())
        );
        assert_eq!(
            snapshot_settle_tail("界界界\r\n🙂 done\r\n"),
            Some("🙂 done".to_string())
        );
        assert_eq!(snapshot_settle_tail(""), None);
        assert_eq!(snapshot_settle_tail("\r\n   \r\n"), None);
    }
}
