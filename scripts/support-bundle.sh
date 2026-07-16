#!/usr/bin/env bash
# Create a privacy-preserving jterm4 support archive without network access.

set -Eeuo pipefail
umask 077

usage() {
    printf 'Usage: %s [OUTPUT_DIRECTORY]\n' "$0" >&2
}

if (( $# > 1 )); then
    usage
    exit 2
fi

OUTPUT_DIR="${1:-.}"
JTERM4_BIN="${JTERM4_BIN:-jterm4}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
if [[ "${JTERM4_BIN}" == "jterm4" ]] \
    && ! command -v jterm4 >/dev/null 2>&1 \
    && [[ -x "${SCRIPT_DIR}/jterm4" ]]; then
    JTERM4_BIN="${SCRIPT_DIR}/jterm4"
fi
if [[ "${JTERM4_BIN}" == */* ]]; then
    [[ -x "${JTERM4_BIN}" ]] || {
        printf 'Error: jterm4 executable is not usable: %s\n' "${JTERM4_BIN}" >&2
        exit 1
    }
    binary_path="${JTERM4_BIN}"
else
    binary_path="$(command -v -- "${JTERM4_BIN}" 2>/dev/null || true)"
    [[ -n "${binary_path}" && -x "${binary_path}" ]] || {
        printf 'Error: jterm4 executable not found: %s\n' "${JTERM4_BIN}" >&2
        exit 1
    }
fi

mkdir -p -- "${OUTPUT_DIR}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BUNDLE_NAME="jterm4-support-${STAMP}"
WORK_DIR="$(mktemp -d)"
BUNDLE_DIR="${WORK_DIR}/${BUNDLE_NAME}"
trap 'rm -rf -- "${WORK_DIR}"' EXIT
mkdir -m 0700 -- "${BUNDLE_DIR}"

doctor_status=0
doctor_json_status=0
config_status=0
config_json_status=0
JTERM4_DIAGNOSTICS_REDACT=1 "${binary_path}" --doctor \
    >"${BUNDLE_DIR}/doctor.txt" 2>/dev/null || doctor_status=$?
JTERM4_DIAGNOSTICS_REDACT=1 "${binary_path}" --doctor --json \
    >"${BUNDLE_DIR}/doctor.json" 2>/dev/null || doctor_json_status=$?
JTERM4_DIAGNOSTICS_REDACT=1 "${binary_path}" --check-config \
    >/dev/null 2>&1 || config_status=$?
JTERM4_DIAGNOSTICS_REDACT=1 "${binary_path}" --check-config --json \
    >/dev/null 2>&1 || config_json_status=$?

version="$("${binary_path}" --version 2>/dev/null || true)"
if [[ ! "${version}" =~ ^jterm4\ [0-9A-Za-z.+_-]+$ ]]; then
    version="unavailable"
fi
config_path="$("${binary_path}" --config-path 2>/dev/null || true)"
config_home="${XDG_CONFIG_HOME:-${HOME:-}/.config}"
state_home="${XDG_STATE_HOME:-${HOME:-}/.local/state}"

{
    printf 'generated_at_utc=%s\n' "${STAMP}"
    printf 'version=%s\n' "${version}"
    printf 'doctor_exit=%s\n' "${doctor_status}"
    printf 'doctor_json_exit=%s\n' "${doctor_json_status}"
    printf 'config_check_exit=%s\n' "${config_status}"
    printf 'config_check_json_exit=%s\n' "${config_json_status}"
    printf 'kernel_name=%s\n' "$(uname -s 2>/dev/null || true)"
    printf 'kernel_release=%s\n' "$(uname -r 2>/dev/null || true)"
    printf 'architecture=%s\n' "$(uname -m 2>/dev/null || true)"
    printf 'session_type_present=%s\n' "$([[ -n "${XDG_SESSION_TYPE:-}" ]] && echo yes || echo no)"
    printf 'wayland_display_present=%s\n' "$([[ -n "${WAYLAND_DISPLAY:-}" ]] && echo yes || echo no)"
    printf 'x11_display_present=%s\n' "$([[ -n "${DISPLAY:-}" ]] && echo yes || echo no)"
    if command -v locale >/dev/null 2>&1; then
        printf 'locale_charmap=%s\n' "$(locale charmap 2>/dev/null || true)"
    fi
} >"${BUNDLE_DIR}/system.txt"

metadata() {
    local label="$1"
    local path="$2"
    if [[ -e "${path}" || -L "${path}" ]]; then
        if stat --printf='%A %a %s bytes\n' -- "${path}" >/dev/null 2>&1; then
            printf '%s: ' "${label}"
            stat --printf='%A %a %s bytes\n' -- "${path}"
        else
            printf '%s: present (metadata unavailable)\n' "${label}"
        fi
    else
        printf '%s: missing\n' "${label}"
    fi
}

{
    if [[ -n "${config_path}" && "${config_path}" != *$'\n'* ]]; then
        config_dir="$(dirname -- "${config_path}")"
        config_name="$(basename -- "${config_path}")"
        config_stem="${config_name%.*}"
        [[ -n "${config_stem}" ]] || config_stem="config"
        metadata 'config' "${config_path}"
        metadata 'config backup 1' "${config_dir}/${config_stem}.toml.bak"
        metadata 'config backup 2' "${config_dir}/${config_stem}.toml.bak.1"
        metadata 'config before restore' "${config_dir}/${config_stem}.toml.before-restore"
        metadata 'config write lock' "${config_dir}/${config_stem}.toml.lock"
    else
        printf 'config: path unavailable\n'
    fi
    metadata 'default command history' "${state_home}/jterm4/history.jsonl"
    if [[ -d "${config_home}/jterm4/windows" ]]; then
        shopt -s nullglob
        snapshots=("${config_home}/jterm4/windows"/window-*)
        printf 'session snapshot directory: present (%s entries)\n' "${#snapshots[@]}"
        shopt -u nullglob
    else
        printf 'session snapshot directory: missing\n'
    fi
} >"${BUNDLE_DIR}/storage-metadata.txt"

{
    for name in \
        JTERM4_AI_API_KEY JTERM4_AI_API_KEY_FILE ANTHROPIC_API_KEY OPENAI_API_KEY OLLAMA_API_KEY \
        JTERM4_AI_PROVIDER JTERM4_AI_MODEL JTERM4_AI_BASE_URL \
        JTERM4_ASSET_DIR JTERM4_WORKFLOW_DIR; do
        if [[ -n "${!name:-}" ]]; then
            printf '%s=present\n' "${name}"
        else
            printf '%s=absent\n' "${name}"
        fi
    done
} >"${BUNDLE_DIR}/environment-presence.txt"

cat >"${BUNDLE_DIR}/README.txt" <<'EOF_README'
This support bundle was generated without making network requests. It excludes
unredacted diagnostics, configuration-check output, configuration contents,
terminal/session contents, command history, clipboard data, environment values,
API keys, SSH destinations, host names, and local filesystem paths. It contains
redacted readiness diagnostics, diagnostic/configuration-check exit statuses,
non-sensitive system characteristics, file permission/size metadata, counts,
and only the presence or absence of selected integration variables.

The diagnostic report may still reveal enabled feature types and aggregate
counts. Review every file before sharing the archive.
EOF_README

ARCHIVE_PATH="${OUTPUT_DIR%/}/${BUNDLE_NAME}.tar.gz"
if [[ -e "${ARCHIVE_PATH}" || -L "${ARCHIVE_PATH}" ]]; then
    printf 'Error: refusing to overwrite %s\n' "${ARCHIVE_PATH}" >&2
    exit 1
fi
(
    set -o noclobber
    tar --sort=name --owner=0 --group=0 --numeric-owner -C "${WORK_DIR}" -cf - "${BUNDLE_NAME}" \
        | gzip -n -9 >"${ARCHIVE_PATH}"
)
chmod 0600 -- "${ARCHIVE_PATH}"
printf 'Created %s\n' "${ARCHIVE_PATH}"
