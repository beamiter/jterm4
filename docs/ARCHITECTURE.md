# jterm4 Architecture

## Process entry and headless commands

`src/bin/jterm4.rs` is intentionally tiny and delegates to the library application. `src/cli.rs` handles help, version, human/JSON config validation and diagnostics, config initialization/backup recovery, shell-integration output, and launch overrides before GTK initialization. Headless operations therefore remain usable over SSH and in CI. A normal launch enters `src/main.rs`, which builds the libadwaita application and the shared `UiState`.

## Terminal models

jterm4 has two explicit terminal models:

- **VTE mode** attaches a GTK VTE widget directly to a PTY and supports conventional split panes.
- **Block mode**, the default, owns its PTY and reader lifecycle in `TermView`, parses shell integration markers, and renders commands and output as searchable finished blocks plus one live terminal.

`PaneLeaf` is the backend-neutral ownership boundary. It exposes the visible root, live input surface, PTY/shell process probe, cwd/restorable command, focus, and teardown without discovering an arbitrary nested VTE widget. Splitting a Block leaf retains that Block controller and creates a conventional VTE sibling; nested VTE splits use the same pane tree. Close paths scan every leaf, restore zoomed trees before mutation, confirm foreground work, and terminate the corresponding process group.

## UI composition

`UiState` coordinates tabs, panes, sidebars, search, configuration reloads, and actions. The file sidebar uses GTK4's supported model-view stack: `gio::ListStore`, `TreeListModel`, `ListView`, and `TreeExpander`. Directory scans run on named worker threads; generation checks prevent stale results from repopulating a changed root.

The unified palette keeps fuzzy ranking in a pure data layer and combines actions, metadata-only JSONL command history, YAML/TOML workflows, and the review-first natural-language command entry. Prefixes select one source without creating separate search implementations.

The native Shell Agent dashboard captures one active Block `TermView` as its target and surfaces its cwd, provider/model, shell, safety state, turn budget, transcript, and the persistent review-first command-correction toggle. Selected Block data and pane environment metadata are byte-bounded, JSON-encoded, and carried only as explicitly untrusted user-role context; truncation is visible in the context chip. A pure bounded state machine accepts only strict JSON `say`, `run`, or `done` actions and compacts its own transcript to 128 KiB/128 entries; unknown fields, prose, stale proposal IDs, invalid transitions, multi-line/control-character commands, and malformed transitions fail closed. The GTK layer renders editable proposal cards and is the only layer allowed to turn an explicit **Approve & Run** click into PTY input. It preserves the exact approved visible text, recomputes risk after edits and common shell wrappers, requires a second exact-command confirmation for recognized dangerous operations, refuses approval unless the pinned prompt is idle and empty, then correlates the resulting finished block before feeding its bounded observation back into the next turn.

The AI panel presents one reusable, 1 MiB-bounded visible transcript/composer and a searchable Chats library rather than retaining a GTK widget tree per conversation. A stable chat ID selects the model rendered into that surface. **New chat** inserts and selects a new record without mutating older chats; automatic titles and explicit rename, archive/unarchive, and confirmed-delete transitions are model operations. Draft text, provider-bound Block context, archive state, request generation, cancellation and retry payloads belong to each chat, not to the shared widgets. Active and pending-retry Block context is always represented by a clearable chip; shutdown promotes memory-only Block retry state into durable draft/context.

## Asynchronous boundaries

GTK widgets are only mutated on the main context. PTY reads, directory enumeration, Git metadata, notifications, AI/Agent transport, Notebook cells, and remote work happen outside the UI thread and return bounded results. AI transport has a four-request global permit bound, cancellable queueing, bounded request history/response pipes, and a short shutdown barrier that lets the worker kill and reap curl before application teardown. Every AI response carries its originating stable chat ID and request generation: switching chats only changes rendering, while stopping or deleting a chat makes its late response a no-op. Agent turn cancellation uses the same transport token while keeping session cancellation distinct; neither path silently authorizes or reinterprets a late reply. Notebook cells run in isolated process groups, stream stdout/stderr through bounded UI updates, and are cancelled as a group. Callbacks that outlive a tab are explicitly removed or guarded by weak references/generations.

## Persistence

Each process owns one active window snapshot. A snapshot becomes available to a future process only after graceful shutdown, using an atomic rename from `.active` to `.state`. Launches atomically claim one ready snapshot, stale active snapshots are recovered after their owner PID exits, and legacy `tabs.state` is migrated without allowing two processes to restore it. Ready snapshots are retained with a bounded count. The same snapshot carries AI schema v2: the current selection plus at most 50 stable chat metadata records, each with archive state, draft, provider-bound Block context, and at most 100 complete user/assistant turns. Schema v1 single-chat payloads migrate automatically; malformed or future AI payloads are discarded independently so tab recovery still succeeds. In-flight user turns never become apparently completed restored conversations.

The entire compact AI collection, not each chat independently, has an 8 MiB JSON ceiling. Collection compaction preserves chat metadata and removes only the oldest complete user/assistant pairs, never an in-flight trailing user turn, while marking affected chats `truncated` so data loss is visible. The line-oriented state envelope keeps the existing 20 MiB Pane/Tab budget and adds a dedicated 64 KiB metadata reserve; exact escaped-size compaction uses that headroom and fails closed against the last known-good file if invariants cannot be met. Persistence redaction is collection-wide and includes active, non-active, archived, and draft/context data. In-flight questions are represented as recoverable drafts, never completed turns, and final window close flushes the draft debounce. Safe mode neither restores nor publishes the collection; `--no-restore` retains its existing fresh, persistable-workspace behavior.

Session snapshots and Block history use owner-only Unix permissions. File contents are written to a uniquely created sibling temporary file, synced, atomically renamed, and followed by a directory sync where applicable. Parsers retain legacy compatibility and reject pathological record sizes. Safe mode and fresh-workspace launches do not restore the AI payload. The separate JSONL command index deliberately stores no output and enforces record/file bounds.

## Native and Flatpak execution boundary

Native builds execute shells and helper tools directly. Flatpak builds keep the
GTK process sandboxed but route interactive shells, SSH, Git metadata, curl, and
notifications through a single `host` module backed by
`flatpak-spawn --host --watch-bus`. Cwd and selected environment values are
encoded as argv options before process creation, so VTE and Block PTYs share the
same explicit host boundary and cleanup rules. The stable application ID is
`io.github.beamiter.jterm4`.

The Flatpak is intentionally granted host filesystem and command access because a
terminal emulator is not a command sandbox. That authority is documented and
validated rather than hidden behind a package that only works inside its own
container. Runtime assets live under `/app/share/jterm4`; explicit environment
paths make them discoverable without copying examples into user data.

## Configuration and observability

Configuration is parsed and semantically validated before replacing the active runtime value. Writes take an advisory lock, compare the exact loaded revision, preserve two valid rotating backups, and use private durable atomic replacement. Conflicts and invalid TOML cannot overwrite a newer disk revision, and UI-originated failures are surfaced without discarding the active in-memory value. File monitoring provides hot reload while manual reload remains available. Safe mode bypasses the external config path and environment overrides entirely, creates the built-in VTE profile plus default keymap, and rejects manual reload.

The lightweight logger supports plain levels and target-specific `RUST_LOG` directives; each line includes relative time, severity, and target. `--doctor` reports configuration validation/permissions/backups/lock state, display/input environment, optional tools, shell choice, AI provider readiness, workflow/Notebook discovery, remote readiness, and ready/active snapshot counts without network probes or snapshot contents; `--json` makes diagnostics machine-readable. The support-bundle wrapper enables a path/value-redacted diagnostic mode and adds only non-content file metadata and non-sensitive system characteristics.

## Quality gates

CI runs rustfmt, tests, strict Clippy, Rustdoc with warnings denied, a release build, shell syntax/ShellCheck, and RustSec auditing. Dependabot covers Cargo and GitHub Actions. Pure helpers should have unit tests; GTK behavior that cannot be automated belongs in the acceptance checklist and pull-request validation notes.

## Invariants

1. A visible terminal owns every live PTY; closing a tab terminates its process group.
2. Background work never directly mutates GTK widgets.
3. Concurrent processes never claim or overwrite the same ready session snapshot.
4. Persisted terminal data is bounded, atomically replaced, and owner-only.
5. Invalid configuration never replaces the last valid runtime configuration.
6. Generated commands are never submitted to a PTY without an explicit user action.
7. Agent approval requires the exact pinned Block prompt to be idle and empty; every completed result is correlated to the approved proposal.
8. New features must preserve both VTE and Block input routing or explicitly document a mode limitation.
9. An AI response may update only its originating extant chat; switching or deleting that chat cannot redirect or resurrect the response.
10. Terminal/Block bytes are bounded and carried as untrusted user-role data, never interpolated into AI system instructions.
11. Stopping AI work terminates its transport child; stopping an Agent model turn never terminates an already approved shell command.
