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

    call(["git", "merge", "--abort"])
    call(["git", "reset", "--hard", "HEAD"])
    status = call(["git", "status", "--short"], capture=True)
    detail = (
        f"stage: {stage}\n"
        f"command: {' '.join(args)}\n"
        f"returncode: {result.returncode}\n"
        "--- stdout ---\n"
        f"{result.stdout[-60000:]}\n"
        "--- stderr ---\n"
        f"{result.stderr[-60000:]}\n"
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
# The legacy workflow omitted GLib's pkg-config metadata on this runner image.
# Install it explicitly before Cargo invokes glib-sys.
require(
    "install-glib-dev",
    [
        "sudo",
        "apt-get",
        "install",
        "--no-install-recommends",
        "-y",
        "libglib2.0-dev",
    ],
)
require("verify-glib-pkgconfig", ["pkg-config", "--modversion", "glib-2.0"])
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

strict_guard = (
    "    if count != 1:\n"
    "        raise SystemExit(f\"{path}: expected one match, found {count}: {old[:100]!r}\")\n"
)
relaxed_guard = (
    "    if count != 1:\n"
    "        duplicate_arrow_move = (\n"
    "            count == 2\n"
    "            and path == \"src/block_view/mod.rs\"\n"
    "            and old.lstrip().startswith(\"move_finished_block_selection(\")\n"
    "            and \"// Enter recalls\" not in old\n"
    "        )\n"
    "        if not duplicate_arrow_move:\n"
    "            raise SystemExit(f\"{path}: expected one match, found {count}: {old[:100]!r}\")\n"
)
if original.count(strict_guard) != 1:
    raise SystemExit("could not locate the original patch assertion guard")
original = original.replace(strict_guard, relaxed_guard, 1)
ORIGINAL.write_text(original, encoding="utf-8")
require(
    "merge-master",
    ["git", "merge", "--no-edit", "-X", "ours", "origin/master"],
)
require("apply-original-patch", ["python3", str(ORIGINAL)])
require("format-after-patch", ["cargo", "fmt", "--all"])
require("test-after-patch", ["cargo", "test", "--all-features", "--locked"])
require(
    "clippy-after-patch",
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
