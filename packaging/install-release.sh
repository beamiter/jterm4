#!/usr/bin/env bash
# Install a prebuilt jterm4 release bundle for the current user.

set -Eeuo pipefail
umask 077

APP_ID="io.github.beamiter.jterm4"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
HOME_DIR="${HOME:-}"
PREFIX="${HOME_DIR}/.local"
BIN_DIR="${PREFIX}/bin"
SHARE_DIR="${PREFIX}/share"
CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME_DIR}/.config}"
CONFIG_DIR="${CONFIG_HOME}/jterm4"
ASSET_DIR="${SHARE_DIR}/jterm4"
DOC_DIR="${SHARE_DIR}/doc/jterm4"

die() {
    printf 'jterm4 release install: %s\n' "$*" >&2
    exit 1
}

[[ -n "${HOME_DIR}" ]] || die "HOME is not set"
[[ "${CONFIG_HOME}" == /* ]] || die "XDG_CONFIG_HOME must be an absolute path"
[[ -x "${SCRIPT_DIR}/bin/jterm4" ]] \
    || die "${SCRIPT_DIR}/bin/jterm4 is missing or not executable"

printf 'Installing jterm4 for %s...\n' "${USER:-the current user}"
install -Dm0755 "${SCRIPT_DIR}/bin/jterm4" "${BIN_DIR}/jterm4"
install -Dm0755 "${SCRIPT_DIR}/bin/jterm4-support-bundle" \
    "${BIN_DIR}/jterm4-support-bundle"

install -d -m 0700 "${CONFIG_DIR}"
if [[ ! -e "${CONFIG_DIR}/config.toml" ]]; then
    install -m 0600 "${SCRIPT_DIR}/share/doc/jterm4/config.toml.example" \
        "${CONFIG_DIR}/config.toml"
    printf 'Created %s\n' "${CONFIG_DIR}/config.toml"
else
    printf 'Keeping existing configuration: %s\n' "${CONFIG_DIR}/config.toml"
fi

install -Dm0644 "${SCRIPT_DIR}/share/applications/${APP_ID}.desktop" \
    "${SHARE_DIR}/applications/${APP_ID}.desktop"
install -Dm0644 "${SCRIPT_DIR}/share/metainfo/${APP_ID}.metainfo.xml" \
    "${SHARE_DIR}/metainfo/${APP_ID}.metainfo.xml"
install -Dm0644 "${SCRIPT_DIR}/share/icons/hicolor/scalable/apps/${APP_ID}.svg" \
    "${SHARE_DIR}/icons/hicolor/scalable/apps/${APP_ID}.svg"
for size in 128 256; do
    install -Dm0644 \
        "${SCRIPT_DIR}/share/icons/hicolor/${size}x${size}/apps/${APP_ID}.png" \
        "${SHARE_DIR}/icons/hicolor/${size}x${size}/apps/${APP_ID}.png"
done

install -d -m 0755 "${ASSET_DIR}/shell-integration" "${ASSET_DIR}/workflows"
install -m 0644 "${SCRIPT_DIR}/share/jterm4/shell-integration/README.md" \
    "${SCRIPT_DIR}"/share/jterm4/shell-integration/jterm4.* \
    "${ASSET_DIR}/shell-integration/"
install -m 0644 "${SCRIPT_DIR}"/share/jterm4/workflows/*.yaml \
    "${ASSET_DIR}/workflows/"
install -Dm0644 "${SCRIPT_DIR}/share/jterm4/notebooks/welcome.jtnb.md" \
    "${ASSET_DIR}/notebooks/welcome.jtnb.md"

install -Dm0644 "${SCRIPT_DIR}/share/doc/jterm4/README.md" "${DOC_DIR}/README.md"
install -Dm0644 "${SCRIPT_DIR}/share/doc/jterm4/Cargo.lock" "${DOC_DIR}/Cargo.lock"
install -Dm0644 "${SCRIPT_DIR}/share/doc/jterm4/BUILDINFO" "${DOC_DIR}/BUILDINFO"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "${SHARE_DIR}/applications" >/dev/null 2>&1 || true
fi

printf '\njterm4 installation complete.\n'
printf '  Binary:            %s\n' "${BIN_DIR}/jterm4"
printf '  Support bundle:    %s\n' "${BIN_DIR}/jterm4-support-bundle"
printf '  Configuration:     %s\n' "${CONFIG_DIR}/config.toml"
printf '  Runtime assets:    %s\n' "${ASSET_DIR}"
printf '\nMake sure %s is in PATH, then run jterm4 --doctor.\n' "${BIN_DIR}"

