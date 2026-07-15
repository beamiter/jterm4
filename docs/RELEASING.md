# Release Process

1. Start from a clean, green `master` branch. Update the version in `Cargo.toml` and move the relevant `CHANGELOG.md` entries from **Unreleased** into a dated version section.
2. Run all commands from `CONTRIBUTING.md`, including the release build and RustSec audit. Validate `jterm4 --doctor`, configuration migration, normal and forced tab close, VTE splits, Block history, Wayland, and X11.
3. Confirm that no temporary migration workflows, source-export markers, local paths, hosts, or secrets are present. Review dependency and GitHub Action updates.
4. Create and push a signed `vX.Y.Z` tag, then draft GitHub release notes from the changelog and merged pull requests.
5. Build `packaging/flatpak/io.github.beamiter.jterm4.yml` from a clean checkout, verify the committed Cargo source manifest, validate desktop/AppStream metadata, and publish the Flatpak bundle together with its SHA-256 file. The raw Cargo binary dynamically links GTK/libadwaita/VTE and must not be described as portable.
6. Install the produced Flatpak in a clean user account. Run `--version`, `--doctor`, VTE and Block launches under Wayland and X11, SSH-agent access, host working-directory/file-tree access, notifications, and AI networking before publishing.
7. After publishing, verify uninstall/data-retention behavior and mark the changelog comparison links.

The crate is intentionally `publish = false` until the project owner selects and documents a license.
