# jterm4 Architecture

## Process entry and headless commands

`src/bin/jterm4.rs` is intentionally tiny and delegates to the library application. `src/cli.rs` handles help, version, config validation, path printing, and `--doctor` before GTK initialization, so diagnostics remain usable over SSH and in CI. A normal launch then enters `src/main.rs`, which builds the libadwaita application and the shared `UiState`.

## Terminal models

jterm4 has two explicit terminal models:

- **VTE mode** attaches a GTK VTE widget directly to a PTY and supports native split panes.
- **Block mode** owns its PTY and reader lifecycle in `TermView`, parses shell integration markers, and renders commands and output as searchable finished blocks plus one live terminal.

These modes share configuration, theming, input routing, process cleanup, and session metadata, but they do not pretend to have identical ownership. Block-mode pane splits remain disabled until every pane can own an independent `TermView` and PTY without creating hidden processes.

## UI composition

`UiState` coordinates tabs, panes, sidebars, search, configuration reloads, and actions. The file sidebar uses GTK4's supported model-view stack: `gio::ListStore`, `TreeListModel`, `ListView`, and `TreeExpander`. Directory scans run on named worker threads; generation checks prevent stale results from repopulating a changed root.

## Asynchronous boundaries

GTK widgets are only mutated on the main context. PTY reads, directory enumeration, Git metadata, notifications, and remote work happen outside the UI thread and return bounded results. Callbacks that outlive a tab are explicitly removed or guarded by weak references/generations.

## Persistence

Each process owns one active window snapshot. A snapshot becomes available to a future process only after graceful shutdown, using an atomic rename from `.active` to `.state`. Launches atomically claim one ready snapshot, stale active snapshots are recovered after their owner PID exits, and legacy `tabs.state` is migrated without allowing two processes to restore it. Ready snapshots are retained with a bounded count.

Session snapshots and Block history use owner-only Unix permissions. File contents are written to a sibling temporary file, synced, atomically renamed, and followed by a directory sync where applicable. Parsers retain legacy compatibility and reject pathological record sizes.

## Configuration and observability

Configuration is parsed and validated before replacing the active runtime value. File monitoring provides hot reload while manual reload remains available. The lightweight logger supports plain levels and target-specific `RUST_LOG` directives; each line includes relative time, severity, and target. `--doctor` reports configuration status, display/input environment, optional tools, shell choice, and ready/active snapshot counts without exposing snapshot contents.

## Quality gates

CI runs rustfmt, tests, strict Clippy, Rustdoc with warnings denied, a release build, shell syntax/ShellCheck, and RustSec auditing. Dependabot covers Cargo and GitHub Actions. Pure helpers should have unit tests; GTK behavior that cannot be automated belongs in the acceptance checklist and pull-request validation notes.

## Invariants

1. A visible terminal owns every live PTY; closing a tab terminates its process group.
2. Background work never directly mutates GTK widgets.
3. Concurrent processes never claim or overwrite the same ready session snapshot.
4. Persisted terminal data is bounded, atomically replaced, and owner-only.
5. Invalid configuration never replaces the last valid runtime configuration.
6. New features must preserve both VTE and Block input routing or explicitly document a mode limitation.
