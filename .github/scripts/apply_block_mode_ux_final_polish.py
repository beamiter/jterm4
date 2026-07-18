#!/usr/bin/env python3
from pathlib import Path
import subprocess

REPO = Path.cwd()
BRANCH = "fix/block-mode-selection-input-history"
ORIGINAL_COMMIT = "666fa009ebafc5b30c12a921dd4804c90ab441fd"
ORIGINAL = Path("/tmp/apply_block_mode_ux_final_polish_original.py")
DIAGNOSTIC = REPO / ".github/pr31-resolver-error.log"


def call(args: list[str], *, capture: bool = False) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        cwd=REPO,
        text=True,
        capture_output=capture,
        check=False,
    )


def require(stage: str, args: list[str]) -> subprocess.CompletedProcess[str]:
    result = call(args, capture=True)
    if result.returncode == 0:
        if result.stdout:
            print(result.stdout, end="")
        if result.stderr:
            print(result.stderr, end="")
        return result

    # Preserve actionable diagnostics on the PR branch because GitHub may truncate
    # the downloaded job log. Abort only an unfinished merge; a successful merge
    # commit is intentionally retained for the next retry.
    call(["git", "merge", "--abort"])
    call(["git", "reset", "--hard", "HEAD"])
    status = call(["git", "status", "--short"], capture=True)
    detail = (
        f"stage: {stage}\n"
        f"command: {' '.join(args)}\n"
        f"returncode: {result.returncode}\n"
        "--- stdout ---\n"
        f"{result.stdout}\n"
        "--- stderr ---\n"
        f"{result.stderr}\n"
        "--- git status ---\n"
        f"{status.stdout}\n"
    )
    DIAGNOSTIC.write_text(detail, encoding="utf-8")
    call(["git", "add", str(DIAGNOSTIC.relative_to(REPO))])
    call(["git", "commit", "-m", f"ci: capture PR 31 resolver failure at {stage}"])
    call(["git", "push", "origin", f"HEAD:{BRANCH}"])
    print(detail)
    raise SystemExit(result.returncode or 1)


require("configure-name", ["git", "config", "user.name", "github-actions[bot]"])
require(
    "configure-email",
    [
        "git",
        "config",
        "user.email",
        "41898282+github-actions[bot]@users.noreply.github.com",
    ],
)
# actions/checkout created a depth-one clone. Expand it, then explicitly populate
# both remote-tracking refs so `origin/master` is guaranteed to exist.
call(["git", "fetch", "--unshallow", "origin"])
require(
    "fetch-refs",
    [
        "git",
        "fetch",
        "origin",
        f"+refs/heads/{BRANCH}:refs/remotes/origin/{BRANCH}",
        "+refs/heads/master:refs/remotes/origin/master",
    ],
)
original = require(
    "recover-original-patch",
    [
        "git",
        "show",
        f"{ORIGINAL_COMMIT}:.github/scripts/apply_block_mode_ux_final_polish.py",
    ],
).stdout
ORIGINAL.write_text(original, encoding="utf-8")
require(
    "merge-master",
    ["git", "merge", "--no-edit", "-X", "ours", "origin/master"],
)
require("apply-original-patch", ["python3", str(ORIGINAL)])
DIAGNOSTIC.unlink(missing_ok=True)
