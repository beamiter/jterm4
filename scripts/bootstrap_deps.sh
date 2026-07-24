#!/usr/bin/env bash
# Provision the native dependencies required to build and install jterm4.
#
# `cargo install --path .` (and a plain `cargo build`) needs the GTK 4 stack
# resolved through pkg-config: glib-2.0 >= 2.80, pango >= 1.52, graphene,
# gtk4 (>= 4.14), libadwaita-1 (>= 1.5), vte-2.91-gtk4 (>= 0.76), and pcre2.
# Many stable distributions (for example Ubuntu 22.04) ship these too old or
# omit the GTK 4 build of VTE entirely, so the crate build scripts fail with
# "was not found" / "but version of X is <old>" errors.
#
# This script makes `cargo install --path .` reproducible in two ways:
#
#   * Nix backend (default, recommended): installs Nix if needed, enables
#     flakes, and drives the build through `nix develop`, which pins the exact
#     library versions the project targets and never touches system packages.
#   * System backend: installs the matching -dev packages with the platform
#     package manager, then verifies the pkg-config versions are new enough.
#
# See CONTRIBUTING.md and flake.nix for the canonical development environment.

set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

BACKEND="auto"
ASSUME_YES=0
CHECK_ONLY=0
DO_INSTALL_CRATE=0

# Minimum pkg-config module versions the crate graph requires. Keep these in
# sync with the feature flags in Cargo.toml (relm4 gnome_46, vte4 v0_76).
readonly REQ_GLIB="2.80"
readonly REQ_PANGO="1.52"
readonly REQ_GTK4="4.14"
readonly REQ_ADW="1.5"
readonly REQ_VTE="0.76"

usage() {
    cat <<'USAGE'
Usage: ./scripts/bootstrap_deps.sh [options]

Provision the native dependencies for building and installing jterm4.

Options:
  --backend auto|nix|system
                    Provisioning strategy (default: auto; prefers Nix).
  --check           Only verify dependencies; install nothing. Exit non-zero
                    if the selected backend cannot build jterm4.
  --install         After provisioning, run `cargo install --path .` inside
                    the selected backend.
  --yes             Do not prompt before installing Nix or system packages.
  -h, --help        Show this help.

Examples:
  ./scripts/bootstrap_deps.sh                 # set up the recommended toolchain
  ./scripts/bootstrap_deps.sh --check         # report what is missing
  ./scripts/bootstrap_deps.sh --backend system --install
USAGE
}

log() { printf 'bootstrap_deps: %s\n' "$*"; }
warn() { printf 'bootstrap_deps: warning: %s\n' "$*" >&2; }
die() {
    printf 'bootstrap_deps: error: %s\n' "$*" >&2
    exit 1
}

have() { command -v "$1" >/dev/null 2>&1; }

confirm() {
    # confirm "prompt" -> 0 if the user agrees (or --yes was passed).
    ((ASSUME_YES == 1)) && return 0
    local reply
    printf '%s [y/N] ' "$1" >&2
    read -r reply || return 1
    [[ "${reply}" == [yY] || "${reply}" == [yY][eE][sS] ]]
}

# Return 0 if $1 (found version) is >= $2 (required version), dotted numbers.
version_ge() {
    [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" == "$2" ]]
}

parse_args() {
    while (($# > 0)); do
        case "$1" in
            --backend)
                (($# >= 2)) || die "--backend requires auto, nix, or system"
                BACKEND="$2"
                shift 2
                ;;
            --backend=*)
                BACKEND="${1#*=}"
                shift
                ;;
            --check)
                CHECK_ONLY=1
                shift
                ;;
            --install)
                DO_INSTALL_CRATE=1
                shift
                ;;
            --yes | -y)
                ASSUME_YES=1
                shift
                ;;
            -h | --help)
                usage
                exit 0
                ;;
            *)
                die "unknown option: $1"
                ;;
        esac
    done

    case "${BACKEND}" in
        auto | nix | system) ;;
        *) die "invalid backend '${BACKEND}'; expected auto, nix, or system" ;;
    esac
}

# --------------------------------------------------------------------------
# Nix backend
# --------------------------------------------------------------------------

nix_flakes_ready() {
    have nix || return 1
    # Flakes require both the experimental features to be enabled.
    nix flake --help >/dev/null 2>&1
}

enable_nix_flakes() {
    nix_flakes_ready && return 0

    local conf_dir="${HOME}/.config/nix"
    local conf="${conf_dir}/nix.conf"
    log "Enabling flakes for the current user in ${conf}"
    mkdir -p "${conf_dir}"
    if [[ -f "${conf}" ]] && grep -q 'experimental-features' "${conf}"; then
        warn "${conf} already sets experimental-features; leaving it unchanged"
    else
        printf 'experimental-features = nix-command flakes\n' >>"${conf}"
    fi
    nix_flakes_ready ||
        warn "flakes still unavailable; open a new shell or check ${conf}"
}

install_nix() {
    have nix && return 0

    have curl || die "curl is required to install Nix"
    cat >&2 <<'NOTE'
Nix is not installed. The official installer performs a system-level change:
it creates the /nix store, adds build users, and (multi-user mode) installs a
daemon and modifies shell profiles. Review https://nixos.org/download/ first.
NOTE
    confirm "Run the official Nix installer now?" ||
        die "Nix is required for the nix backend; install it and re-run"

    log "Downloading and running the official Nix installer (multi-user)"
    sh <(curl -fsSL https://nixos.org/nix/install) --daemon

    # The installer writes a profile script that this non-interactive shell has
    # not sourced yet; source it so `nix` is on PATH for the rest of the run.
    local daemon_profile=/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
    local single_profile="${HOME}/.nix-profile/etc/profile.d/nix.sh"
    # shellcheck disable=SC1090
    if [[ -e "${daemon_profile}" ]]; then
        . "${daemon_profile}"
    elif [[ -e "${single_profile}" ]]; then
        . "${single_profile}"
    fi

    have nix ||
        die "Nix installed but not on PATH; open a new shell and re-run"

    repair_nix_daemon
}

# The official multi-user installer occasionally finishes extracting the store
# but never links or starts the systemd daemon (for example when interrupted).
# In that state a non-root user cannot write to /nix/store and every build
# fails. Link and start the units from the store if they are missing.
repair_nix_daemon() {
    have systemctl || return 0
    [[ -S /nix/var/nix/daemon-socket/socket ]] && return 0

    local unit_dir=/nix/var/nix/profiles/default/lib/systemd/system
    [[ -e "${unit_dir}/nix-daemon.socket" ]] || return 0

    local sudo
    sudo="$(sudo_prefix)"
    log "Completing nix-daemon systemd setup"
    ${sudo} ln -sf "${unit_dir}/nix-daemon.socket" \
        /etc/systemd/system/nix-daemon.socket
    ${sudo} ln -sf "${unit_dir}/nix-daemon.service" \
        /etc/systemd/system/nix-daemon.service
    ${sudo} systemctl daemon-reload
    ${sudo} systemctl enable --now nix-daemon.socket
    [[ -S /nix/var/nix/daemon-socket/socket ]] ||
        warn "nix-daemon socket still absent; check 'systemctl status nix-daemon'"
}

run_nix_backend() {
    log "Using the Nix backend (recommended)"

    if ((CHECK_ONLY == 1)); then
        have nix || die "Nix is not installed"
        nix_flakes_ready || die "Nix flakes are not enabled (see nix.conf)"
        log "Verifying the flake dev shell resolves..."
        nix develop --command true ||
            die "nix develop failed; the flake dev shell could not be realized"
        log "OK: 'nix develop' can build jterm4"
        return 0
    fi

    install_nix
    enable_nix_flakes

    log "Realizing the flake dev shell (first run downloads the GTK stack)..."
    nix develop --command true ||
        die "nix develop failed; see the output above"
    log "OK: dependencies are available through 'nix develop'"

    if ((DO_INSTALL_CRATE == 1)); then
        log "Running: nix develop --command cargo install --path . --locked"
        nix develop --command cargo install --path . --locked
    fi
}

# --------------------------------------------------------------------------
# System-package backend
# --------------------------------------------------------------------------

detect_pkg_manager() {
    if have apt-get; then
        printf 'apt'
    elif have dnf; then
        printf 'dnf'
    elif have pacman; then
        printf 'pacman'
    elif have zypper; then
        printf 'zypper'
    else
        printf 'unknown'
    fi
}

# Print the -dev/-devel package list for a package manager, space-separated.
system_packages() {
    case "$1" in
        apt)
            printf '%s' "build-essential pkg-config libglib2.0-dev \
                libpango1.0-dev libgtk-4-dev libadwaita-1-dev \
                libvte-2.91-gtk4-dev libpcre2-dev"
            ;;
        dnf)
            printf '%s' "gcc pkgconf-pkg-config glib2-devel pango-devel \
                gtk4-devel libadwaita-devel vte291-gtk4-devel pcre2-devel"
            ;;
        pacman)
            printf '%s' "base-devel pkgconf glib2 pango gtk4 libadwaita \
                vte4 pcre2"
            ;;
        zypper)
            printf '%s' "gcc pkgconf-pkg-config glib2-devel pango-devel \
                gtk4-devel libadwaita-devel vte-devel pcre2-devel"
            ;;
        *)
            return 1
            ;;
    esac
}

sudo_prefix() {
    if [[ "$(id -u)" -eq 0 ]]; then
        printf ''
    elif have sudo; then
        printf 'sudo'
    else
        die "root privileges are required to install packages (install sudo)"
    fi
}

install_system_packages() {
    local mgr="$1"
    local pkgs
    pkgs="$(system_packages "${mgr}")" ||
        die "no known package set for '${mgr}'; install the GTK 4 dev stack manually"
    # Normalise the line-continuation whitespace into single spaces.
    read -r -a pkg_array <<<"${pkgs}"

    log "Will install with ${mgr}: ${pkg_array[*]}"
    confirm "Install these ${#pkg_array[@]} packages now?" ||
        die "declined; install the packages above and re-run --check"

    local sudo
    sudo="$(sudo_prefix)"
    case "${mgr}" in
        apt)
            ${sudo} apt-get update
            ${sudo} apt-get install -y "${pkg_array[@]}"
            ;;
        dnf) ${sudo} dnf install -y "${pkg_array[@]}" ;;
        pacman) ${sudo} pacman -S --needed --noconfirm "${pkg_array[@]}" ;;
        zypper) ${sudo} zypper install -y "${pkg_array[@]}" ;;
    esac
}

# check_module <pkg-config name> [minimum version]
check_module() {
    local name="$1" min="${2:-}" found
    if ! pkg-config --exists "${name}" 2>/dev/null; then
        warn "missing pkg-config module: ${name}"
        return 1
    fi
    if [[ -n "${min}" ]]; then
        found="$(pkg-config --modversion "${name}" 2>/dev/null || printf '0')"
        if ! version_ge "${found}" "${min}"; then
            warn "${name} ${found} is too old (need >= ${min})"
            return 1
        fi
    fi
    return 0
}

verify_system_deps() {
    have pkg-config || {
        warn "pkg-config is not installed"
        return 1
    }
    local ok=0
    check_module "glib-2.0" "${REQ_GLIB}" || ok=1
    check_module "pango" "${REQ_PANGO}" || ok=1
    check_module "graphene-gobject-1.0" || ok=1
    check_module "gtk4" "${REQ_GTK4}" || ok=1
    check_module "libadwaita-1" "${REQ_ADW}" || ok=1
    check_module "vte-2.91-gtk4" "${REQ_VTE}" || ok=1
    check_module "libpcre2-8" || ok=1
    return "${ok}"
}

run_system_backend() {
    log "Using the system-package backend"
    have cargo || die "cargo (Rust toolchain) is required; see rustup.rs"

    if verify_system_deps; then
        log "OK: system libraries satisfy jterm4's build requirements"
    elif ((CHECK_ONLY == 1)); then
        die "system libraries do not satisfy jterm4; see warnings above"
    else
        local mgr
        mgr="$(detect_pkg_manager)"
        [[ "${mgr}" == "unknown" ]] &&
            die "unsupported package manager; install the GTK 4 dev stack manually"
        install_system_packages "${mgr}"
        verify_system_deps ||
            die "dependencies still unsatisfied after install. On older \
distributions (e.g. Ubuntu 22.04) the packaged GTK/VTE are too old or omit \
vte-2.91-gtk4; use '--backend nix' instead."
    fi

    ((CHECK_ONLY == 1)) && return 0

    if ((DO_INSTALL_CRATE == 1)); then
        log "Running: cargo install --path . --locked"
        cargo install --path . --locked
    fi
}

# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------

resolve_backend() {
    if [[ "${BACKEND}" != "auto" ]]; then
        printf '%s' "${BACKEND}"
        return
    fi
    # Prefer Nix when it is present or when the system libraries are too old
    # to build the project; otherwise use the already-usable system packages.
    if have nix; then
        printf 'nix'
    elif verify_system_deps >/dev/null 2>&1; then
        printf 'system'
    else
        printf 'nix'
    fi
}

main() {
    parse_args "$@"
    local backend
    backend="$(resolve_backend)"
    case "${backend}" in
        nix) run_nix_backend ;;
        system) run_system_backend ;;
    esac

    if ((CHECK_ONLY == 0)) && ((DO_INSTALL_CRATE == 0)); then
        cat <<EOF

Dependencies ready. To build and install jterm4:
  ${backend}$([[ ${backend} == nix ]] && printf ' develop --command')  cargo install --path . --locked

Or re-run this script with --install to do it now.
EOF
    fi
}

main "$@"
