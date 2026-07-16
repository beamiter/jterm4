#!/usr/bin/env bash
# Reproducible dependency and shell-script security checks.

set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

cargo metadata --locked --format-version 1 --no-deps >/dev/null

if ! command -v cargo-audit >/dev/null 2>&1; then
    printf "Error: cargo-audit is required (install with 'cargo install cargo-audit --locked').\n" >&2
    exit 1
fi
cargo audit
cargo tree --locked --duplicates

if ! command -v shellcheck >/dev/null 2>&1; then
    printf 'Error: shellcheck is required.\n' >&2
    exit 1
fi
mapfile -t shell_files < <(find scripts packaging -type f -name '*.sh' -print | sort)
shellcheck "${shell_files[@]}"

