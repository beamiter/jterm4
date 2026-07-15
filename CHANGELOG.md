# Changelog

All notable user-visible and operational changes are recorded here.

## Unreleased

### Added

- Reproducible GNOME 50 Flatpak packaging, stable desktop application ID, AppStream metadata, scalable/raster icons, checksummed CI bundles, and X11/Wayland VTE/Block smoke tests.
- A Flatpak host-command bridge so shells, SSH, Git probes, AI curl requests, and desktop notifications operate on the host instead of the application sandbox.
- Modern GTK4 `TreeListModel`/`ListView` file browser with asynchronous lazy directory scans.
- Per-window session snapshots with atomic claiming, stale-process recovery, legacy migration, retention, and doctor counts.
- Target-aware `JTERM4_LOG` / `RUST_LOG` filtering with relative timestamps and module targets.
- Cargo-or-Nix installer, safe uninstaller, Rust toolchain metadata, CODEOWNERS, contribution/security/architecture/release documentation, Dependabot, and RustSec auditing.

### Changed

- Session snapshots and Block history now use owner-only Unix permissions and durable atomic replacement.
- CI now checks maintained shell scripts and exports complete formatting diagnostics.
- Temporary round-two source-export workflows and marker files were removed.

### Security

- Persisted commands, output, working directories, and session metadata are restricted to `0700` directories and `0600` files on Unix.
- The package is marked `publish = false` until a project license is selected.
