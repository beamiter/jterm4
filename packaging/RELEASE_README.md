# jterm4 relocatable Linux bundle

This archive contains a prebuilt `jterm4` binary plus its desktop metadata,
shell integrations, example workflows, documented configuration, and welcome
notebook. It installs into the current user's `~/.local` prefix; root access is
not required, and an existing `config.toml` is never overwritten.

## Runtime requirements

This is not a statically linked or self-contained portable application. A
compatible graphical Linux system with GTK 4, libadwaita, GTK4 VTE, and PCRE2
runtime libraries is required. Optional integrations include `notify-send`,
OpenSSH, Git, and a configured AI provider.

## Verify, extract, and install

From the directory containing the archive and checksum:

```bash
sha256sum --check jterm4-*.tar.gz.sha256
tar -xzf jterm4-*.tar.gz
cd jterm4-*/
./install.sh
```

The extracted `./uninstall.sh` removes the binary and installed assets while
preserving configuration and state by default. Add `--purge-config` only when
those user files should also be removed.

After installation:

```bash
jterm4 --doctor
jterm4 --doctor --json
jterm4 --check-config
jterm4 --safe-mode
jterm4
```

For support, `jterm4-support-bundle [OUTPUT_DIRECTORY]` creates a
privacy-preserving archive without network access. Review it before sharing.

