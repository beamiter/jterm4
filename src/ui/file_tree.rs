//! file_tree — sidebar file browser for UiState.
#![allow(deprecated)]
// This module owns the current TreeView/TreeStore implementation. GTK 4.10
// deprecates it in favor of ColumnView/TreeListModel, which is a larger rewrite.

use gtk4::prelude::*;
use gtk4::{TreeIter, TreePath, TreeView};
use std::path::{Path, PathBuf};
use vte4::TerminalExt;

use super::*;
use crate::terminal::terminal_working_directory;

// TreeStore column indices.
const COL_NAME: i32 = 0;
const COL_PATH: i32 = 1;
const COL_IS_DIR: i32 = 2;
const COL_ICON: i32 = 3;

impl UiState {
    /// Set up the initial file tree root (current tab cwd, else $HOME).
    pub(crate) fn init_file_tree(&self) {
        let start = self
            .current_terminal()
            .as_ref()
            .and_then(terminal_working_directory)
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        self.set_file_tree_root(start);
    }

    /// Rebuild the tree with `root` at the top.
    pub(crate) fn set_file_tree_root(&self, root: PathBuf) {
        self.file_tree_store.clear();
        self.file_tree_root_label.set_text(&display_path(&root));
        self.file_tree_root_label
            .set_tooltip_text(Some(&root.to_string_lossy()));
        self.populate_dir(None, &root);
        *self.file_tree_root.borrow_mut() = root;
    }

    /// Jump the file tree to the active tab's working directory.
    pub(crate) fn file_tree_goto_current_cwd(&self) {
        let cwd = self
            .current_terminal()
            .as_ref()
            .and_then(terminal_working_directory)
            .map(PathBuf::from)
            .filter(|p| p.is_dir());
        match cwd {
            Some(dir) => {
                if *self.file_tree_root.borrow() != dir {
                    self.set_file_tree_root(dir);
                }
            }
            None => {
                // No reportable cwd (e.g. remote shell) — leave the tree as-is,
                // unless it was never initialized.
                if self.file_tree_root.borrow().as_os_str().is_empty() {
                    if let Some(home) = home_dir() {
                        self.set_file_tree_root(home);
                    }
                }
            }
        }
    }

    /// Move the root up to the parent directory.
    pub(crate) fn file_tree_go_up(&self) {
        let parent = self.file_tree_root.borrow().parent().map(Path::to_path_buf);
        if let Some(parent) = parent {
            self.set_file_tree_root(parent);
        }
    }

    /// Lazily fill a directory row's real children on first expansion.
    pub(crate) fn file_tree_on_expand(&self, iter: &TreeIter) {
        // A not-yet-loaded directory has a single placeholder child (empty path).
        let Some(first_child) = self.file_tree_store.iter_children(Some(iter)) else {
            return;
        };
        let child_path: String = self
            .file_tree_store
            .get_value(&first_child, COL_PATH)
            .get()
            .unwrap_or_default();
        if !child_path.is_empty() {
            return; // already populated
        }
        // Remove the placeholder, then populate the real entries.
        self.file_tree_store.remove(&first_child);
        let dir_path: String = self
            .file_tree_store
            .get_value(iter, COL_PATH)
            .get()
            .unwrap_or_default();
        if !dir_path.is_empty() {
            self.populate_dir(Some(iter), Path::new(&dir_path));
        }
    }

    /// Double-click / Enter: expand directories, insert file paths into the
    /// active terminal.
    pub(crate) fn file_tree_on_activate(&self, tv: &TreeView, path: &TreePath) {
        let Some(iter) = self.file_tree_store.iter(path) else {
            return;
        };
        let is_dir: bool = self
            .file_tree_store
            .get_value(&iter, COL_IS_DIR)
            .get()
            .unwrap_or(false);
        if is_dir {
            if tv.row_expanded(path) {
                tv.collapse_row(path);
            } else {
                tv.expand_row(path, false);
            }
            return;
        }
        let file_path: String = self
            .file_tree_store
            .get_value(&iter, COL_PATH)
            .get()
            .unwrap_or_default();
        if file_path.is_empty() {
            return;
        }
        if let Some(term) = self.current_terminal() {
            let snippet = format!("{} ", shell_quote(&file_path));
            term.feed_child(snippet.as_bytes());
            term.grab_focus();
        }
    }

    /// Insert one tree row per directory entry under `parent` (sorted: dirs
    /// first, then files, case-insensitive). Directories get a placeholder child
    /// so the expander arrow shows before they are loaded.
    fn populate_dir(&self, parent: Option<&TreeIter>, dir: &Path) {
        let mut entries: Vec<(String, PathBuf, bool)> = Vec::new();
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = path.is_dir();
                entries.push((name, path, is_dir));
            }
        }
        entries.sort_by(|a, b| {
            b.2.cmp(&a.2) // directories (true) first
                .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });

        for (name, path, is_dir) in entries {
            let icon = if is_dir {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            };
            let path_str = path.to_string_lossy().to_string();
            let iter = self.file_tree_store.insert_with_values(
                parent,
                None,
                &[
                    (COL_NAME as u32, &name),
                    (COL_PATH as u32, &path_str),
                    (COL_IS_DIR as u32, &is_dir),
                    (COL_ICON as u32, &icon),
                ],
            );
            if is_dir {
                // Placeholder child (empty path) → expander shows, loaded lazily.
                self.file_tree_store.insert_with_values(
                    Some(&iter),
                    None,
                    &[
                        (COL_NAME as u32, &""),
                        (COL_PATH as u32, &""),
                        (COL_IS_DIR as u32, &false),
                        (COL_ICON as u32, &""),
                    ],
                );
            }
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Abbreviate the home directory to `~` for the header label.
fn display_path(path: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rel.to_string_lossy());
        }
    }
    path.to_string_lossy().to_string()
}

/// Single-quote a path for safe shell insertion.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
