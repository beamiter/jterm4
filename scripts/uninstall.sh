#!/usr/bin/env bash
# Remove jterm4 while preserving user configuration and state by default.

set -Eeuo pipefail

APP_ID="io.github.beamiter.jterm4"
HOME_DIR="${HOME:-}"
DESTDIR="${DESTDIR:-}"
PREFIX="${HOME_DIR}/.local"
BIN_DIR=""
PURGE_CONFIG=0
DRY_RUN=0

usage() {
    cat <<'USAGE'
Usage: uninstall.sh [options]

Options:
  --prefix PATH          Runtime prefix (default: ~/.local)
  --bin-dir PATH         Runtime binary directory (overrides --prefix)
  --purge-config         Also remove jterm4 config and default XDG state
  --dry-run              Print commands without changing files
  -h, --help             Show this help

Environment:
  DESTDIR                Optional staging root for packaging
  XDG_CONFIG_HOME        Config base (default: ~/.config)
  XDG_STATE_HOME         State base (default: ~/.local/state)
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

remove_file() {
    local path="$1"
    if [[ -e "${path}" || -L "${path}" ]]; then
        run rm -f -- "${path}"
    fi
}

remove_dir_if_empty() {
    local path="$1"
    if [[ -d "${path}" ]]; then
        run rmdir --ignore-fail-on-non-empty -- "${path}"
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

remove_file "${DESTDIR}${BIN_DIR}/jterm4"
remove_file "${DESTDIR}${BIN_DIR}/jterm4-support-bundle"
SHARE_DIR="${DESTDIR}${PREFIX}/share"
remove_file "${SHARE_DIR}/applications/${APP_ID}.desktop"
remove_file "${SHARE_DIR}/metainfo/${APP_ID}.metainfo.xml"
remove_file "${SHARE_DIR}/icons/hicolor/scalable/apps/${APP_ID}.svg"
remove_file "${SHARE_DIR}/icons/hicolor/128x128/apps/${APP_ID}.png"
remove_file "${SHARE_DIR}/icons/hicolor/256x256/apps/${APP_ID}.png"
remove_file "${SHARE_DIR}/jterm4/shell-integration/README.md"
remove_file "${SHARE_DIR}/jterm4/shell-integration/jterm4.bash"
remove_file "${SHARE_DIR}/jterm4/shell-integration/jterm4.zsh"
remove_file "${SHARE_DIR}/jterm4/shell-integration/jterm4.fish"
remove_file "${SHARE_DIR}/jterm4/shell-integration/jterm4.ps1"
remove_file "${SHARE_DIR}/jterm4/workflows/git-feature.yaml"
remove_file "${SHARE_DIR}/jterm4/workflows/find-large-files.yaml"
remove_file "${SHARE_DIR}/jterm4/workflows/git-rebase-interactive.yaml"
remove_file "${SHARE_DIR}/jterm4/workflows/ssh-tunnel.yaml"
remove_file "${SHARE_DIR}/jterm4/workflows/docker-tail-logs.yaml"
remove_file "${SHARE_DIR}/jterm4/workflows/kill-port.yaml"
remove_file "${SHARE_DIR}/jterm4/notebooks/welcome.jtnb.md"
remove_file "${SHARE_DIR}/doc/jterm4/README.md"
remove_file "${SHARE_DIR}/doc/jterm4/config.toml.example"
remove_file "${SHARE_DIR}/doc/jterm4/Cargo.lock"
remove_file "${SHARE_DIR}/doc/jterm4/BUILDINFO"
remove_dir_if_empty "${SHARE_DIR}/jterm4/shell-integration"
remove_dir_if_empty "${SHARE_DIR}/jterm4/workflows"
remove_dir_if_empty "${SHARE_DIR}/jterm4/notebooks"
remove_dir_if_empty "${SHARE_DIR}/jterm4"
remove_dir_if_empty "${SHARE_DIR}/doc/jterm4"

if ((PURGE_CONFIG == 1)); then
    CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME_DIR}/.config}"
    [[ "${CONFIG_HOME}" == /* ]] || die "XDG_CONFIG_HOME must be an absolute path"
    CONFIG_DIR="${DESTDIR}${CONFIG_HOME}/jterm4"
    if [[ -e "${CONFIG_DIR}" ]]; then
        run rm -rf -- "${CONFIG_DIR}"
    else
        printf 'Config/state directory not present: %s\n' "${CONFIG_HOME}/jterm4"
    fi
    STATE_HOME="${XDG_STATE_HOME:-${HOME_DIR}/.local/state}"
    [[ "${STATE_HOME}" == /* ]] || die "XDG_STATE_HOME must be an absolute path"
    STATE_DIR="${DESTDIR}${STATE_HOME}/jterm4"
    if [[ -e "${STATE_DIR}" ]]; then
        run rm -rf -- "${STATE_DIR}"
    else
        printf 'Default state directory not present: %s\n' "${STATE_HOME}/jterm4"
    fi
else
    printf 'Preserved config and state. Use --purge-config to remove them.\n'
fi
