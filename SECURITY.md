# Security Policy

## Supported versions

Security fixes are developed against the current `master` branch and included in the next release. Older snapshots are not maintained as separate security branches.

## Reporting a vulnerability

Use GitHub's **Report a vulnerability** / private vulnerability reporting flow when it is available for this repository. If that option is unavailable, open a minimal issue asking the maintainer for a private contact channel. Do not include exploit details, credentials, private hostnames, terminal history, or captured output in a public issue.

A useful private report contains the affected commit or version, environment, reproduction steps, impact, and any proposed mitigation. The acknowledgement target is three business days; remediation timing depends on severity and complexity.

## Sensitive surfaces

Pay particular attention to PTY lifecycle and process-group signalling, OSC/clipboard handling, SSH and reconnect flows, AI-context redaction, configuration parsing, session snapshots, and Block history files. Session snapshots and Block history are owner-only on Unix (`0700` directories and `0600` files), but users should still avoid persisting secrets in terminal history.

## Disclosure

Please allow time for a fix and coordinated release before public disclosure. Confirmed vulnerabilities may receive a GitHub security advisory and CVE when appropriate.

## Flatpak host authority

The Flatpak intentionally uses `flatpak-spawn --host` and host filesystem access
so terminal commands operate on the user's real account. Reports about unintended
privilege expansion, command-argument confusion, leaked environment values, or a
host process surviving after its owning pane closes are security-sensitive.
