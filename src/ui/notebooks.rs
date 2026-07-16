use adw::prelude::*;
use libadwaita as adw;
use std::path::Path;

use super::UiState;

impl UiState {
    pub(crate) fn open_notebook(&self, path: &Path) {
        if std::env::var_os("JTERM4_SAFE_MODE").is_some() {
            self.show_notebook_error("Executable notebooks are disabled in safe mode.");
            return;
        }
        let cwd = self
            .current_terminal()
            .as_ref()
            .and_then(crate::terminal::terminal_working_directory)
            .map(std::path::PathBuf::from);
        let shell = self.shell_argv.borrow().clone();
        if let Err(error) =
            crate::notebook::NotebookDialog::open(&self.window, path, &shell, cwd.as_deref())
        {
            self.show_notebook_error(&error.to_string());
        }
    }

    pub(crate) fn open_welcome_notebook(&self) {
        match crate::workflows::welcome_notebook_path() {
            Some(path) => self.open_notebook(&path),
            None => self.show_notebook_error(
                "The welcome notebook is not installed. Set JTERM4_ASSET_DIR or reinstall jterm4.",
            ),
        }
    }

    fn show_notebook_error(&self, message: &str) {
        let dialog = adw::AlertDialog::new(Some("Cannot open notebook"), Some(message));
        dialog.add_response("ok", "OK");
        dialog.set_default_response(Some("ok"));
        dialog.present(Some(&self.window));
    }
}
