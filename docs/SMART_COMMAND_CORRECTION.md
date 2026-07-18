# Smart command correction

jterm4 can offer a reviewable correction after a Block-mode command fails with an error that looks like a typo, an unknown executable, an unknown package, an invalid subcommand, or an invalid option.

The feature can be toggled from the Shell Agent dashboard or Settings and is
persisted as `command_correction_enabled`. It defaults to `true` when absent
from older configuration files; `JTERM4_COMMAND_CORRECTION_ENABLED` can
override it for one launch. Turning it off while a correction is being resolved
also prevents the pending result from opening a dialog.

## Interaction

A correction is never submitted automatically. The confirmation dialog presents the exact candidate command and keeps three distinct choices:

- **Dismiss** leaves the prompt unchanged.
- **Insert only** writes the reviewed command into an empty, idle prompt without pressing Enter.
- **Run corrected command** is an explicit one-command approval. Recognizable destructive commands require a second confirmation.

The candidate remains editable before either action. If the original pane is busy or already contains input by the time the user confirms, jterm4 refuses to overwrite it.

## Resolution order

jterm4 prefers evidence from the command's actual target over model memory:

1. A suggestion printed by the failed tool itself, such as Git's `most similar command` output.
2. For a local `apt`/`apt-get` package error, fuzzy matching against the host's `apt-cache pkgnames` index.
3. For a local `command not found` error, fuzzy matching against commands available in the host PATH.
4. The configured AI provider, using the bounded command, exit status, working directory, and error output, only when the deterministic resolvers do not produce a candidate.

For example, when `sudo apt install fmpg` fails with `Unable to locate package fmpg`, a host whose package index contains `ffmpeg` can offer `sudo apt install ffmpeg` as a verified candidate while preserving the user's wrapper, flags, quoting, and any other package names.

## Local and remote targets

A local package or PATH index must never be presented as evidence about an rsh/SSH target. Remote panes therefore skip local APT and PATH probes. A suggestion emitted by the remote tool itself is still target evidence; an AI-only remote suggestion is visibly marked as unverified on that target.

A future rsh control channel can add remote read-only probes without injecting hidden commands into the interactive PTY.

## Model protocol and safety boundary

The AI fallback must return exactly one strict JSON object with either a single suggestion or no suggestion. Before presenting a model candidate, jterm4 rejects responses that are multiline, unchanged, oversized, or contain terminal control characters. It also rejects candidates that newly add:

- `sudo`, `doas`, or `su`;
- `ssh` remote execution;
- pipes, command separators, redirects, or command substitution.

These checks limit silent authority expansion by the model. They do not attempt to prove that arbitrary shell text is harmless, which is why every candidate remains review-first and destructive patterns retain an additional warning.

## Scope

The automatic monitor currently applies to Block panes, where shell integration supplies reliable command boundaries, exit status, working directory, and bounded output. Ordinary build failures, test failures, and network failures do not trigger correction unless their output also contains one of the narrow typo-shaped error signals. They remain available to the normal AI Block analysis flow.
