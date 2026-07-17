# Security Policy

## Supported versions

Security fixes are developed against the current `master` branch and included in the next release. Older snapshots are not maintained as separate security branches.

## Reporting a vulnerability

Use GitHub's **Report a vulnerability** / private vulnerability reporting flow when it is available for this repository. If that option is unavailable, open a minimal issue asking the maintainer for a private contact channel. Do not include exploit details, credentials, private hostnames, terminal history, or captured output in a public issue.

A useful private report contains the affected commit or version, environment, reproduction steps, impact, and any proposed mitigation. The acknowledgement target is three business days; remediation timing depends on severity and complexity.

## Sensitive surfaces

Pay particular attention to PTY lifecycle and process-group signalling, OSC/clipboard handling, SSH and reconnect flows, AI-context redaction, configuration parsing, session snapshots, and Block history files. Session snapshots include the selected AI chat, active/non-active/archived chat metadata, completed turns, per-chat drafts, and provider-bound Block context. When configured, redaction applies to every retained chat rather than only the visible one. Background model results are correlated with the originating chat; deleting that chat invalidates any late result so it cannot recreate or contaminate another conversation.

The AI collection is capped at 50 chat metadata records and 100 turns per chat, while its complete compact JSON remains bounded to 8 MiB by trimming the oldest content and marking the affected chat `truncated`. The 20 MiB workspace-state budget has a separate 64 KiB AI-metadata reserve: when combined state is tight, payload is compacted while every bounded chat row is retained; invariant failures preserve the last known-good file instead of writing a snapshot with the AI line removed. Legacy v1 single-chat payloads are migrated through the same strict validation and redaction boundary. Snapshots and Block history are owner-only on Unix (`0700` directories and `0600` files), but file permissions, size bounds, and optional redaction do not make stored commands/output non-sensitive; users should still avoid persisting secrets. Safe mode does not restore or publish the library, and `--no-restore` starts from a fresh library under its existing fresh-workspace persistence semantics.

## Disclosure

Please allow time for a fix and coordinated release before public disclosure. Confirmed vulnerabilities may receive a GitHub security advisory and CVE when appropriate.

## Flatpak host authority

The Flatpak intentionally uses `flatpak-spawn --host` and host filesystem access
so terminal commands operate on the user's real account. Reports about unintended
privilege expansion, command-argument confusion, leaked environment values, or a
host process surviving after its owning pane closes are security-sensitive.
