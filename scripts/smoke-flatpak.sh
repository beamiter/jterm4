#!/usr/bin/env bash
# Exercise VTE and Block launches under headless X11 and Wayland sessions.

set -Eeuo pipefail

APP_ID="${1:-io.github.beamiter.jterm4}"
LOG_DIR="${LOG_DIR:-flatpak-smoke-logs}"
RUNTIME_DIR="${XDG_RUNTIME_DIR:-$(mktemp -d)}"
CREATED_RUNTIME=0
XVFB_PID=""
WESTON_PID=""

if [[ -z "${XDG_RUNTIME_DIR:-}" ]]; then
    CREATED_RUNTIME=1
fi
export XDG_RUNTIME_DIR="${RUNTIME_DIR}"
mkdir -p "${XDG_RUNTIME_DIR}" "${LOG_DIR}"
chmod 0700 "${XDG_RUNTIME_DIR}"

cleanup() {
    flatpak kill "${APP_ID}" >/dev/null 2>&1 || true
    [[ -z "${WESTON_PID}" ]] || kill "${WESTON_PID}" >/dev/null 2>&1 || true
    [[ -z "${XVFB_PID}" ]] || kill "${XVFB_PID}" >/dev/null 2>&1 || true
    if ((CREATED_RUNTIME == 1)); then
        rm -rf -- "${XDG_RUNTIME_DIR}"
    fi
}
trap cleanup EXIT

smoke_mode() {
    local backend="$1"
    local mode="$2"
    local log="${LOG_DIR}/${backend}-${mode}.log"

    flatpak run --env="JTERM4_MODE=${mode}" "${APP_ID}" >"${log}" 2>&1 &
    local launcher=$!
    sleep 4
    if ! kill -0 "${launcher}" >/dev/null 2>&1; then
        wait "${launcher}" || true
        cat "${log}" >&2
        printf 'Flatpak %s/%s launch exited before smoke window\n' "${backend}" "${mode}" >&2
        return 1
    fi
    flatpak kill "${APP_ID}" >/dev/null 2>&1 || true
    wait "${launcher}" || true
}

command -v Xvfb >/dev/null 2>&1 || {
    printf 'Xvfb is required for the X11 smoke test\n' >&2
    exit 1
}
Xvfb :99 -screen 0 1280x800x24 >"${LOG_DIR}/xvfb.log" 2>&1 &
XVFB_PID=$!
export DISPLAY=:99
unset WAYLAND_DISPLAY
sleep 2
smoke_mode x11 vte
smoke_mode x11 block
kill "${XVFB_PID}" >/dev/null 2>&1 || true
wait "${XVFB_PID}" 2>/dev/null || true
XVFB_PID=""

command -v weston >/dev/null 2>&1 || {
    printf 'weston is required for the Wayland smoke test\n' >&2
    exit 1
}
unset DISPLAY
export WAYLAND_DISPLAY=wayland-jterm4
weston \
    --backend=headless-backend.so \
    --renderer=pixman \
    --socket="${WAYLAND_DISPLAY}" \
    --idle-time=0 \
    --log="${LOG_DIR}/weston.log" &
WESTON_PID=$!
sleep 3
smoke_mode wayland vte
smoke_mode wayland block
