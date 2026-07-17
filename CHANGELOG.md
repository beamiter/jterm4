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
- Packaged OSC 133/7 shell integration for bash, zsh, fish, and PowerShell, also printable through `--shell-integration`.
- Headless JSON diagnostics, config initialization and backup restore, cwd/argv launch overrides, backend override, no-restore, and isolated safe mode.
- Metadata-only bounded JSONL command history shared by Block and VTE palettes.
- One fuzzy command palette spanning actions, history, YAML/TOML workflows, and a review-first natural-language command entry.
- Installed YAML workflow examples and multi-directory workflow precedence with both `{name}` and `{{name}}` placeholders.
- Executable `.jtnb.md` notebooks with per-cell/Run All execution, separate stdout/stderr, bounded output, and process-group cancellation.
- Provider-neutral AI for Anthropic, OpenAI-compatible endpoints, and Ollama, plus natural-language command generation and a native Block-bound Shell Agent. Its bounded multi-turn UI strictly parses JSON proposals, permits edit/reject/per-command approval, flags recognizable destructive patterns, feeds completed command results back to the model, and supports cancellation.
- Per-window AI conversation restoration with bounded, versioned snapshots and the provider-bound selected-block context.
- Foreground-process discovery and close confirmation across Block/VTE panes, split tabs, batch tab closure, zoomed layouts, and whole windows.
- Privacy-preserving `jterm4-support-bundle` diagnostics plus richer doctor checks for config permissions/backups/locks, provider readiness, workflows, Notebook assets, history, display, and remote tooling.
- Review-only workflow examples for interactive rebase, SSH port forwarding, Docker log streaming, and signaling a process by listening port.
- Deterministic relocatable Linux release archives with SHA-256 checksums, a user-local bundle installer, tag-driven release publishing, and Nix package/app/check outputs.

### Changed

- Default shortcuts now share the jterm ergonomic layout: directional Pane
  focus/resize layers, browser-style tab digits, symmetric zoom/opacity keys,
  and shell-owned `Ctrl+P` passthrough.
- Session snapshots and Block history now use owner-only Unix permissions and durable atomic replacement.
- Block is now the default terminal backend. Starting a split from Block preserves the Block leaf and creates a managed VTE sibling instead of rejecting the action.
- Runtime configuration updates propagate to Block leaves nested in pane trees.
- Pane-to-tab moves preserve stable process/session identities, tab chrome, and remote reconnect ownership across repeated primary or remote pane moves.
- Application config saves now validate syntax/semantics, serialize through an advisory lock, reject stale revisions, rotate two valid backups, and use private durable atomic replacement.
- Safe mode now constructs a fully isolated built-in VTE profile without reading user config or behavior overrides; configuration reload is disabled and save failures are visible in the UI.
- The installer, uninstaller, and Flatpak bundle now manage shell integration, workflows, and Notebook runtime assets under `share/jterm4`.
- CI now checks maintained shell scripts and exports complete formatting diagnostics.
- Notebook output transport now applies bounded backpressure, and both cancellation
  and normal interpreter exit terminate the cell process group before joining pipes.
- The AI panel now restores and persists its dragged width, has a themed empty/composer/status UI, routes focused copy/paste correctly, and uses Enter or Ctrl+Enter to send while Shift+Enter inserts a newline without stealing IME candidate confirmation.
- Temporary round-two source-export workflows and marker files were removed.

### Security

- Persisted commands, output, working directories, and session metadata are restricted to `0700` directories and `0600` files on Unix.
- AI credential contents remain outside `config.toml`: environment variables take priority, with an optional owner-only `ai_api_key_file` fallback; safe mode disables AI/Agent, executable notebooks, history, remote hosts, restoration, and persistence.
- Completed AI chat pairs and their provider-bound Block context (redacted when configured) share the bounded, owner-only, atomically replaced per-window snapshot; in-flight requests are never restored as completed replies.
- AI/Agent command proposals never submit or execute a command without an explicit user action.
- Agent approval is refused while the bound Block prompt is busy or already contains input; malformed model output never degrades into a runnable proposal.
- History, workflow, file-tree and AI review insertions reject line breaks and terminal control characters before writing to a PTY.
- Support archives are created owner-only, make no network requests, and exclude configuration/history/session contents, credentials, host identity, SSH targets, and local paths.
- The package is marked `publish = false` until a project license is selected.
