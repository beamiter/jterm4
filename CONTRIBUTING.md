# Contributing to jterm4

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
