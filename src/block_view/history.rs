//! history — extracted from block_view (mechanical split, no logic changes)
//!
//! Persist the in-memory `block_data` deque to/from disk as length-prefixed
//! rkyv records (optional zstd). Truncate-on-save (not append) keeps the file
//! bounded, since the deque was already seeded from this file on startup.

use super::{BlockData, TermView};

#[allow(dead_code)]
impl TermView {
    /// Save block history to file (if configured).
    pub fn save_history(&self) -> std::io::Result<()> {
        use std::io::Write;

        let path_opt = self.config.borrow().block_history_path.as_ref().cloned();
        if path_opt.is_none() {
            return Ok(());
        }

        let path = path_opt.unwrap();
        let blocks = self.block_data.borrow();

        // Overwrite (truncate), do NOT append. The in-memory deque was itself
        // seeded from this file at startup, so appending it re-wrote every loaded
        // block on each session — O(N²) file growth and duplicate blocks on the
        // next load. Persisting the current capped deque keeps the file bounded.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;

        let compress = self.config.borrow().block_history_compress;

        for block in blocks.iter() {
            let serialized = rkyv::to_bytes::<_, 256>(block)
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let record: &[u8] = if compress {
                &zstd::encode_all(serialized.as_slice(), 3)
                    .map_err(|e| std::io::Error::other(e.to_string()))?
            } else {
                &serialized
            };

            // The length prefix is a u32; silently truncating it would corrupt all
            // following frame boundaries. Skip any (pathologically large) record
            // that would not fit rather than write a bad prefix.
            if record.len() > u32::MAX as usize {
                log::warn!(
                    "save_history: skipping block of {} bytes (exceeds u32 frame limit)",
                    record.len()
                );
                continue;
            }
            file.write_all(&(record.len() as u32).to_le_bytes())?;
            file.write_all(record)?;
        }

        Ok(())
    }

    /// Load block history from file (if configured).
    pub fn load_history(&self) -> std::io::Result<()> {
        let path_opt = self.config.borrow().block_history_path.as_ref().cloned();
        if path_opt.is_none() {
            return Ok(());
        }

        let path = path_opt.unwrap();
        if !std::path::Path::new(&path).exists() {
            return Ok(());
        }

        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let lazy_load_threshold = self.config.borrow().lazy_load_threshold as usize;
        let mut temp_blocks = Vec::new();

        // First pass: load all blocks into temporary storage
        loop {
            let mut len_bytes = [0u8; 4];
            if file.read_exact(&mut len_bytes).is_err() {
                break;
            }

            let len = u32::from_le_bytes(len_bytes) as usize;
            // Guard against a corrupt/misaligned length causing a giant allocation.
            const MAX_RECORD_BYTES: usize = 256 * 1024 * 1024;
            if len > MAX_RECORD_BYTES {
                log::warn!("load_history: record length {} exceeds {} — treating file as corrupt, stopping", len, MAX_RECORD_BYTES);
                break;
            }
            let mut data = vec![0u8; len];
            file.read_exact(&mut data)?;

            let decoded = if self.config.borrow().block_history_compress {
                zstd::decode_all(data.as_slice())
                    .map_err(|e| std::io::Error::other(e.to_string()))?
            } else {
                data
            };

            if let Ok(block) = rkyv::from_bytes::<BlockData>(&decoded) {
                temp_blocks.push(block);
            }
        }

        // Second pass: only load the most recent N blocks (lazy loading optimization)
        let total_loaded = temp_blocks.len();
        let start_idx = if total_loaded > lazy_load_threshold {
            log::info!("Lazy loading history: keeping {} recent blocks out of {} total (skipping {} old blocks)",
                lazy_load_threshold, total_loaded, total_loaded - lazy_load_threshold);
            total_loaded - lazy_load_threshold
        } else {
            0
        };

        let mut blocks = self.block_data.borrow_mut();
        for (idx, block) in temp_blocks.into_iter().enumerate() {
            if idx >= start_idx {
                log::debug!("Loaded historical block #{}: prompt={:?}, cmd={:?}, output_len={}, exit_code={}",
                    idx, &block.prompt, &block.cmd, block.output.len(), block.exit_code);
                blocks.push_back(block);
            }
        }

        Ok(())
    }
}
