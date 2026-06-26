//! export — extracted from block_view (mechanical split, no logic changes)
//!
//! Serializes finished blocks to JSON / Markdown for the user-facing export
//! actions, plus a clipboard-copy helper for the per-block right-click menu.
//! Reads only the in-memory `block_data` and `finished_blocks` snapshots; no
//! VTE state mutation.

use gtk4::prelude::*;

use super::{strip_ansi, BlockData, TermView};

#[allow(dead_code)]
impl TermView {
    /// Export a block by ID to JSON format
    pub fn export_block_json(&self, block_id: u64) -> Option<String> {
        let blocks = self.block_data.borrow();
        blocks
            .iter()
            .find(|b| b.id == block_id)
            .map(|b| b.to_json())
    }

    /// Export a block by ID to Markdown format
    pub fn export_block_markdown(&self, block_id: u64) -> Option<String> {
        let blocks = self.block_data.borrow();
        blocks
            .iter()
            .find(|b| b.id == block_id)
            .map(|b| b.to_markdown())
    }

    /// Export all blocks in the session as JSON
    pub fn export_session_json(&self) -> String {
        let blocks = self.block_data.borrow();
        let blocks_vec: Vec<&BlockData> = blocks.iter().collect();
        serde_json::to_string_pretty(&blocks_vec).unwrap_or_else(|_| "[]".to_string())
    }

    /// Export all blocks in the session as Markdown
    pub fn export_session_markdown(&self) -> String {
        let blocks = self.block_data.borrow();
        let mut md = String::new();

        md.push_str("# Terminal Session Export\n\n");
        md.push_str(&format!("Total blocks: {}\n\n", blocks.len()));
        md.push_str("---\n\n");

        for (index, block) in blocks.iter().enumerate() {
            md.push_str(&format!("## Block #{}\n\n", index + 1));
            md.push_str(&block.to_markdown());
            md.push_str("\n---\n\n");
        }

        md
    }

    /// Copy a block's content to clipboard (prompt + cmd + output).
    pub fn copy_block_by_id(&self, block_id: u64) {
        let finished = self.finished_blocks.borrow();
        if let Some(block) = finished.iter().find(|b| b.id == block_id) {
            let prompt_text = block.prompt_text.clone();
            let cmd_text = block.cmd_text.clone();
            let output_text = strip_ansi(&block.full_output.borrow());

            let full_text = format!("{}\n{}\n{}", prompt_text, cmd_text, output_text);
            let clipboard = self.active_vte.clipboard();
            clipboard.set_text(&full_text);
        }
    }
}
