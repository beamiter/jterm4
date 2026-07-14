#!/usr/bin/env bash
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
