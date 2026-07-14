#!/usr/bin/env python3
"""Apply repository, privacy, installation, and maintenance hardening."""

from __future__ import annotations

from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one match in {path}, found {count}: {old[:120]!r}")
    path.write_text(text.replace(old, new, 1))


def insert_before_last_brace(path: Path, block: str) -> None:
    text = path.read_text()
    index = text.rfind("\n}")
    if index < 0:
        raise SystemExit(f"could not find final module brace in {path}")
    path.write_text(text[:index] + block + text[index:])


root = Path(__file__).resolve().parents[1]
state = root / "src/state.rs"
history = root / "src/block_view/history.rs"
readme = root / "README.md"
cargo = root / "Cargo.toml"
ci = root / ".github/workflows/ci.yml"

# Remove temporary audit scaffolding that was accidentally merged by previous
# source-export PRs. These workflows only create skipped checks on unrelated PRs.
for relative in [
    ".github/workflows/export-block-parity-round2.yml",
    ".github/workflows/export-round2-current.yml",
    ".round2-last-trigger",
    ".round2-open",
    ".round2-pr",
    ".round2-pr-ready",
    ".round2-trigger",
    "ROUND2_NOTES.md",
]:
    path = root / relative
    if path.exists():
        path.unlink()

# Session snapshots contain working directories and restorable commands. Keep
# both directories and files owner-only, sync file contents, and sync the rename.
replace_once(
    state,
    '''use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};''',
    '''use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};''',
)
replace_once(
    state,
    '''static WINDOW_STATE_PATHS: OnceLock<WindowStatePaths> = OnceLock::new();
static WINDOW_STATE_FINALIZED: AtomicBool = AtomicBool::new(false);

fn window_state_directory() -> PathBuf {''',
    '''static WINDOW_STATE_PATHS: OnceLock<WindowStatePaths> = OnceLock::new();
static WINDOW_STATE_FINALIZED: AtomicBool = AtomicBool::new(false);

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn make_file_private(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn write_private_file(path: &Path, payload: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(payload)?;
    file.sync_all()
}

fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::File::open(parent)?.sync_all()
}

fn window_state_directory() -> PathBuf {''',
)
replace_once(
    state,
    '''    if let Err(error) = fs::create_dir_all(&paths.directory) {''',
    '''    if let Err(error) = ensure_private_directory(&paths.directory) {''',
)
replace_once(
    state,
    '''            Ok(()) => log::info!("Recovered interrupted window snapshot {}", ready.display()),''',
    '''            Ok(()) => {
                if let Err(error) = make_file_private(&ready) {
                    log::warn!("Failed to tighten snapshot permissions {}: {error}", ready.display());
                }
                log::info!("Recovered interrupted window snapshot {}", ready.display());
            }''',
)
replace_once(
    state,
    '''            Ok(()) => {
                log::info!("Claimed legacy tabs snapshot {}", legacy.display());
                return paths.active.clone();
            }''',
    '''            Ok(()) => {
                if let Err(error) = make_file_private(&paths.active) {
                    log::warn!(
                        "Failed to tighten legacy snapshot permissions {}: {error}",
                        paths.active.display()
                    );
                }
                log::info!("Claimed legacy tabs snapshot {}", legacy.display());
                return paths.active.clone();
            }''',
)
replace_once(
    state,
    '''    if let Some(claimed) = claim_ready_snapshot_in(&paths.directory, &paths.active) {
        log::info!("Claimed window snapshot {}", claimed.display());
    }''',
    '''    if let Some(claimed) = claim_ready_snapshot_in(&paths.directory, &paths.active) {
        if let Err(error) = make_file_private(&paths.active) {
            log::warn!(
                "Failed to tighten claimed snapshot permissions {}: {error}",
                paths.active.display()
            );
        }
        log::info!("Claimed window snapshot {}", claimed.display());
    }''',
)
replace_once(
    state,
    '''        Ok(()) => {
            prune_ready_snapshots_in(&paths.directory, MAX_READY_WINDOW_STATES);
            log::info!("Published window snapshot {}", paths.ready.display());
        }''',
    '''        Ok(()) => {
            if let Err(error) = make_file_private(&paths.ready) {
                log::warn!(
                    "Failed to tighten published snapshot permissions {}: {error}",
                    paths.ready.display()
                );
            }
            if let Err(error) = sync_parent_directory(&paths.ready) {
                log::debug!(
                    "Failed to sync window-state directory {}: {error}",
                    paths.directory.display()
                );
            }
            prune_ready_snapshots_in(&paths.directory, MAX_READY_WINDOW_STATES);
            log::info!("Published window snapshot {}", paths.ready.display());
        }''',
)
replace_once(
    state,
    '''        if let Err(err) = fs::create_dir_all(parent) {''',
    '''        if let Err(err) = ensure_private_directory(parent) {''',
)
replace_once(
    state,
    '''    if let Err(err) = fs::write(&tmp_path, &payload) {''',
    '''    if let Err(err) = write_private_file(&tmp_path, payload.as_bytes()) {''',
)
replace_once(
    state,
    '''    log::info!("Successfully saved tabs state to {}", path.display());
}''',
    '''    if let Err(err) = make_file_private(&path) {
        log::warn!("Failed to tighten state permissions {}: {err}", path.display());
    }
    if let Err(err) = sync_parent_directory(&path) {
        log::debug!("Failed to sync state directory for {}: {err}", path.display());
    }
    log::info!("Successfully saved tabs state to {}", path.display());
}''',
)
insert_before_last_brace(
    state,
    r'''

    #[test]
    fn private_state_storage_uses_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_state_dir("private-permissions");
        let directory = root.join("windows");
        let snapshot = directory.join("window-1-1.active");
        ensure_private_directory(&directory).unwrap();
        write_private_file(&snapshot, b"state").unwrap();

        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }
''',
)

# Block history can contain full commands and output. Preserve its existing
# crash-safe replacement semantics while making newly created storage private.
replace_once(
    history,
    '''use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};''',
    '''use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};''',
)
replace_once(
    history,
    '''    fs::create_dir_all(parent)?;

    let temp_path = parent.join(temp_file_name(target)?);''',
    '''    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(parent)?;

    let temp_path = parent.join(temp_file_name(target)?);''',
)
replace_once(
    history,
    '''        let mut temp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        write_contents(&mut temp)?;''',
    '''        let mut temp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp_path)?;
        temp.set_permissions(fs::Permissions::from_mode(0o600))?;
        write_contents(&mut temp)?;''',
)
replace_once(
    history,
    '''        fs::rename(&temp_path, target)?;

        // Persist the directory entry''',
    '''        fs::rename(&temp_path, target)?;
        fs::set_permissions(target, fs::Permissions::from_mode(0o600))?;

        // Persist the directory entry''',
)
replace_once(
    history,
    '''    use std::io;
    use std::path::{Path, PathBuf};''',
    '''    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};''',
)
replace_once(
    history,
    '''        assert_eq!(fs::read(&target).unwrap(), b"first");

        atomic_write(&target, |file| {''',
    '''        assert_eq!(fs::read(&target).unwrap(), b"first");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(target.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        atomic_write(&target, |file| {''',
)

# Make the package explicitly non-publishable until the owner chooses a license.
replace_once(
    cargo,
    '''readme = "README.md"
categories = ["command-line-utilities", "development-tools"]''',
    '''readme = "README.md"
publish = false
categories = ["command-line-utilities", "development-tools"]''',
)

# CI should diagnose every formatted Rust file and lint the maintained scripts.
replace_once(
    ci,
    '''          git diff -- src/pty.rs src/block_view/scroll.rs > rustfmt.patch''',
    '''          git diff -- . > rustfmt.patch''',
)
replace_once(
    ci,
    '''          retention-days: 1

  clippy:''',
    '''          retention-days: 1

  shell:
    name: Shell scripts
    runs-on: ubuntu-24.04
    timeout-minutes: 10
    steps:
      - name: Checkout repository
        uses: actions/checkout@v7

      - name: Install ShellCheck
        run: |
          sudo apt-get update
          sudo apt-get install --no-install-recommends -y shellcheck

      - name: Check installer scripts
        run: |
          bash -n scripts/install.sh scripts/uninstall.sh
          shellcheck scripts/install.sh scripts/uninstall.sh
          scripts/install.sh --help >/dev/null
          scripts/uninstall.sh --help >/dev/null

  clippy:''',
)

INSTALL_SH = r'''#!/usr/bin/env bash
# Install jterm4 from a source checkout using Nix or Cargo.

set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
HOME_DIR="${HOME:-}"
DESTDIR="${DESTDIR:-}"
PREFIX="${HOME_DIR}/.local"
BIN_DIR=""
BACKEND="auto"
INSTALL_CONFIG=1
DRY_RUN=0

usage() {
    cat <<'USAGE'
Usage: ./scripts/install.sh [options]

Options:
  --prefix PATH          Runtime prefix (default: ~/.local)
  --bin-dir PATH         Runtime binary directory (overrides --prefix)
  --backend auto|nix|cargo
                         Build backend (default: auto; prefers Nix)
  --no-config            Do not install config.toml.example
  --dry-run              Print commands without changing files
  -h, --help             Show this help

Environment:
  DESTDIR                Optional staging root for packaging
  XDG_CONFIG_HOME        Config base (default: ~/.config)
  CARGO_TARGET_DIR       Cargo target directory (default: <repo>/target)
USAGE
}

die() {
    printf 'jterm4 install: %s\n' "$*" >&2
    exit 1
}

print_command() {
    printf '  '
    printf '%q ' "$@"
    printf '\n'
}

run() {
    print_command "$@"
    if ((DRY_RUN == 0)); then
        "$@"
    fi
}

run_in_repo() {
    printf '  (cd %q && ' "${REPO_ROOT}"
    printf '%q ' "$@"
    printf ')\n'
    if ((DRY_RUN == 0)); then
        (cd -- "${REPO_ROOT}" && "$@")
    fi
}

require_command() {
    if command -v "$1" >/dev/null 2>&1; then
        return
    fi
    ((DRY_RUN == 1)) || die "required command not found: $1"
}

while (($# > 0)); do
    case "$1" in
        --prefix)
            (($# >= 2)) || die "--prefix requires a path"
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#*=}"
            shift
            ;;
        --bin-dir)
            (($# >= 2)) || die "--bin-dir requires a path"
            BIN_DIR="$2"
            shift 2
            ;;
        --bin-dir=*)
            BIN_DIR="${1#*=}"
            shift
            ;;
        --backend)
            (($# >= 2)) || die "--backend requires auto, nix, or cargo"
            BACKEND="$2"
            shift 2
            ;;
        --backend=*)
            BACKEND="${1#*=}"
            shift
            ;;
        --no-config)
            INSTALL_CONFIG=0
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            (($# == 0)) || die "unexpected positional arguments: $*"
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

[[ -n "${HOME_DIR}" ]] || die "HOME is not set"
[[ -n "${PREFIX}" ]] || die "prefix must not be empty"
[[ "${PREFIX}" == /* ]] || die "--prefix must be an absolute path"
if [[ -z "${BIN_DIR}" ]]; then
    BIN_DIR="${PREFIX}/bin"
fi
[[ "${BIN_DIR}" == /* ]] || die "--bin-dir must be an absolute path"
if [[ -n "${DESTDIR}" ]]; then
    [[ "${DESTDIR}" == /* ]] || die "DESTDIR must be an absolute path"
    DESTDIR="${DESTDIR%/}"
fi

case "${BACKEND}" in
    auto)
        if command -v nix >/dev/null 2>&1; then
            BACKEND="nix"
        else
            BACKEND="cargo"
        fi
        ;;
    nix|cargo) ;;
    *) die "invalid backend '${BACKEND}'; expected auto, nix, or cargo" ;;
esac

TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
if [[ "${TARGET_DIR}" != /* ]]; then
    TARGET_DIR="${REPO_ROOT}/${TARGET_DIR}"
fi
export CARGO_TARGET_DIR="${TARGET_DIR}"

printf 'Building jterm4 with %s...\n' "${BACKEND}"
case "${BACKEND}" in
    nix)
        require_command nix
        run_in_repo nix develop --command cargo build --release --locked
        ;;
    cargo)
        require_command cargo
        run_in_repo cargo build --release --locked
        ;;
esac

BINARY="${TARGET_DIR}/release/jterm4"
if ((DRY_RUN == 0)) && [[ ! -x "${BINARY}" ]]; then
    die "release binary was not produced at ${BINARY}"
fi

require_command install
STAGED_BIN_DIR="${DESTDIR}${BIN_DIR}"
run install -d -m 0755 "${STAGED_BIN_DIR}"
run install -m 0755 "${BINARY}" "${STAGED_BIN_DIR}/jterm4"

CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME_DIR}/.config}"
[[ "${CONFIG_HOME}" == /* ]] || die "XDG_CONFIG_HOME must be an absolute path"
CONFIG_DIR="${CONFIG_HOME}/jterm4"
STAGED_CONFIG_DIR="${DESTDIR}${CONFIG_DIR}"
if ((INSTALL_CONFIG == 1)); then
    run install -d -m 0700 "${STAGED_CONFIG_DIR}"
    if [[ ! -e "${STAGED_CONFIG_DIR}/config.toml" ]]; then
        run install -m 0600 "${REPO_ROOT}/config.toml.example" "${STAGED_CONFIG_DIR}/config.toml"
    else
        printf 'Keeping existing config: %s\n' "${CONFIG_DIR}/config.toml"
    fi
fi

printf 'Installed jterm4 to %s\n' "${BIN_DIR}/jterm4"
if [[ -n "${DESTDIR}" ]]; then
    printf 'Staged file: %s\n' "${STAGED_BIN_DIR}/jterm4"
fi
printf 'Validate with: %s --doctor\n' "${BIN_DIR}/jterm4"
'''

UNINSTALL_SH = r'''#!/usr/bin/env bash
# Remove a jterm4 installation while preserving user data by default.

set -Eeuo pipefail

HOME_DIR="${HOME:-}"
DESTDIR="${DESTDIR:-}"
PREFIX="${HOME_DIR}/.local"
BIN_DIR=""
PURGE_CONFIG=0
DRY_RUN=0

usage() {
    cat <<'USAGE'
Usage: ./scripts/uninstall.sh [options]

Options:
  --prefix PATH          Runtime prefix (default: ~/.local)
  --bin-dir PATH         Runtime binary directory (overrides --prefix)
  --purge-config         Also remove the complete jterm4 config/state directory
  --dry-run              Print commands without changing files
  -h, --help             Show this help

Environment:
  DESTDIR                Optional staging root for packaging
  XDG_CONFIG_HOME        Config base (default: ~/.config)
USAGE
}

die() {
    printf 'jterm4 uninstall: %s\n' "$*" >&2
    exit 1
}

print_command() {
    printf '  '
    printf '%q ' "$@"
    printf '\n'
}

run() {
    print_command "$@"
    if ((DRY_RUN == 0)); then
        "$@"
    fi
}

while (($# > 0)); do
    case "$1" in
        --prefix)
            (($# >= 2)) || die "--prefix requires a path"
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#*=}"
            shift
            ;;
        --bin-dir)
            (($# >= 2)) || die "--bin-dir requires a path"
            BIN_DIR="$2"
            shift 2
            ;;
        --bin-dir=*)
            BIN_DIR="${1#*=}"
            shift
            ;;
        --purge-config)
            PURGE_CONFIG=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            (($# == 0)) || die "unexpected positional arguments: $*"
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

[[ -n "${HOME_DIR}" ]] || die "HOME is not set"
[[ -n "${PREFIX}" ]] || die "prefix must not be empty"
[[ "${PREFIX}" == /* ]] || die "--prefix must be an absolute path"
if [[ -z "${BIN_DIR}" ]]; then
    BIN_DIR="${PREFIX}/bin"
fi
[[ "${BIN_DIR}" == /* ]] || die "--bin-dir must be an absolute path"
if [[ -n "${DESTDIR}" ]]; then
    [[ "${DESTDIR}" == /* ]] || die "DESTDIR must be an absolute path"
    DESTDIR="${DESTDIR%/}"
fi

BINARY="${DESTDIR}${BIN_DIR}/jterm4"
if [[ -e "${BINARY}" || -L "${BINARY}" ]]; then
    run rm -f -- "${BINARY}"
else
    printf 'Binary not present: %s\n' "${BIN_DIR}/jterm4"
fi

if ((PURGE_CONFIG == 1)); then
    CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME_DIR}/.config}"
    [[ "${CONFIG_HOME}" == /* ]] || die "XDG_CONFIG_HOME must be an absolute path"
    CONFIG_DIR="${DESTDIR}${CONFIG_HOME}/jterm4"
    if [[ -e "${CONFIG_DIR}" ]]; then
        run rm -rf -- "${CONFIG_DIR}"
    else
        printf 'Config/state directory not present: %s\n' "${CONFIG_HOME}/jterm4"
    fi
else
    printf 'Preserved config and state. Use --purge-config to remove them.\n'
fi
'''

install_path = root / "scripts/install.sh"
uninstall_path = root / "scripts/uninstall.sh"
install_path.write_text(INSTALL_SH)
uninstall_path.write_text(UNINSTALL_SH)
install_path.chmod(0o755)
uninstall_path.chmod(0o755)

(root / "rust-toolchain.toml").write_text(
    '''[toolchain]
channel = "stable"
profile = "minimal"
components = ["rustfmt", "clippy"]
'''
)

(root / ".github/CODEOWNERS").write_text(
    '''* @beamiter
/.github/workflows/ @beamiter
/src/state.rs @beamiter
/src/pty.rs @beamiter
/src/block_view/ @beamiter
/SECURITY.md @beamiter
'''
)

(root / ".github/dependabot.yml").write_text(
    '''version: 2
updates:
  - package-ecosystem: cargo
    directory: /
    schedule:
      interval: weekly
      day: monday
      time: "04:17"
      timezone: Etc/UTC
    open-pull-requests-limit: 5
    groups:
      rust-minor-and-patch:
        update-types:
          - minor
          - patch

  - package-ecosystem: github-actions
    directory: /
    schedule:
      interval: monthly
    open-pull-requests-limit: 3
    groups:
      github-actions:
        patterns:
          - "*"
'''
)

(root / ".github/workflows/security-audit.yml").write_text(
    '''name: Security audit

on:
  pull_request:
    branches: [master]
    paths:
      - Cargo.toml
      - Cargo.lock
      - .github/workflows/security-audit.yml
  push:
    branches: [master]
    paths:
      - Cargo.toml
      - Cargo.lock
      - .github/workflows/security-audit.yml
  schedule:
    - cron: "17 3 * * 1"
  workflow_dispatch:

permissions:
  contents: read
  checks: write
  issues: write

concurrency:
  group: security-audit-${{ github.event.pull_request.number || github.ref }}
  cancel-in-progress: true

jobs:
  audit:
    name: RustSec
    runs-on: ubuntu-24.04
    timeout-minutes: 15
    steps:
      - name: Checkout repository
        uses: actions/checkout@v7

      - name: Audit Cargo.lock
        uses: rustsec/audit-check@v2.0.0
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
'''
)

(root / ".github/pull_request_template.md").write_text(
    '''## Summary

<!-- Explain the user-visible or architectural change. -->

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --all-features --locked`
- [ ] `cargo clippy --all-targets --all-features --locked -- -D warnings`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked`
- [ ] Relevant Wayland/X11 and VTE/Block smoke checks completed

## Safety and compatibility

- [ ] No credentials, private hosts, personal paths, or captured terminal output were committed
- [ ] PTY/process cleanup and persisted-state behavior were considered
- [ ] Configuration changes preserve existing defaults or document migration
- [ ] User-facing behavior and `CHANGELOG.md` were updated where appropriate
'''
)

(root / "SECURITY.md").write_text(
    '''# Security Policy

## Supported versions

Security fixes are developed against the current `master` branch and included in the next release. Older snapshots are not maintained as separate security branches.

## Reporting a vulnerability

Use GitHub's **Report a vulnerability** / private vulnerability reporting flow when it is available for this repository. If that option is unavailable, open a minimal issue asking the maintainer for a private contact channel. Do not include exploit details, credentials, private hostnames, terminal history, or captured output in a public issue.

A useful private report contains the affected commit or version, environment, reproduction steps, impact, and any proposed mitigation. The acknowledgement target is three business days; remediation timing depends on severity and complexity.

## Sensitive surfaces

Pay particular attention to PTY lifecycle and process-group signalling, OSC/clipboard handling, SSH and reconnect flows, AI-context redaction, configuration parsing, session snapshots, and Block history files. Session snapshots and Block history are owner-only on Unix (`0700` directories and `0600` files), but users should still avoid persisting secrets in terminal history.

## Disclosure

Please allow time for a fix and coordinated release before public disclosure. Confirmed vulnerabilities may receive a GitHub security advisory and CVE when appropriate.
'''
)

(root / "CONTRIBUTING.md").write_text(
    '''# Contributing to jterm4

## Development setup

The reproducible path is the repository's Nix shell:

```bash
nix develop
cargo run
```

A native Cargo build also works after installing GTK4, libadwaita, VTE GTK4, PCRE2, and `pkg-config` development packages. The repository toolchain file installs stable Rust with rustfmt and Clippy.

## Required checks

Run the same gates as CI before opening a pull request:

```bash
cargo fmt --all -- --check
cargo test --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked
cargo build --release --all-features --locked
bash -n scripts/install.sh scripts/uninstall.sh
shellcheck scripts/install.sh scripts/uninstall.sh
```

For UI changes, smoke-test both Wayland and X11 when practical, VTE and Block modes, CJK input, tab closing, process cleanup, and session restoration. Changes to Block rendering should also follow `docs/BLOCK_MODE_ACCEPTANCE.md`.

## Design expectations

Keep GTK work on the main thread and filesystem/process work off it. Preserve explicit PTY ownership, generation/cancellation checks for asynchronous UI results, atomic persistence, and backwards-compatible configuration defaults. Add focused unit tests for pure parsing, state transitions, quoting, and boundary conditions.

Never commit tokens, private hostnames, personal paths, captured terminal output, or real configuration files. Use placeholders in documentation and tests. Report vulnerabilities through `SECURITY.md`, not a public proof of concept.

## Pull requests

Prefer reviewable commits with a clear user-visible rationale. Update `README.md`, the user guide, architecture notes, and `CHANGELOG.md` when behavior changes. A pull request should describe residual risk and any manual checks that cannot run in CI.
'''
)

(root / "docs/ARCHITECTURE.md").write_text(
    '''# jterm4 Architecture

## Process entry and headless commands

`src/bin/jterm4.rs` is intentionally tiny and delegates to the library application. `src/cli.rs` handles help, version, config validation, path printing, and `--doctor` before GTK initialization, so diagnostics remain usable over SSH and in CI. A normal launch then enters `src/main.rs`, which builds the libadwaita application and the shared `UiState`.

## Terminal models

jterm4 has two explicit terminal models:

- **VTE mode** attaches a GTK VTE widget directly to a PTY and supports native split panes.
- **Block mode** owns its PTY and reader lifecycle in `TermView`, parses shell integration markers, and renders commands and output as searchable finished blocks plus one live terminal.

These modes share configuration, theming, input routing, process cleanup, and session metadata, but they do not pretend to have identical ownership. Block-mode pane splits remain disabled until every pane can own an independent `TermView` and PTY without creating hidden processes.

## UI composition

`UiState` coordinates tabs, panes, sidebars, search, configuration reloads, and actions. The file sidebar uses GTK4's supported model-view stack: `gio::ListStore`, `TreeListModel`, `ListView`, and `TreeExpander`. Directory scans run on named worker threads; generation checks prevent stale results from repopulating a changed root.

## Asynchronous boundaries

GTK widgets are only mutated on the main context. PTY reads, directory enumeration, Git metadata, notifications, and remote work happen outside the UI thread and return bounded results. Callbacks that outlive a tab are explicitly removed or guarded by weak references/generations.

## Persistence

Each process owns one active window snapshot. A snapshot becomes available to a future process only after graceful shutdown, using an atomic rename from `.active` to `.state`. Launches atomically claim one ready snapshot, stale active snapshots are recovered after their owner PID exits, and legacy `tabs.state` is migrated without allowing two processes to restore it. Ready snapshots are retained with a bounded count.

Session snapshots and Block history use owner-only Unix permissions. File contents are written to a sibling temporary file, synced, atomically renamed, and followed by a directory sync where applicable. Parsers retain legacy compatibility and reject pathological record sizes.

## Configuration and observability

Configuration is parsed and validated before replacing the active runtime value. File monitoring provides hot reload while manual reload remains available. The lightweight logger supports plain levels and target-specific `RUST_LOG` directives; each line includes relative time, severity, and target. `--doctor` reports configuration status, display/input environment, optional tools, shell choice, and ready/active snapshot counts without exposing snapshot contents.

## Quality gates

CI runs rustfmt, tests, strict Clippy, Rustdoc with warnings denied, a release build, shell syntax/ShellCheck, and RustSec auditing. Dependabot covers Cargo and GitHub Actions. Pure helpers should have unit tests; GTK behavior that cannot be automated belongs in the acceptance checklist and pull-request validation notes.

## Invariants

1. A visible terminal owns every live PTY; closing a tab terminates its process group.
2. Background work never directly mutates GTK widgets.
3. Concurrent processes never claim or overwrite the same ready session snapshot.
4. Persisted terminal data is bounded, atomically replaced, and owner-only.
5. Invalid configuration never replaces the last valid runtime configuration.
6. New features must preserve both VTE and Block input routing or explicitly document a mode limitation.
'''
)

(root / "docs/RELEASING.md").write_text(
    '''# Release Process

1. Start from a clean, green `master` branch. Update the version in `Cargo.toml` and move the relevant `CHANGELOG.md` entries from **Unreleased** into a dated version section.
2. Run all commands from `CONTRIBUTING.md`, including the release build and RustSec audit. Validate `jterm4 --doctor`, configuration migration, normal and forced tab close, VTE splits, Block history, Wayland, and X11.
3. Confirm that no temporary migration workflows, source-export markers, local paths, hosts, or secrets are present. Review dependency and GitHub Action updates.
4. Create and push a signed `vX.Y.Z` tag, then draft GitHub release notes from the changelog and merged pull requests.
5. Publish only artifacts that were built by a documented, reproducible packaging path. The raw Cargo binary dynamically links GTK/libadwaita/VTE and should not be described as a portable Linux binary. Until Flatpak/AppImage/native packages are implemented, use source archives and explicit distro dependencies.
6. After publishing, run the installed build's `--version`, `--doctor`, and one interactive terminal smoke test, then mark the changelog comparison links.

The crate is intentionally `publish = false` until the project owner selects and documents a license.
'''
)

(root / "CHANGELOG.md").write_text(
    '''# Changelog

All notable user-visible and operational changes are recorded here.

## Unreleased

### Added

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
'''
)

replace_once(
    readme,
    '''## 构建与运行

推荐使用仓库提供的 Nix 开发环境：

```bash
nix develop
cargo run
```

常用命令：

```bash
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

安装到 `~/.local/bin`：

```bash
./scripts/install.sh
```

系统需要 GTK4、libadwaita、VTE GTK4、PCRE2 和 `pkg-config` 的开发包。
''',
    '''## 构建与运行

推荐使用仓库提供的 Nix 开发环境：

```bash
nix develop
cargo run
```

也可以在安装 GTK4、libadwaita、VTE GTK4、PCRE2 与 `pkg-config` 开发包后直接使用 Cargo。完整质量门禁：

```bash
cargo fmt --all -- --check
cargo test --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked
cargo build --release --all-features --locked
```

安装脚本默认优先使用 Nix；没有 Nix 时自动退回 Cargo，并且不会覆盖已有配置：

```bash
./scripts/install.sh
./scripts/install.sh --backend cargo
./scripts/install.sh --prefix /opt/jterm4 --no-config
./scripts/install.sh --dry-run
```

默认安装到 `~/.local/bin/jterm4`，配置使用 `0600`。脚本支持 `DESTDIR`、`XDG_CONFIG_HOME` 和 `CARGO_TARGET_DIR`。卸载默认保留配置、状态与历史：

```bash
./scripts/uninstall.sh
./scripts/uninstall.sh --purge-config   # 明确删除全部配置和状态
```
''',
)
replace_once(
    readme,
    '''- 每个窗口使用独立的原子会话快照；并发窗口互不覆盖，崩溃遗留快照会在下次启动回收。''',
    '''- 每个窗口使用独立的原子会话快照；并发窗口互不覆盖，崩溃遗留快照会在下次启动回收。
- 会话快照和 Block 历史使用 `0700` 目录、`0600` 文件、同步写入与原子替换，降低信息泄露和断电损坏风险。
- Cargo 包在许可证确定前标记为 `publish = false`，并由每周 RustSec 审计与 Dependabot 持续检查依赖。''',
)
replace_once(
    readme,
    '''进一步说明见 [用户指南](docs/USER_GUIDE.md)、[Block 模式验收清单](docs/BLOCK_MODE_ACCEPTANCE.md)、[性能指南](docs/PERFORMANCE.md) 和 [Tailscale/SSH 配置](docs/tailscale-setup.md)。''',
    '''进一步说明见 [用户指南](docs/USER_GUIDE.md)、[架构说明](docs/ARCHITECTURE.md)、[Block 模式验收清单](docs/BLOCK_MODE_ACCEPTANCE.md)、[性能指南](docs/PERFORMANCE.md)、[发布流程](docs/RELEASING.md) 和 [Tailscale/SSH 配置](docs/tailscale-setup.md)。参与开发前请阅读 [贡献指南](CONTRIBUTING.md)、[安全策略](SECURITY.md) 与 [变更日志](CHANGELOG.md)。''',
)

for required in [
    root / "scripts/uninstall.sh",
    root / "SECURITY.md",
    root / "CONTRIBUTING.md",
    root / "CHANGELOG.md",
    root / "docs/ARCHITECTURE.md",
    root / "docs/RELEASING.md",
    root / ".github/workflows/security-audit.yml",
    root / ".github/dependabot.yml",
    root / ".github/CODEOWNERS",
    root / "rust-toolchain.toml",
]:
    if not required.exists() or not required.read_text().strip():
        raise SystemExit(f"required hardening file missing or empty: {required}")

for stale in ["TreeStore", "TreeView", "last one closed wins", "ROUND2_NOTES"]:
    if stale in state.read_text() or stale in history.read_text():
        raise SystemExit(f"stale marker remains in runtime source: {stale}")

print("project hardening migration applied")
