//! file_tree — sidebar file browser for UiState.
#![allow(deprecated)]
// This module owns the current TreeView/TreeStore implementation. GTK 4.10
// deprecates it in favor of ColumnView/TreeListModel, which is a larger rewrite.

use gtk4::prelude::*;
use gtk4::{glib, TreeIter, TreePath, TreeRowReference, TreeStore, TreeView};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use vte4::TerminalExt;

use super::*;
use crate::terminal::terminal_working_directory;

// TreeStore column indices.
const COL_NAME: i32 = 0;
const COL_PATH: i32 = 1;
const COL_IS_DIR: i32 = 2;
const COL_ICON: i32 = 3;
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug)]
struct FileEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

fn scan_dir(dir: &Path) -> io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: path.is_dir(),
            path,
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

fn request_dir_scan<F>(dir: PathBuf, apply: F) -> io::Result<()>
where
    F: FnOnce(io::Result<Vec<FileEntry>>) + 'static,
{
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("jterm4-file-tree-scan".to_string())
        .spawn(move || {
            let _ = tx.send(scan_dir(&dir));
        })?;

    let mut apply = Some(apply);
    glib::timeout_add_local(SCAN_POLL_INTERVAL, move || match rx.try_recv() {
        Ok(result) => {
            if let Some(apply) = apply.take() {
                apply(result);
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => {
            if let Some(apply) = apply.take() {
                apply(Err(io::Error::other("file-tree scan worker disconnected")));
            }
            glib::ControlFlow::Break
        }
    });
    Ok(())
}

fn append_entries(store: &TreeStore, parent: Option<&TreeIter>, entries: Vec<FileEntry>) {
    for FileEntry { name, path, is_dir } in entries {
        let icon = if is_dir {
            "folder-symbolic"
        } else {
            "text-x-generic-symbolic"
        };
        let path_str = path.to_string_lossy().into_owned();
        let iter = store.insert_with_values(
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
            store.insert_with_values(
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
        let generation = self.file_tree_scan_generation.get().wrapping_add(1);
        self.file_tree_scan_generation.set(generation);
        self.file_tree_store.clear();
        self.file_tree_root_label.set_text(&display_path(&root));
        self.file_tree_root_label
            .set_tooltip_text(Some(&root.to_string_lossy()));
        *self.file_tree_root.borrow_mut() = root.clone();

        let store = self.file_tree_store.clone();
        let active_generation = self.file_tree_scan_generation.clone();
        let active_root = self.file_tree_root.clone();
        let expected_root = root.clone();
        if let Err(error) = request_dir_scan(root, move |result| {
            if active_generation.get() != generation || *active_root.borrow() != expected_root {
                return;
            }
            match result {
                Ok(entries) => append_entries(&store, None, entries),
                Err(error) => log::warn!(
                    "failed to scan file-tree root {}: {error}",
                    expected_root.display()
                ),
            }
        }) {
            log::warn!("failed to start file-tree scan: {error}");
        }
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
        let scan_in_progress: bool = self
            .file_tree_store
            .get_value(&first_child, COL_IS_DIR)
            .get()
            .unwrap_or(false);
        if scan_in_progress {
            return;
        }
        let dir_path: String = self
            .file_tree_store
            .get_value(iter, COL_PATH)
            .get()
            .unwrap_or_default();
        if dir_path.is_empty() {
            return;
        }
        let Some(row_ref) =
            TreeRowReference::new(&self.file_tree_store, &self.file_tree_store.path(iter))
        else {
            return;
        };

        // Reuse the invisible placeholder's boolean column as an in-flight bit.
        self.file_tree_store
            .set(&first_child, &[(COL_IS_DIR as u32, &true)]);
        let store = self.file_tree_store.clone();
        let active_generation = self.file_tree_scan_generation.clone();
        let generation = active_generation.get();
        let expected_path = dir_path.clone();
        if let Err(error) = request_dir_scan(PathBuf::from(dir_path), move |result| {
            if active_generation.get() != generation {
                return;
            }
            let Some(row_path) = row_ref.path() else {
                return;
            };
            let Some(parent) = store.iter(&row_path) else {
                return;
            };
            let current_path: String = store.get_value(&parent, COL_PATH).get().unwrap_or_default();
            if current_path != expected_path {
                return;
            }
            let Some(placeholder) = store.iter_children(Some(&parent)) else {
                return;
            };
            let placeholder_path: String = store
                .get_value(&placeholder, COL_PATH)
                .get()
                .unwrap_or_default();
            if !placeholder_path.is_empty() {
                return;
            }
            store.remove(&placeholder);
            match result {
                Ok(entries) => append_entries(&store, Some(&parent), entries),
                Err(error) => {
                    log::warn!("failed to scan directory {expected_path}: {error}")
                }
            }
        }) {
            self.file_tree_store
                .set(&first_child, &[(COL_IS_DIR as u32, &false)]);
            log::warn!("failed to start directory scan: {error}");
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
        let snippet = format!("{} ", shell_quote(&file_path));
        if let Some(term_view) = self.current_term_view() {
            // Block mode owns its PTY instead of attaching it to the display VTE,
            // so `Terminal::feed_child` has nowhere to write.  Route through the
            // TermView input path just like keyboard input and clipboard paste.
            term_view.write_input(snippet.as_bytes());
            term_view.grab_focus();
        } else if let Some(term) = self.current_terminal() {
            term.feed_child(snippet.as_bytes());
            term.grab_focus();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_sort_directories_first_then_by_name() {
        let mut entries = vec![
            FileEntry {
                name: "Zulu.txt".into(),
                path: PathBuf::from("Zulu.txt"),
                is_dir: false,
            },
            FileEntry {
                name: "beta".into(),
                path: PathBuf::from("beta"),
                is_dir: true,
            },
            FileEntry {
                name: "Alpha.txt".into(),
                path: PathBuf::from("Alpha.txt"),
                is_dir: false,
            },
            FileEntry {
                name: "Able".into(),
                path: PathBuf::from("Able"),
                is_dir: true,
            },
        ];

        sort_entries(&mut entries);

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["Able", "beta", "Alpha.txt", "Zulu.txt"]);
    }
}
