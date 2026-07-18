#!/usr/bin/env python3
from pathlib import Path
import subprocess

REPO = Path.cwd()
ORIGINAL = Path("/tmp/apply_block_mode_ux_final_polish_original.py")


def run(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=REPO, check=check, text=True)


# The existing validation run checks out this branch with a shallow clone. Recover
# the original one-shot patch from the parent commit, merge current master using
# the PR branch for overlapping Block-mode hunks, then apply the focused patch.
run("git", "config", "user.name", "github-actions[bot]")
run(
    "git",
    "config",
    "user.email",
    "41898282+github-actions[bot]@users.noreply.github.com",
)
run("git", "fetch", "--unshallow", "origin", check=False)
run("git", "fetch", "origin", "fix/block-mode-selection-input-history", "master")
original = subprocess.run(
    ["git", "show", "HEAD^:.github/scripts/apply_block_mode_ux_final_polish.py"],
    cwd=REPO,
    check=True,
    text=True,
    capture_output=True,
).stdout
ORIGINAL.write_text(original, encoding="utf-8")
run("git", "merge", "--no-edit", "-X", "ours", "origin/master")
run("python3", str(ORIGINAL))
