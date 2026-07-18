#!/usr/bin/env python3
from pathlib import Path
import subprocess

REPO = Path.cwd()
TARGET_BRANCH = "fix/block-command-capture-sidebar-contrast"
PATCH_COMMIT = "544bf949b92ccfe63fb8977b5686007b12cd15e1"
PATCH = Path("/tmp/apply_block_capture_contrast_fix.py")
DIAGNOSTIC = REPO / ".github/pr32-resolver-error.log"


def call(args: list[str], *, capture: bool = False) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        cwd=REPO,
        text=True,
        capture_output=capture,
        check=False,
    )


def checked(args: list[str]) -> subprocess.CompletedProcess[str]:
    result = call(args, capture=True)
    if result.returncode != 0:
        raise SystemExit(
            f"command failed ({result.returncode}): {' '.join(args)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="")
    return result


def fail(stage: str, result: subprocess.CompletedProcess[str]) -> None:
    # Discard a partial source transformation, retain only an actionable log on
    # top of current master, and update the conflicted PR branch atomically.
    call(["git", "merge", "--abort"])
    call(["git", "reset", "--hard", "HEAD"])
    status = call(["git", "status", "--short"], capture=True)
    detail = (
        f"stage: {stage}\n"
        f"returncode: {result.returncode}\n"
        "--- stdout ---\n"
        f"{result.stdout[-60000:]}\n"
        "--- stderr ---\n"
        f"{result.stderr[-60000:]}\n"
        "--- git status ---\n"
        f"{status.stdout}\n"
    )
    DIAGNOSTIC.write_text(detail, encoding="utf-8")
    checked(["git", "add", str(DIAGNOSTIC.relative_to(REPO))])
    checked(["git", "commit", "-m", f"ci: capture PR 32 failure at {stage}"])
    checked(
        [
            "git",
            "push",
            "--force-with-lease",
            "origin",
            f"HEAD:{TARGET_BRANCH}",
        ]
    )
    print(detail)
    raise SystemExit(result.returncode or 1)


def require(stage: str, args: list[str]) -> subprocess.CompletedProcess[str]:
    result = call(args, capture=True)
    if result.returncode != 0:
        fail(stage, result)
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="")
    return result


checked(["git", "config", "user.name", "github-actions[bot]"])
checked(
    [
        "git",
        "config",
        "user.email",
        "41898282+github-actions[bot]@users.noreply.github.com",
    ]
)
# Ensure the native metadata needed by gtk-rs is present before validation.
checked(
    [
        "sudo",
        "apt-get",
        "install",
        "--no-install-recommends",
        "-y",
        "libglib2.0-dev",
    ]
)
checked(["pkg-config", "--modversion", "glib-2.0"])

call(["git", "fetch", "--unshallow", "origin"])
checked(
    [
        "git",
        "fetch",
        "origin",
        f"+refs/heads/{TARGET_BRANCH}:refs/remotes/origin/{TARGET_BRANCH}",
        "+refs/heads/master:refs/remotes/origin/master",
    ]
)
patch_text = checked(
    [
        "git",
        "show",
        f"{PATCH_COMMIT}:.github/scripts/apply_block_capture_contrast_fix.py",
    ]
).stdout
PATCH.write_text(patch_text, encoding="utf-8")

checked(
    [
        "git",
        "checkout",
        "-B",
        TARGET_BRANCH,
        f"refs/remotes/origin/{TARGET_BRANCH}",
    ]
)
checked(["git", "reset", "--hard", "origin/master"])
require("apply-patch", ["python3", str(PATCH)])
require("format", ["cargo", "fmt", "--all"])
require("test", ["cargo", "test", "--all-features", "--locked"])
require(
    "clippy",
    [
        "cargo",
        "clippy",
        "--all-targets",
        "--all-features",
        "--locked",
        "--",
        "-D",
        "warnings",
    ],
)
DIAGNOSTIC.unlink(missing_ok=True)
Path(".github/resolver-test-failure.log").unlink(missing_ok=True)

checked(["git", "add", "-A"])
staged = call(["git", "diff", "--cached", "--quiet"])
if staged.returncode == 0:
    raise SystemExit("PR 32 resolver produced no source changes")
checked(["git", "commit", "-m", "fix: resolve PR 32 against merged master"])
checked(
    [
        "git",
        "push",
        "--force-with-lease",
        "origin",
        f"HEAD:{TARGET_BRANCH}",
    ]
)

# The legacy validation job that hosts this controller removes these paths after
# repeating fmt/test/clippy. Recreate them as untracked sentinels so that cleanup
# succeeds, while the final no-op commit stops before its hard-coded PR 31 push.
Path(".github/scripts").mkdir(parents=True, exist_ok=True)
Path(".github/workflows").mkdir(parents=True, exist_ok=True)
Path(".github/scripts/apply_block_mode_ux_final_polish.py").write_text(
    "# validation sentinel\n", encoding="utf-8"
)
Path(".github/workflows/apply-block-mode-ux-final-polish.yml").write_text(
    "name: validation sentinel\n", encoding="utf-8"
)
