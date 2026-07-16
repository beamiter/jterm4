# jterm4 shell integration

Source the file matching your shell from its rc file:

| Shell | File | Source from |
|---|---|---|
| bash | `jterm4.bash` | `~/.bashrc` |
| zsh | `jterm4.zsh` | `~/.zshrc` |
| fish | `jterm4.fish` | `~/.config/fish/config.fish` |
| PowerShell | `jterm4.ps1` | `$PROFILE` |

The easiest setup does not require locating these files:

```bash
source <(jterm4 --shell-integration bash)
```

Replace `bash` with `zsh`, `fish`, or `pwsh`. The source installer also places
these files under `${prefix}/share/jterm4/shell-integration/`; the Flatpak copy
is under `/app/share/jterm4/shell-integration/`.

Flatpak terminals launch the interactive shell on the host, where the sandbox's
`/app` path is not a dependable rc-file source. Use the application ID from the
host rc instead:

```bash
[[ $TERM_PROGRAM == jterm4 ]] && \
    source <(flatpak run io.github.beamiter.jterm4 --shell-integration bash)
```

For fish, use
`flatpak run io.github.beamiter.jterm4 --shell-integration fish | source` inside
the equivalent `TERM_PROGRAM` guard.

Each script emits OSC 133 (FTCS) command lifecycle marks and OSC 7 working
directory updates. Those marks let Block mode attribute output to commands and
record exact exit codes. Other terminals silently ignore the sequences.
