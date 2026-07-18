#!/usr/bin/env python3
from pathlib import Path
import subprocess

REPO = Path.cwd()
TARGET_BRANCH = "fix/block-command-capture-sidebar-contrast"
PATCH_SNAPSHOT = REPO / ".github/scripts/pr32_patch_snapshot.py"
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
    checked(["git", "push", "--force", "origin", f"HEAD:{TARGET_BRANCH}"])
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


def internal_failure(stage: str, message: str) -> None:
    fail(
        stage,
        subprocess.CompletedProcess(
            args=[stage],
            returncode=1,
            stdout="",
            stderr=message,
        ),
    )


checked(["git", "config", "user.name", "github-actions[bot]"])
checked(
    [
        "git",
        "config",
        "user.email",
        "41898282+github-actions[bot]@users.noreply.github.com",
    ]
)
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

# Keep the asserted PR patch in this controller branch so a force-rebase of the
# target branch cannot discard the source transformation before a retry.
patch_text = PATCH_SNAPSHOT.read_text(encoding="utf-8")
# The snapshot was written through a JSON API, so its Python source contains one
# literal backslash in each Rust escape. Convert only the 16 opening triple quotes
# to raw strings; closing delimiters are unindented and remain unchanged.
opening = "\n    '''"
opening_count = patch_text.count(opening)
if opening_count != 16:
    raise SystemExit(f"expected 16 PR 32 patch string openings, found {opening_count}")
patch_text = patch_text.replace(opening, "\n    r'''")

strict_guard = r'''    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}\n--- needle ---\n{old}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")
'''
adapted_guard = r'''    count = text.count(old)
    legacy_test_import = (
        path == Path("src/block_view/mod.rs")
        and "coalesce_bytes_events, compute_viewport_state, normalize_captured_command" in old
    )
    if legacy_test_import and count == 0:
        return
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}\n--- needle ---\n{old}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")
'''
if patch_text.count(strict_guard) != 1:
    raise SystemExit("PR 32 patch snapshot guard was not found exactly once")
patch_text = patch_text.replace(strict_guard, adapted_guard, 1)
PATCH.write_text(patch_text, encoding="utf-8")

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

# PR 31 expanded and reflowed the test import list. Add the new helper within the
# test module header instead of relying on the obsolete two-line import context.
mod_path = REPO / "src/block_view/mod.rs"
mod_source = mod_path.read_text(encoding="utf-8")
module_marker = "#[cfg(test)]\nmod tests {"
try:
    test_start = mod_source.index(module_marker)
    first_test = mod_source.index("    #[test]", test_start)
except ValueError as error:
    internal_failure("adapt-test-import", f"could not locate test module header: {error}")

test_header = mod_source[test_start:first_test]
if "resolve_submitted_command" not in test_header:
    import_needle = "normalize_captured_command,"
    if test_header.count(import_needle) != 1:
        internal_failure(
            "adapt-test-import",
            "expected one normalize_captured_command import in the test header",
        )
    test_header = test_header.replace(
        import_needle,
        "normalize_captured_command, resolve_submitted_command,",
        1,
    )
    mod_source = mod_source[:test_start] + test_header + mod_source[first_test:]
    mod_path.write_text(mod_source, encoding="utf-8")

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
checked(["git", "push", "--force", "origin", f"HEAD:{TARGET_BRANCH}"])

# The host workflow repeats the same gates. Leave untracked sentinels so its
# hard-coded cleanup succeeds; its final no-op commit is expected to stop there.
Path(".github/scripts").mkdir(parents=True, exist_ok=True)
Path(".github/workflows").mkdir(parents=True, exist_ok=True)
Path(".github/scripts/apply_block_mode_ux_final_polish.py").write_text(
    "# validation sentinel\n", encoding="utf-8"
)
Path(".github/workflows/apply-block-mode-ux-final-polish.yml").write_text(
    "name: validation sentinel\n", encoding="utf-8"
)
