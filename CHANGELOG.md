# Changelog

All notable changes to jterm4 will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Added - Major Features

#### Session Persistence Enhancements (2026-05-10)
- **Auto-restore working directories** - Tabs automatically cd to saved directories on restart
- **Auto-restore environments** - nix develop, ssh, docker sessions automatically restored
- **Split pane layout persistence** - Complete split layout saved and restored
- **Session ID passing** - rsh receives --session flag for history restoration

#### Safety Features (2026-05-10)
- **Tab close confirmation** - Warns before closing tabs with running processes
  - Detects ssh, mosh, docker, nix develop, and other important processes
  - Shows confirmation dialog with process information
  - Destructive action styling (red button)
  - Applies to all close methods (Ctrl+W, close button, etc.)

#### Testing Infrastructure (2026-05-10)
- **17 unit tests** - 100% pass rate
  - 9 state serialization tests
  - 3 config module tests
  - 5 library integration tests
- Created test infrastructure with `src/lib.rs`
- Backward compatibility tests for legacy state format

#### Code Organization (2026-05-10)
- **Extracted type definitions** - `src/block_view_types.rs` for better reusability
- **Enhanced documentation** - Comprehensive module-level docs
- **Architecture documentation** - State machine, performance optimizations

### Changed - Breaking Changes

#### State File Format (2026-05-10)
- **New JSON-based format** for `~/.config/jterm4/tabs.state`
  - Old format: `tab=name\tdir\tsid\tcmds`
  - New format: `tab=name\t<layout_json>`
- **Backward compatible** - Old format still supported
- **Enhanced capabilities** - Supports nested split layouts

### Changed - Improvements

#### Block Mode (2026-05-10)
- **session_id via environment** - RSH_SESSION_ID env var + --session flag
- **Command replay on PromptEnd** - Reliable command restoration
- **Command queue** - Sequential execution of multiple commands

#### VTE Mode (2026-05-10)
- **Fixed --session passing** - Only passed to rsh, not all shells
- **Better session restore** - session_id and initial_commands properly wired

#### UI/UX (2026-05-10)
- **Better error messages** - More informative process detection
- **Async dialogs** - Non-blocking confirmation dialogs
- **Improved tab management** - Better state tracking

### Fixed - Bug Fixes

#### VTE Mode Session Bug (2026-05-10)
- **CRITICAL**: Fixed bug where --session was passed to all shells
  - Previously: bash, sh would receive unknown --session flag
  - Now: Only rsh receives --session flag
  - Detection via filename parsing

#### Session Persistence (2026-05-10)
- **initial_commands ignored** - Block mode now uses saved commands
- **session_id not passed** - Both modes now pass session_id to rsh

### Infrastructure

#### Development (2026-05-10)
- Added `scripts/install.sh` - One-command installation
- Added `scripts/dev.sh` - Development convenience commands
- Added `config.toml.example` - Configuration template
- Added `OPTIMIZATION_SUMMARY.md` - Detailed optimization report

#### Dependencies (2026-05-10)
- Added `serde_json = "1.0"` - JSON serialization
- Added `env_logger = "0.11"` - Test logging (dev dependency)

## [0.1.0] - Previous Version

### Features
- Dual terminal modes (Block and VTE)
- Tab management with custom sidebar
- Split pane support (H/V)
- Basic session persistence (working directory)
- Theme support (default, light)
- Keyboard shortcuts
- Search functionality
- Font scaling and opacity controls
- GTK4 + VTE4 based implementation

---

## Version Numbering

- **Major**: Breaking changes to config format or API
- **Minor**: New features, backward compatible
- **Patch**: Bug fixes and minor improvements

## Migration Guides

### 0.1.0 → Unreleased

**State file format change** (backward compatible):
- Old `tabs.state` files are automatically converted
- New split pane layouts use JSON format
- No action required - automatic migration

**API changes** (for library users):
- `PaneLayout` enum is now public
- `escape_tab_state`, `unescape_tab_state`, `parse_tabs_state` are now public
- `Config` and `TerminalMode` are now public
- `TermView::new()` and `VteTerminalView::new()` now accept `session_id` and `initial_commands`

---

## Links

- [Optimization Summary](OPTIMIZATION_SUMMARY.md)
- [Configuration Example](config.toml.example)
- [Installation Script](scripts/install.sh)
- [Development Script](scripts/dev.sh)
