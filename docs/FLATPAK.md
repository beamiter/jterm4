# Flatpak packaging and host integration

jterm4's Flatpak application ID is `io.github.beamiter.jterm4`. The manifest is
`packaging/flatpak/io.github.beamiter.jterm4.yml` and targets the GNOME 50
runtime. Cargo dependencies are pinned by the committed
`packaging/flatpak/cargo-sources.json` generated from `Cargo.lock`.

## Why a host bridge is required

A terminal emulator is useful only when its shell and command-line tools operate
on the user's host environment. Inside Flatpak, jterm4 therefore launches shells,
SSH, Git metadata probes, `curl`, and `notify-send` through
`flatpak-spawn --host --watch-bus`. Native installations continue to execute
those programs directly. Both paths use the same PTY, backpressure, input, and
process-cleanup code.

The Flatpak package is not a containment boundary for terminal commands. Opening
a shell intentionally grants that shell normal host-user authority. The sandbox
still isolates the GTK application process and makes its host access explicit.

## Permissions

The manifest requests:

- Wayland and fallback X11 sockets, IPC sharing, and DRI for GTK rendering.
- `--filesystem=host` so the file tree and reported working directories can show
  host projects. Flatpak still excludes several system paths from this shortcut.
- `--talk-name=org.freedesktop.Flatpak` for `flatpak-spawn --host`.
- SSH agent and network access for remote sessions and the optional AI/Agent UI.

OSC 52 clipboard writes remain disabled by jterm4 unless the user explicitly
enables them. AI-bound terminal text is still redacted by default.

The package installs shell integration, example workflows, and the welcome
notebook below `/app/share/jterm4`. `JTERM4_ASSET_DIR` and
`JTERM4_WORKFLOW_DIR` are fixed to those read-only package paths so built-in
content is available without copying it into the user's writable data area.
User workflows in the per-app config directory continue to take precedence.

The interactive terminal itself runs on the host. Its rc file therefore cannot
reliably source `/app/share/...` directly, because `/app` belongs to the Flatpak
mount namespace. Load the embedded script through the installed application ID
instead. For bash, add this to the host `~/.bashrc`:

```bash
if [[ $TERM_PROGRAM == jterm4 ]]; then
    source <(flatpak run io.github.beamiter.jterm4 --shell-integration bash)
fi
```

Use `zsh` in the command for `~/.zshrc`. Fish can use
`flatpak run io.github.beamiter.jterm4 --shell-integration fish | source` inside
the equivalent `TERM_PROGRAM` guard. The child environment is set before the rc
file is read, for both Block and VTE backends.

## Build

Install Flatpak and flatpak-builder, add Flathub, then run:

```bash
flatpak remote-add --user --if-not-exists flathub \
  https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak-builder --user --install-deps-from=flathub --force-clean \
  --disable-rofiles-fuse --repo=flatpak-repo flatpak-build \
  packaging/flatpak/io.github.beamiter.jterm4.yml
flatpak build-bundle flatpak-repo io.github.beamiter.jterm4.flatpak \
  io.github.beamiter.jterm4
sha256sum io.github.beamiter.jterm4.flatpak
```

CI regenerates the Cargo source manifest, validates the desktop and AppStream
metadata, builds the bundle, records its SHA-256 checksum, and launches both VTE
and Block modes under headless X11 and Wayland sessions.

## Install and diagnose

```bash
flatpak --user install ./io.github.beamiter.jterm4.flatpak
flatpak run io.github.beamiter.jterm4 --doctor
flatpak run --command=jterm4-support-bundle io.github.beamiter.jterm4 "$PWD"
flatpak run io.github.beamiter.jterm4
```

The support archive command performs no network requests and records neither
host paths nor terminal/configuration contents. Review its files before sharing.

Flatpak applications do not automatically inherit arbitrary host environment
variables. To use AI, provide `JTERM4_AI_API_KEY` or the selected provider's
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or `OLLAMA_API_KEY` through a trusted
launcher or an explicit Flatpak override. Alternatively, set
`ai_api_key_file` to an owner-only file visible inside the sandbox (normally
under the app's Flatpak config directory). Only its path is stored in
`config.toml`; API key contents remain in the separate `0600` file.

AI `curl` requests use the host-command bridge. Notebook `shell`/unlabelled cells
use the configured shell argv (including its host wrapper when present), while an
explicit interpreter fence selects that interpreter by name. Executable notebooks
must therefore be treated as trusted code, not as sandboxed snippets; review a
`.jtnb.md` file before pressing Run or Run All.

## Known boundary

OSC 7 is the authoritative working-directory signal in Flatpak. `/proc` fallbacks
and foreground-process inspection can only see the sandbox-side
`flatpak-spawn` helper, so integrations that omit OSC 7 may have less precise
process names or current-directory recovery. This does not affect command I/O.

The project license remains an explicit owner decision tracked separately. The
AppStream metadata uses `LicenseRef-proprietary` until that decision is made; the
Flatpak is intended for testing and direct project distribution, not Flathub
submission, until the license issue is resolved.
