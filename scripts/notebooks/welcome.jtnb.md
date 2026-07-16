# Welcome to jterm4 notebooks

A `.jtnb.md` file is ordinary **Markdown** with executable shell fences.
Run one cell with *Run*, execute all runnable cells in order with *Run All*, or
cancel a cell without leaving child processes behind.

Notebook commands run in isolated process groups. They do not type into or
modify the active terminal. Output, errors, and exit status stay with the cell.

## Explicit interpreters

A `bash` fence runs with `bash`; a `sh` fence runs with `sh`:

```bash
printf 'hello from bash\n'
printf 'working directory: %s\n' "$PWD"
```

```sh
uname -srm
```

`shell` and unlabeled fences use the shell argv supplied by jterm4. The source
is sent to that shell on standard input, so configured login/host wrappers are
preserved without constructing a fragile quoted `-c` command.

```shell
printf 'configured shell: %s\n' "${SHELL:-unknown}"
```

## stdout, stderr, and exit status

The two streams are shown separately. A non-zero exit status highlights the
cell but does not prevent later cells in *Run All* from running.

```bash
echo 'ordinary output'
echo 'diagnostic output' >&2
exit 7
```

## Cancellation

Use *Stop* or *Stop All* to terminate the cell's complete process group. Closing
the notebook performs the same cleanup.

```bash
echo 'starting a 30 second task'
sleep 30
echo 'finished'
```

## Display-only fences

Non-shell languages remain readable and copyable but are never executed.

```python
print("display only")
```

## Markdown scope

The lightweight viewer styles `# / ## / ###` headings, `**bold**`, `*italic*`,
and `` `inline code` ``. More advanced Markdown remains visible as plain text.
