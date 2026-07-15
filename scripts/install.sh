#!/usr/bin/env bash
# Install jterm4 and its Linux desktop integration from a source checkout.

set -Eeuo pipefail
umask 077

APP_ID="io.github.beamiter.jterm4"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
HOME_DIR="${HOME:-}"
DESTDIR="${DESTDIR:-}"
PREFIX="${HOME_DIR}/.local"
BIN_DIR=""
BACKEND="auto"
INSTALL_CONFIG=1
INSTALL_DESKTOP=1
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
  --no-desktop           Do not install desktop, AppStream, or icon files
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
        --no-desktop)
            INSTALL_DESKTOP=0
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

if ((INSTALL_DESKTOP == 1)); then
    SHARE_DIR="${DESTDIR}${PREFIX}/share"
    run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}.desktop" \
        "${SHARE_DIR}/applications/${APP_ID}.desktop"
    run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}.metainfo.xml" \
        "${SHARE_DIR}/metainfo/${APP_ID}.metainfo.xml"
    run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}.svg" \
        "${SHARE_DIR}/icons/hicolor/scalable/apps/${APP_ID}.svg"
    run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}-128.png" \
        "${SHARE_DIR}/icons/hicolor/128x128/apps/${APP_ID}.png"
    run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}-256.png" \
        "${SHARE_DIR}/icons/hicolor/256x256/apps/${APP_ID}.png"
fi

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
if ((INSTALL_DESKTOP == 1)); then
    printf 'Installed desktop integration under %s/share\n' "${PREFIX}"
fi
if [[ -n "${DESTDIR}" ]]; then
    printf 'Staged file: %s\n' "${STAGED_BIN_DIR}/jterm4"
fi
printf 'Validate with: %s --doctor\n' "${BIN_DIR}/jterm4"
