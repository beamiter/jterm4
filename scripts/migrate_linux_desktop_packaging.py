#!/usr/bin/env python3
"""Add reproducible Linux desktop packaging and a Flatpak host-command bridge."""

from __future__ import annotations

from pathlib import Path
from textwrap import dedent

ROOT = Path(__file__).resolve().parents[1]
APP_ID = "io.github.beamiter.jterm4"


def write(path: str, content: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(dedent(content).lstrip().rstrip() + "\n")


def replace_once(path: str, old: str, new: str) -> None:
    target = ROOT / path
    text = target.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one match in {path}, found {count}: {old[:120]!r}")
    target.write_text(text.replace(old, new, 1))


def replace_section(path: str, start: str, end: str, replacement: str) -> None:
    target = ROOT / path
    text = target.read_text()
    start_index = text.find(start)
    if start_index < 0:
        raise SystemExit(f"missing section start in {path}: {start!r}")
    end_index = text.find(end, start_index)
    if end_index < 0:
        raise SystemExit(f"missing section end in {path}: {end!r}")
    target.write_text(text[:start_index] + dedent(replacement).lstrip() + text[end_index:])


write(
    "src/host.rs",
    r'''
    //! Host integration for native and Flatpak launches.
    //!
    //! A terminal emulator packaged as Flatpak must not silently start a shell
    //! inside the application sandbox. In Flatpak mode, interactive shells and
    //! optional helper commands are routed through `flatpak-spawn --host`; native
    //! builds keep their existing direct-exec behavior.

    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    pub const APP_ID: &str = "io.github.beamiter.jterm4";

    pub(crate) fn is_flatpak() -> bool {
        static VALUE: OnceLock<bool> = OnceLock::new();
        *VALUE.get_or_init(|| {
            std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").is_file()
        })
    }

    fn find_executable_in_path(name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(name))
                .find(|candidate| candidate.is_file())
        })
    }

    pub(crate) fn bridge_available() -> bool {
        !is_flatpak()
            || Path::new("/usr/bin/flatpak-spawn").is_file()
            || find_executable_in_path("flatpak-spawn").is_some()
    }

    fn wrap_argv_for(
        flatpak: bool,
        argv: &[String],
        cwd: Option<&str>,
        env_extra: &[(&str, &str)],
    ) -> Vec<String> {
        if !flatpak {
            return argv.to_vec();
        }

        let mut wrapped = vec![
            "flatpak-spawn".to_string(),
            "--host".to_string(),
            "--watch-bus".to_string(),
        ];
        if let Some(cwd) = cwd.filter(|value| !value.is_empty()) {
            wrapped.push(format!("--directory={cwd}"));
        }
        wrapped.push("--env=TERM=xterm-256color".to_string());
        for (key, value) in env_extra {
            wrapped.push(format!("--env={key}={value}"));
        }
        wrapped.extend(argv.iter().cloned());
        wrapped
    }

    pub(crate) fn wrap_argv(
        argv: &[String],
        cwd: Option<&str>,
        env_extra: &[(&str, &str)],
    ) -> Vec<String> {
        wrap_argv_for(is_flatpak(), argv, cwd, env_extra)
    }

    pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
        if is_flatpak() {
            let mut command = Command::new("flatpak-spawn");
            command.args(["--host", "--watch-bus"]);
            command.arg(program);
            command
        } else {
            Command::new(program)
        }
    }

    pub(crate) fn command_with_cwd(program: impl AsRef<OsStr>, cwd: &Path) -> Command {
        if is_flatpak() {
            let mut command = Command::new("flatpak-spawn");
            command.args(["--host", "--watch-bus"]);
            command.arg(format!("--directory={}", cwd.display()));
            command.arg(program);
            command
        } else {
            let mut command = Command::new(program);
            command.current_dir(cwd);
            command
        }
    }

    pub(crate) fn command_available(name: &str) -> bool {
        if !is_flatpak() {
            return find_executable_in_path(name).is_some();
        }
        if !bridge_available() {
            return false;
        }

        command("sh")
            .args([
                "-lc",
                "command -v -- \"$1\" >/dev/null 2>&1",
                "jterm4-host-probe",
                name,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn native_argv_is_unchanged() {
            let argv = vec!["bash".to_string(), "-l".to_string()];
            assert_eq!(wrap_argv_for(false, &argv, Some("/tmp"), &[("LESS", "R")]), argv);
        }

        #[test]
        fn flatpak_argv_routes_cwd_and_environment_to_host() {
            let argv = vec!["bash".to_string(), "-l".to_string()];
            assert_eq!(
                wrap_argv_for(true, &argv, Some("/home/alice/project"), &[("LESS", "R")]),
                vec![
                    "flatpak-spawn",
                    "--host",
                    "--watch-bus",
                    "--directory=/home/alice/project",
                    "--env=TERM=xterm-256color",
                    "--env=LESS=R",
                    "bash",
                    "-l",
                ]
            );
        }
    }
    ''',
)

replace_once(
    "src/lib.rs",
    "pub mod git_meta;\npub mod keybindings;",
    "pub mod git_meta;\npub mod host;\npub mod keybindings;",
)
replace_once(
    "src/main.rs",
    '.application_id("app.jterm4")',
    ".application_id(crate::host::APP_ID)",
)

replace_section(
    "src/config.rs",
    "pub(crate) fn choose_shell_argv(configured_shell: Option<&str>) -> Vec<String> {",
    "\n\n#[cfg(test)]",
    r'''
    fn choose_flatpak_host_shell_argv(configured_shell: Option<&str>) -> Vec<String> {
        if let Some(shell) = configured_shell.filter(|value| !value.trim().is_empty()) {
            let shell_name = Path::new(shell)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if shell_name == "rsh" && crate::host::command_available("bash") {
                return vec![
                    "bash".to_string(),
                    "-ic".to_string(),
                    format!("exec {}", shell_single_quote(shell)),
                ];
            }
            return vec![shell.to_string()];
        }

        if crate::host::command_available("rsh") {
            if crate::host::command_available("bash") {
                return vec![
                    "bash".to_string(),
                    "-ic".to_string(),
                    "exec rsh".to_string(),
                ];
            }
            return vec!["rsh".to_string()];
        }

        if let Some(shell) = std::env::var("SHELL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            return vec![shell, "-l".to_string()];
        }
        if crate::host::command_available("bash") {
            return vec!["bash".to_string(), "-l".to_string()];
        }
        vec!["sh".to_string()]
    }

    pub(crate) fn choose_shell_argv(configured_shell: Option<&str>) -> Vec<String> {
        if crate::host::is_flatpak() {
            return choose_flatpak_host_shell_argv(configured_shell);
        }

        // Explicit config / env var wins (needed when PATH is stripped by launchers like wofi).
        if let Some(path) = configured_shell {
            if is_executable(Path::new(path)) {
                let shell_name = Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if shell_name == "rsh" {
                    if let Some(argv) = wrap_rsh_argv_in_interactive_bash(path) {
                        return argv;
                    }
                }
                return vec![path.to_string()];
            }
            log::warn!(
                "Configured shell '{}' is not executable, falling back to auto-detection",
                path
            );
        }

        // Prefer rsh when it's on PATH.
        if let Some(rsh_path) = find_executable_in_path("rsh") {
            if let Some(argv) = wrap_rsh_argv_in_interactive_bash(&rsh_path.to_string_lossy()) {
                return argv;
            }
            return vec![rsh_path.to_string_lossy().to_string()];
        }

        // Fallback: bash
        if let Some(bash_path) = find_executable_in_path("bash") {
            return vec![bash_path.to_string_lossy().to_string(), "-l".to_string()];
        }

        // Last resort: POSIX sh
        vec!["sh".to_string()]
    }
    ''',
)

replace_once(
    "src/pty.rs",
    r'''    pub fn spawn(argv: &[&str], cwd: Option<&str>, env_extra: &[(&str, &str)]) -> io::Result<Self> {
        let initial_size = nix::pty::Winsize {''',
    r'''    pub fn spawn(argv: &[&str], cwd: Option<&str>, env_extra: &[(&str, &str)]) -> io::Result<Self> {
        let argv_owned: Vec<String> = argv.iter().map(|value| (*value).to_string()).collect();
        let host_bridge = crate::host::is_flatpak();
        let executable_argv = crate::host::wrap_argv(&argv_owned, cwd, env_extra);
        if executable_argv.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty PTY argv"));
        }

        let initial_size = nix::pty::Winsize {''',
)
replace_once(
    "src/pty.rs",
    r'''                if let Some(dir) = cwd {
                    let _ = std::env::set_current_dir(dir);
                }
                for (key, val) in env_extra {
                    unsafe { std::env::set_var(key, val) };
                }
                unsafe { std::env::set_var("TERM", "xterm-256color") };

                let c_argv: Vec<CString> = argv.iter().map(|a| CString::new(*a).unwrap()).collect();
                let _ = unistd::execvp(&c_argv[0], &c_argv);''',
    r'''                if !host_bridge {
                    if let Some(dir) = cwd {
                        let _ = std::env::set_current_dir(dir);
                    }
                }
                for (key, val) in env_extra {
                    unsafe { std::env::set_var(key, val) };
                }
                unsafe { std::env::set_var("TERM", "xterm-256color") };

                let c_argv: Vec<CString> = executable_argv
                    .iter()
                    .map(|argument| CString::new(argument.as_str()).unwrap())
                    .collect();
                let _ = unistd::execvp(&c_argv[0], &c_argv);''',
)

replace_once(
    "src/terminal.rs",
    r'''    let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();

    // Use empty envv to inherit all environment variables from parent process
    let envv: &[&str] = &[];
    let spawn_flags = SpawnFlags::SEARCH_PATH;
    let cancellable: Option<&Cancellable> = None;
    let home = std::env::var("HOME").ok();
    let working_directory = working_directory.or(home.as_deref());
    let terminal_for_pid = terminal.clone();''',
    r'''    let home = std::env::var("HOME").ok();
    let requested_working_directory = working_directory.or(home.as_deref());
    let argv_vec = crate::host::wrap_argv(&argv_vec, requested_working_directory, &[]);
    let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();

    // Use empty envv to inherit all environment variables from parent process.
    // In Flatpak mode, cwd and host environment forwarding are encoded in the
    // flatpak-spawn argv above instead of being applied to the sandbox helper.
    let envv: &[&str] = &[];
    let spawn_flags = SpawnFlags::SEARCH_PATH;
    let cancellable: Option<&Cancellable> = None;
    let spawn_working_directory = if crate::host::is_flatpak() {
        None
    } else {
        requested_working_directory
    };
    let terminal_for_pid = terminal.clone();''',
)
replace_once(
    "src/terminal.rs",
    "        PtyFlags::DEFAULT,\n        working_directory,\n        &argv,",
    "        PtyFlags::DEFAULT,\n        spawn_working_directory,\n        &argv,",
)

replace_once(
    "src/git_meta.rs",
    "use std::process::{Child, Command, Stdio};",
    "use std::process::{Child, Stdio};",
)
replace_once(
    "src/git_meta.rs",
    r'''    let mut child = Command::new("git")
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;''',
    r'''    let mut command = crate::host::command_with_cwd("git", cwd);
    let mut child = command
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;''',
)

replace_once(
    "src/notify.rs",
    "use std::process::{Command, Stdio};",
    "use std::process::Stdio;",
)
replace_once(
    "src/notify.rs",
    '    let _ = Command::new("notify-send")',
    '    let _ = crate::host::command("notify-send")',
)
replace_once(
    "src/notify.rs",
    '            "--icon=utilities-terminal",',
    '            "--icon=io.github.beamiter.jterm4",',
)

replace_once(
    "src/ai/mod.rs",
    "    use std::process::{Command, Stdio};",
    "    use std::process::Stdio;",
)
replace_once(
    "src/ai/mod.rs",
    '    let mut child = Command::new("curl")',
    '    let mut child = crate::host::command("curl")',
)

replace_section(
    "src/cli.rs",
    "fn doctor() -> bool {",
    "\n\n/// Handle options",
    r'''
    fn doctor() -> bool {
        let path = config_file_path();
        println!("jterm4 {} doctor", env!("CARGO_PKG_VERSION"));
        println!("application id: {}", crate::host::APP_ID);
        println!("config: {}", path.display());

        let config_ok = if path.exists() {
            check_config(&path)
        } else {
            println!("config status: not created (built-in defaults will be used)");
            true
        };

        let flatpak = crate::host::is_flatpak();
        let bridge_ok = crate::host::bridge_available();
        println!("runtime: {}", if flatpak { "flatpak" } else { "native" });
        if flatpak {
            println!(
                "host bridge: {}",
                if bridge_ok { "available" } else { "missing (terminal launch unavailable)" }
            );
        }

        let (ready_snapshots, active_snapshots) = crate::state::session_snapshot_counts();
        println!("session snapshots: {ready_snapshots} ready, {active_snapshots} active");

        let (config, _, _) = load_config();
        let shell = choose_shell_argv(config.shell.as_deref());
        println!("shell: {}", shell.join(" "));
        println!("DISPLAY: {}", env_presence("DISPLAY"));
        println!("WAYLAND_DISPLAY: {}", env_presence("WAYLAND_DISPLAY"));
        println!("GTK_IM_MODULE: {}", env_presence("GTK_IM_MODULE"));

        for (name, purpose) in [
            ("git", "repository status"),
            ("ssh", "remote sessions"),
            ("curl", "AI panel"),
            ("notify-send", "long-command notifications"),
        ] {
            if flatpak {
                if crate::host::command_available(name) {
                    println!("{name}: available on host ({purpose})");
                } else {
                    println!("{name}: not found on host ({purpose} unavailable)");
                }
            } else {
                match find_on_path(name) {
                    Some(found) => println!("{name}: {} ({purpose})", found.display()),
                    None => println!("{name}: not found ({purpose} unavailable)"),
                }
            }
        }
        config_ok && bridge_ok
    }
    ''',
)

write(
    f"data/{APP_ID}.desktop",
    f'''
    [Desktop Entry]
    Type=Application
    Name=jterm4
    GenericName=Terminal Emulator
    Comment=Session-aware GTK4 terminal with structured command blocks
    Exec=jterm4
    Icon={APP_ID}
    Terminal=false
    Categories=System;TerminalEmulator;Development;
    Keywords=terminal;shell;command;developer;ssh;blocks;
    StartupNotify=true
    StartupWMClass={APP_ID}
    X-GNOME-UsesNotifications=true
    Actions=NewWindow;

    [Desktop Action NewWindow]
    Name=New Window
    Exec=jterm4
    ''',
)

write(
    f"data/{APP_ID}.metainfo.xml",
    f'''
    <?xml version="1.0" encoding="UTF-8"?>
    <component type="desktop-application">
      <id>{APP_ID}</id>
      <metadata_license>LicenseRef-proprietary</metadata_license>
      <project_license>LicenseRef-proprietary</project_license>
      <name>jterm4</name>
      <summary>Session-aware GTK4 terminal with structured command blocks</summary>
      <description>
        <p>
          jterm4 combines a traditional VTE terminal with an optional Block mode
          that keeps commands, output, exit status, duration, and working directory
          as searchable structured history.
        </p>
        <ul>
          <li>VTE tabs and split panes with session restoration</li>
          <li>Structured command blocks, search, filtering, bookmarks, and recall</li>
          <li>SSH workflows, an asynchronous file tree, and optional AI assistance</li>
        </ul>
      </description>
      <launchable type="desktop-id">{APP_ID}.desktop</launchable>
      <developer id="io.github.beamiter">
        <name>beamiter</name>
      </developer>
      <url type="homepage">https://github.com/beamiter/jterm4</url>
      <url type="bugtracker">https://github.com/beamiter/jterm4/issues</url>
      <provides>
        <binary>jterm4</binary>
      </provides>
      <recommends>
        <control>keyboard</control>
        <control>pointing</control>
      </recommends>
      <content_rating type="oars-1.1"/>
      <releases>
        <release version="0.2.0" date="2026-07-14">
          <description>
            <p>Modernized GTK4 architecture, persistence, security, and desktop packaging.</p>
          </description>
        </release>
      </releases>
    </component>
    ''',
)

write(
    f"data/{APP_ID}.svg",
    r'''
    <svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512">
      <defs>
        <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stop-color="#6d5dfc"/>
          <stop offset="1" stop-color="#2f65d9"/>
        </linearGradient>
      </defs>
      <rect x="32" y="32" width="448" height="448" rx="104" fill="url(#bg)"/>
      <rect x="82" y="104" width="348" height="304" rx="42" fill="#101522" opacity="0.94"/>
      <circle cx="118" cy="140" r="11" fill="#ff6b6b"/>
      <circle cx="151" cy="140" r="11" fill="#ffd166"/>
      <circle cx="184" cy="140" r="11" fill="#55d68b"/>
      <path d="M133 224l62 52-62 52" fill="none" stroke="#f7f9ff" stroke-width="28" stroke-linecap="round" stroke-linejoin="round"/>
      <path d="M224 330h116" fill="none" stroke="#8fd3ff" stroke-width="28" stroke-linecap="round"/>
    </svg>
    ''',
)

write(
    "packaging/flatpak/io.github.beamiter.jterm4.yml",
    r'''
    app-id: io.github.beamiter.jterm4
    runtime: org.gnome.Platform
    runtime-version: '50'
    sdk: org.gnome.Sdk
    sdk-extensions:
      - org.freedesktop.Sdk.Extension.rust-stable
    command: jterm4

    finish-args:
      - --share=ipc
      - --share=network
      - --socket=wayland
      - --socket=fallback-x11
      - --socket=ssh-auth
      - --device=dri
      - --filesystem=host
      - --talk-name=org.freedesktop.Flatpak

    modules:
      - name: jterm4
        buildsystem: simple
        build-options:
          append-path: /usr/lib/sdk/rust-stable/bin
          env:
            CARGO_HOME: /run/build/jterm4/cargo
            CARGO_NET_OFFLINE: 'true'
        build-commands:
          - cargo --offline fetch --manifest-path Cargo.toml --verbose
          - cargo build --offline --release --all-features --locked
          - install -Dm0755 target/release/jterm4 ${FLATPAK_DEST}/bin/jterm4
          - install -Dm0644 data/io.github.beamiter.jterm4.desktop ${FLATPAK_DEST}/share/applications/io.github.beamiter.jterm4.desktop
          - install -Dm0644 data/io.github.beamiter.jterm4.metainfo.xml ${FLATPAK_DEST}/share/metainfo/io.github.beamiter.jterm4.metainfo.xml
          - install -Dm0644 data/io.github.beamiter.jterm4.svg ${FLATPAK_DEST}/share/icons/hicolor/scalable/apps/io.github.beamiter.jterm4.svg
          - install -Dm0644 data/io.github.beamiter.jterm4-128.png ${FLATPAK_DEST}/share/icons/hicolor/128x128/apps/io.github.beamiter.jterm4.png
          - install -Dm0644 data/io.github.beamiter.jterm4-256.png ${FLATPAK_DEST}/share/icons/hicolor/256x256/apps/io.github.beamiter.jterm4.png
        sources:
          - type: dir
            path: ../..
            skip:
              - .git
              - .flatpak-builder
              - flatpak-build
              - flatpak-repo
              - target
          - cargo-sources.json
    ''',
)

write(
    "scripts/smoke-flatpak.sh",
    r'''
    #!/usr/bin/env bash
    # Exercise VTE and Block launches under headless X11 and Wayland sessions.

    set -Eeuo pipefail

    APP_ID="${1:-io.github.beamiter.jterm4}"
    LOG_DIR="${LOG_DIR:-flatpak-smoke-logs}"
    RUNTIME_DIR="${XDG_RUNTIME_DIR:-$(mktemp -d)}"
    CREATED_RUNTIME=0
    XVFB_PID=""
    WESTON_PID=""

    if [[ -z "${XDG_RUNTIME_DIR:-}" ]]; then
        CREATED_RUNTIME=1
    fi
    export XDG_RUNTIME_DIR="${RUNTIME_DIR}"
    mkdir -p "${XDG_RUNTIME_DIR}" "${LOG_DIR}"
    chmod 0700 "${XDG_RUNTIME_DIR}"

    cleanup() {
        flatpak kill "${APP_ID}" >/dev/null 2>&1 || true
        [[ -z "${WESTON_PID}" ]] || kill "${WESTON_PID}" >/dev/null 2>&1 || true
        [[ -z "${XVFB_PID}" ]] || kill "${XVFB_PID}" >/dev/null 2>&1 || true
        if ((CREATED_RUNTIME == 1)); then
            rm -rf -- "${XDG_RUNTIME_DIR}"
        fi
    }
    trap cleanup EXIT

    smoke_mode() {
        local backend="$1"
        local mode="$2"
        local log="${LOG_DIR}/${backend}-${mode}.log"

        flatpak run --env="JTERM4_MODE=${mode}" "${APP_ID}" >"${log}" 2>&1 &
        local launcher=$!
        sleep 4
        if ! kill -0 "${launcher}" >/dev/null 2>&1; then
            wait "${launcher}" || true
            cat "${log}" >&2
            printf 'Flatpak %s/%s launch exited before smoke window\n' "${backend}" "${mode}" >&2
            return 1
        fi
        flatpak kill "${APP_ID}" >/dev/null 2>&1 || true
        wait "${launcher}" || true
    }

    command -v Xvfb >/dev/null 2>&1 || {
        printf 'Xvfb is required for the X11 smoke test\n' >&2
        exit 1
    }
    Xvfb :99 -screen 0 1280x800x24 >"${LOG_DIR}/xvfb.log" 2>&1 &
    XVFB_PID=$!
    export DISPLAY=:99
    unset WAYLAND_DISPLAY
    sleep 2
    smoke_mode x11 vte
    smoke_mode x11 block
    kill "${XVFB_PID}" >/dev/null 2>&1 || true
    wait "${XVFB_PID}" 2>/dev/null || true
    XVFB_PID=""

    command -v weston >/dev/null 2>&1 || {
        printf 'weston is required for the Wayland smoke test\n' >&2
        exit 1
    }
    unset DISPLAY
    export WAYLAND_DISPLAY=wayland-jterm4
    weston \
        --backend=headless-backend.so \
        --renderer=pixman \
        --socket="${WAYLAND_DISPLAY}" \
        --idle-time=0 \
        --log="${LOG_DIR}/weston.log" &
    WESTON_PID=$!
    sleep 3
    smoke_mode wayland vte
    smoke_mode wayland block
    ''',
)

write(
    "scripts/install.sh",
    r'''
    #!/usr/bin/env bash
    # Install jterm4 and its Linux desktop integration from a source checkout.

    set -Eeuo pipefail
    umask 077

    APP_ID="io.github.beamiter.jterm4"
    SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
    REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
    HOME_DIR="${HOME:-}"
    DESTDIR="${DESTDIR:-}"
    PREFIX="${HOME_DIR}/.local"
    BIN_DIR=""
    BACKEND="auto"
    INSTALL_CONFIG=1
    INSTALL_DESKTOP=1
    DRY_RUN=0

    usage() {
        cat <<'USAGE'
    Usage: ./scripts/install.sh [options]

    Options:
      --prefix PATH          Runtime prefix (default: ~/.local)
      --bin-dir PATH         Runtime binary directory (overrides --prefix)
      --backend auto|nix|cargo
                             Build backend (default: auto; prefers Nix)
      --no-config            Do not install config.toml.example
      --no-desktop           Do not install desktop, AppStream, or icon files
      --dry-run              Print commands without changing files
      -h, --help             Show this help

    Environment:
      DESTDIR                Optional staging root for packaging
      XDG_CONFIG_HOME        Config base (default: ~/.config)
      CARGO_TARGET_DIR       Cargo target directory (default: <repo>/target)
    USAGE
    }

    die() {
        printf 'jterm4 install: %s\n' "$*" >&2
        exit 1
    }

    print_command() {
        printf '  '
        printf '%q ' "$@"
        printf '\n'
    }

    run() {
        print_command "$@"
        if ((DRY_RUN == 0)); then
            "$@"
        fi
    }

    run_in_repo() {
        printf '  (cd %q && ' "${REPO_ROOT}"
        printf '%q ' "$@"
        printf ')\n'
        if ((DRY_RUN == 0)); then
            (cd -- "${REPO_ROOT}" && "$@")
        fi
    }

    require_command() {
        if command -v "$1" >/dev/null 2>&1; then
            return
        fi
        ((DRY_RUN == 1)) || die "required command not found: $1"
    }

    while (($# > 0)); do
        case "$1" in
            --prefix)
                (($# >= 2)) || die "--prefix requires a path"
                PREFIX="$2"
                shift 2
                ;;
            --prefix=*)
                PREFIX="${1#*=}"
                shift
                ;;
            --bin-dir)
                (($# >= 2)) || die "--bin-dir requires a path"
                BIN_DIR="$2"
                shift 2
                ;;
            --bin-dir=*)
                BIN_DIR="${1#*=}"
                shift
                ;;
            --backend)
                (($# >= 2)) || die "--backend requires auto, nix, or cargo"
                BACKEND="$2"
                shift 2
                ;;
            --backend=*)
                BACKEND="${1#*=}"
                shift
                ;;
            --no-config)
                INSTALL_CONFIG=0
                shift
                ;;
            --no-desktop)
                INSTALL_DESKTOP=0
                shift
                ;;
            --dry-run)
                DRY_RUN=1
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            --)
                shift
                (($# == 0)) || die "unexpected positional arguments: $*"
                ;;
            *)
                die "unknown option: $1"
                ;;
        esac
    done

    [[ -n "${HOME_DIR}" ]] || die "HOME is not set"
    [[ -n "${PREFIX}" ]] || die "prefix must not be empty"
    [[ "${PREFIX}" == /* ]] || die "--prefix must be an absolute path"
    if [[ -z "${BIN_DIR}" ]]; then
        BIN_DIR="${PREFIX}/bin"
    fi
    [[ "${BIN_DIR}" == /* ]] || die "--bin-dir must be an absolute path"
    if [[ -n "${DESTDIR}" ]]; then
        [[ "${DESTDIR}" == /* ]] || die "DESTDIR must be an absolute path"
        DESTDIR="${DESTDIR%/}"
    fi

    case "${BACKEND}" in
        auto)
            if command -v nix >/dev/null 2>&1; then
                BACKEND="nix"
            else
                BACKEND="cargo"
            fi
            ;;
        nix|cargo) ;;
        *) die "invalid backend '${BACKEND}'; expected auto, nix, or cargo" ;;
    esac

    TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
    if [[ "${TARGET_DIR}" != /* ]]; then
        TARGET_DIR="${REPO_ROOT}/${TARGET_DIR}"
    fi
    export CARGO_TARGET_DIR="${TARGET_DIR}"

    printf 'Building jterm4 with %s...\n' "${BACKEND}"
    case "${BACKEND}" in
        nix)
            require_command nix
            run_in_repo nix develop --command cargo build --release --locked
            ;;
        cargo)
            require_command cargo
            run_in_repo cargo build --release --locked
            ;;
    esac

    BINARY="${TARGET_DIR}/release/jterm4"
    if ((DRY_RUN == 0)) && [[ ! -x "${BINARY}" ]]; then
        die "release binary was not produced at ${BINARY}"
    fi

    require_command install
    STAGED_BIN_DIR="${DESTDIR}${BIN_DIR}"
    run install -d -m 0755 "${STAGED_BIN_DIR}"
    run install -m 0755 "${BINARY}" "${STAGED_BIN_DIR}/jterm4"

    if ((INSTALL_DESKTOP == 1)); then
        SHARE_DIR="${DESTDIR}${PREFIX}/share"
        run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}.desktop" \
            "${SHARE_DIR}/applications/${APP_ID}.desktop"
        run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}.metainfo.xml" \
            "${SHARE_DIR}/metainfo/${APP_ID}.metainfo.xml"
        run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}.svg" \
            "${SHARE_DIR}/icons/hicolor/scalable/apps/${APP_ID}.svg"
        run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}-128.png" \
            "${SHARE_DIR}/icons/hicolor/128x128/apps/${APP_ID}.png"
        run install -Dm0644 "${REPO_ROOT}/data/${APP_ID}-256.png" \
            "${SHARE_DIR}/icons/hicolor/256x256/apps/${APP_ID}.png"
    fi

    CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME_DIR}/.config}"
    [[ "${CONFIG_HOME}" == /* ]] || die "XDG_CONFIG_HOME must be an absolute path"
    CONFIG_DIR="${CONFIG_HOME}/jterm4"
    STAGED_CONFIG_DIR="${DESTDIR}${CONFIG_DIR}"
    if ((INSTALL_CONFIG == 1)); then
        run install -d -m 0700 "${STAGED_CONFIG_DIR}"
        if [[ ! -e "${STAGED_CONFIG_DIR}/config.toml" ]]; then
            run install -m 0600 "${REPO_ROOT}/config.toml.example" "${STAGED_CONFIG_DIR}/config.toml"
        else
            printf 'Keeping existing config: %s\n' "${CONFIG_DIR}/config.toml"
        fi
    fi

    printf 'Installed jterm4 to %s\n' "${BIN_DIR}/jterm4"
    if ((INSTALL_DESKTOP == 1)); then
        printf 'Installed desktop integration under %s/share\n' "${PREFIX}"
    fi
    if [[ -n "${DESTDIR}" ]]; then
        printf 'Staged file: %s\n' "${STAGED_BIN_DIR}/jterm4"
    fi
    printf 'Validate with: %s --doctor\n' "${BIN_DIR}/jterm4"
    ''',
)

write(
    "scripts/uninstall.sh",
    r'''
    #!/usr/bin/env bash
    # Remove jterm4 while preserving user configuration and state by default.

    set -Eeuo pipefail

    APP_ID="io.github.beamiter.jterm4"
    HOME_DIR="${HOME:-}"
    DESTDIR="${DESTDIR:-}"
    PREFIX="${HOME_DIR}/.local"
    BIN_DIR=""
    PURGE_CONFIG=0
    DRY_RUN=0

    usage() {
        cat <<'USAGE'
    Usage: ./scripts/uninstall.sh [options]

    Options:
      --prefix PATH          Runtime prefix (default: ~/.local)
      --bin-dir PATH         Runtime binary directory (overrides --prefix)
      --purge-config         Also remove the complete jterm4 config/state directory
      --dry-run              Print commands without changing files
      -h, --help             Show this help

    Environment:
      DESTDIR                Optional staging root for packaging
      XDG_CONFIG_HOME        Config base (default: ~/.config)
    USAGE
    }

    die() {
        printf 'jterm4 uninstall: %s\n' "$*" >&2
        exit 1
    }

    print_command() {
        printf '  '
        printf '%q ' "$@"
        printf '\n'
    }

    run() {
        print_command "$@"
        if ((DRY_RUN == 0)); then
            "$@"
        fi
    }

    remove_file() {
        local path="$1"
        if [[ -e "${path}" || -L "${path}" ]]; then
            run rm -f -- "${path}"
        fi
    }

    while (($# > 0)); do
        case "$1" in
            --prefix)
                (($# >= 2)) || die "--prefix requires a path"
                PREFIX="$2"
                shift 2
                ;;
            --prefix=*)
                PREFIX="${1#*=}"
                shift
                ;;
            --bin-dir)
                (($# >= 2)) || die "--bin-dir requires a path"
                BIN_DIR="$2"
                shift 2
                ;;
            --bin-dir=*)
                BIN_DIR="${1#*=}"
                shift
                ;;
            --purge-config)
                PURGE_CONFIG=1
                shift
                ;;
            --dry-run)
                DRY_RUN=1
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            --)
                shift
                (($# == 0)) || die "unexpected positional arguments: $*"
                ;;
            *)
                die "unknown option: $1"
                ;;
        esac
    done

    [[ -n "${HOME_DIR}" ]] || die "HOME is not set"
    [[ -n "${PREFIX}" ]] || die "prefix must not be empty"
    [[ "${PREFIX}" == /* ]] || die "--prefix must be an absolute path"
    if [[ -z "${BIN_DIR}" ]]; then
        BIN_DIR="${PREFIX}/bin"
    fi
    [[ "${BIN_DIR}" == /* ]] || die "--bin-dir must be an absolute path"
    if [[ -n "${DESTDIR}" ]]; then
        [[ "${DESTDIR}" == /* ]] || die "DESTDIR must be an absolute path"
        DESTDIR="${DESTDIR%/}"
    fi

    remove_file "${DESTDIR}${BIN_DIR}/jterm4"
    SHARE_DIR="${DESTDIR}${PREFIX}/share"
    remove_file "${SHARE_DIR}/applications/${APP_ID}.desktop"
    remove_file "${SHARE_DIR}/metainfo/${APP_ID}.metainfo.xml"
    remove_file "${SHARE_DIR}/icons/hicolor/scalable/apps/${APP_ID}.svg"
    remove_file "${SHARE_DIR}/icons/hicolor/128x128/apps/${APP_ID}.png"
    remove_file "${SHARE_DIR}/icons/hicolor/256x256/apps/${APP_ID}.png"

    if ((PURGE_CONFIG == 1)); then
        CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME_DIR}/.config}"
        [[ "${CONFIG_HOME}" == /* ]] || die "XDG_CONFIG_HOME must be an absolute path"
        CONFIG_DIR="${DESTDIR}${CONFIG_HOME}/jterm4"
        if [[ -e "${CONFIG_DIR}" ]]; then
            run rm -rf -- "${CONFIG_DIR}"
        else
            printf 'Config/state directory not present: %s\n' "${CONFIG_HOME}/jterm4"
        fi
    else
        printf 'Preserved config and state. Use --purge-config to remove them.\n'
    fi
    ''',
)

write(
    "docs/FLATPAK.md",
    r'''
    # Flatpak packaging and host integration

    jterm4's Flatpak application ID is `io.github.beamiter.jterm4`. The manifest is
    `packaging/flatpak/io.github.beamiter.jterm4.yml` and targets the GNOME 50
    runtime. Cargo dependencies are pinned by the committed
    `packaging/flatpak/cargo-sources.json` generated from `Cargo.lock`.

    ## Why a host bridge is required

    A terminal emulator is useful only when its shell and command-line tools operate
    on the user's host environment. Inside Flatpak, jterm4 therefore launches shells,
    SSH, Git metadata probes, `curl`, and `notify-send` through
    `flatpak-spawn --host --watch-bus`. Native installations continue to execute
    those programs directly. Both paths use the same PTY, backpressure, input, and
    process-cleanup code.

    The Flatpak package is not a containment boundary for terminal commands. Opening
    a shell intentionally grants that shell normal host-user authority. The sandbox
    still isolates the GTK application process and makes its host access explicit.

    ## Permissions

    The manifest requests:

    - Wayland and fallback X11 sockets, IPC sharing, and DRI for GTK rendering.
    - `--filesystem=host` so the file tree and reported working directories can show
      host projects. Flatpak still excludes several system paths from this shortcut.
    - `--talk-name=org.freedesktop.Flatpak` for `flatpak-spawn --host`.
    - SSH agent and network access for remote sessions and the optional AI panel.

    OSC 52 clipboard writes remain disabled by jterm4 unless the user explicitly
    enables them. AI-bound terminal text is still redacted by default.

    ## Build

    Install Flatpak and flatpak-builder, add Flathub, then run:

    ```bash
    flatpak remote-add --user --if-not-exists flathub \
      https://dl.flathub.org/repo/flathub.flatpakrepo
    flatpak-builder --user --install-deps-from=flathub --force-clean \
      --disable-rofiles-fuse --repo=flatpak-repo flatpak-build \
      packaging/flatpak/io.github.beamiter.jterm4.yml
    flatpak build-bundle flatpak-repo io.github.beamiter.jterm4.flatpak \
      io.github.beamiter.jterm4
    sha256sum io.github.beamiter.jterm4.flatpak
    ```

    CI regenerates the Cargo source manifest, validates the desktop and AppStream
    metadata, builds the bundle, records its SHA-256 checksum, and launches both VTE
    and Block modes under headless X11 and Wayland sessions.

    ## Install and diagnose

    ```bash
    flatpak --user install ./io.github.beamiter.jterm4.flatpak
    flatpak run io.github.beamiter.jterm4 --doctor
    flatpak run io.github.beamiter.jterm4
    ```

    Flatpak applications do not automatically inherit arbitrary host environment
    variables. To use the AI panel, provide `ANTHROPIC_API_KEY` to the app through a
    trusted launcher or an explicit Flatpak override. Treat such overrides as secret
    configuration.

    ## Known boundary

    OSC 7 is the authoritative working-directory signal in Flatpak. `/proc` fallbacks
    and foreground-process inspection can only see the sandbox-side
    `flatpak-spawn` helper, so integrations that omit OSC 7 may have less precise
    process names or current-directory recovery. This does not affect command I/O.

    The project license remains an explicit owner decision tracked separately. The
    AppStream metadata uses `LicenseRef-proprietary` until that decision is made; the
    Flatpak is intended for testing and direct project distribution, not Flathub
    submission, until the license issue is resolved.
    ''',
)

write(
    ".github/workflows/flatpak.yml",
    r'''
    name: Flatpak

    on:
      pull_request:
        branches: [master]
        paths:
          - Cargo.lock
          - Cargo.toml
          - data/**
          - packaging/flatpak/**
          - scripts/smoke-flatpak.sh
          - src/**
          - .github/workflows/flatpak.yml
      push:
        branches: [master]
        paths:
          - Cargo.lock
          - Cargo.toml
          - data/**
          - packaging/flatpak/**
          - scripts/smoke-flatpak.sh
          - src/**
          - .github/workflows/flatpak.yml
      workflow_dispatch:

    permissions:
      contents: read

    concurrency:
      group: flatpak-${{ github.event.pull_request.number || github.ref }}
      cancel-in-progress: true

    env:
      APP_ID: io.github.beamiter.jterm4
      CARGO_GENERATOR_COMMIT: 737c0085912f9f7dabf9341d4608e2a77a51a73a

    jobs:
      build:
        name: Bundle and smoke-test
        runs-on: ubuntu-24.04
        timeout-minutes: 75
        steps:
          - name: Checkout repository
            uses: actions/checkout@v7

          - name: Install packaging dependencies
            run: |
              sudo apt-get update
              sudo apt-get install --no-install-recommends -y \
                appstream \
                dbus-x11 \
                desktop-file-utils \
                flatpak \
                flatpak-builder \
                librsvg2-bin \
                python3-venv \
                weston \
                xvfb

          - name: Verify generated Cargo sources
            run: |
              python3 -m venv /tmp/flatpak-cargo-generator
              /tmp/flatpak-cargo-generator/bin/pip install --disable-pip-version-check \
                aiohttp tomlkit pyyaml
              curl --fail --location \
                --output /tmp/flatpak-cargo-generator.py \
                "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/${CARGO_GENERATOR_COMMIT}/cargo/flatpak-cargo-generator.py"
              /tmp/flatpak-cargo-generator/bin/python /tmp/flatpak-cargo-generator.py \
                Cargo.lock -o /tmp/cargo-sources.json
              diff -u packaging/flatpak/cargo-sources.json /tmp/cargo-sources.json

          - name: Validate desktop metadata and icons
            run: |
              desktop-file-validate data/${APP_ID}.desktop
              appstreamcli validate --no-net data/${APP_ID}.metainfo.xml
              rsvg-convert -w 128 -h 128 data/${APP_ID}.svg > /tmp/icon-128.png
              rsvg-convert -w 256 -h 256 data/${APP_ID}.svg > /tmp/icon-256.png
              cmp data/${APP_ID}-128.png /tmp/icon-128.png
              cmp data/${APP_ID}-256.png /tmp/icon-256.png

          - name: Configure Flathub
            run: |
              flatpak --user remote-add --if-not-exists flathub \
                https://dl.flathub.org/repo/flathub.flatpakrepo

          - name: Build Flatpak repository
            run: |
              flatpak-builder --user --install-deps-from=flathub --force-clean \
                --disable-rofiles-fuse --repo=flatpak-repo flatpak-build \
                packaging/flatpak/${APP_ID}.yml
              flatpak build-bundle flatpak-repo ${APP_ID}.flatpak ${APP_ID}
              sha256sum ${APP_ID}.flatpak | tee ${APP_ID}.flatpak.sha256

          - name: Install local bundle
            run: |
              flatpak --user remote-add --if-not-exists --no-gpg-verify \
                jterm4-ci "${PWD}/flatpak-repo"
              flatpak --user install -y jterm4-ci ${APP_ID}
              flatpak run ${APP_ID} --version
              flatpak run ${APP_ID} --doctor

          - name: Smoke-test X11 and Wayland launches
            run: dbus-run-session -- scripts/smoke-flatpak.sh ${APP_ID}

          - name: Upload Flatpak bundle and diagnostics
            if: ${{ always() }}
            uses: actions/upload-artifact@v4
            with:
              name: jterm4-flatpak-${{ github.sha }}
              path: |
                io.github.beamiter.jterm4.flatpak
                io.github.beamiter.jterm4.flatpak.sha256
                flatpak-smoke-logs/
              if-no-files-found: warn
              retention-days: 7
    ''',
)

replace_once(
    ".github/workflows/ci.yml",
    "          bash -n scripts/install.sh scripts/uninstall.sh\n          shellcheck scripts/install.sh scripts/uninstall.sh\n          scripts/install.sh --help >/dev/null\n          scripts/uninstall.sh --help >/dev/null",
    "          bash -n scripts/install.sh scripts/uninstall.sh scripts/smoke-flatpak.sh\n          shellcheck scripts/install.sh scripts/uninstall.sh scripts/smoke-flatpak.sh\n          scripts/install.sh --help >/dev/null\n          scripts/uninstall.sh --help >/dev/null",
)

replace_once(
    "README.md",
    "\n## 配置\n",
    r'''

    ## Flatpak 与桌面集成

    项目使用稳定应用 ID `io.github.beamiter.jterm4`，提供 desktop、AppStream、
    SVG/PNG 图标以及可复现 Flatpak 清单。Flatpak 中的 Shell、SSH、Git、curl
    和通知命令通过 `flatpak-spawn --host` 运行，因此终端操作的是宿主环境而
    不是一次性沙箱；原生安装路径保持直接执行。

    ```bash
    flatpak-builder --user --install-deps-from=flathub --force-clean \
      --disable-rofiles-fuse --repo=flatpak-repo flatpak-build \
      packaging/flatpak/io.github.beamiter.jterm4.yml
    flatpak build-bundle flatpak-repo io.github.beamiter.jterm4.flatpak \
      io.github.beamiter.jterm4
    ```

    权限模型、宿主桥接、安全边界、安装命令与已知限制见
    [Flatpak 指南](docs/FLATPAK.md)。

    ## 配置
    ''',
)
replace_once(
    "CHANGELOG.md",
    "### Added\n\n",
    "### Added\n\n- Reproducible GNOME 50 Flatpak packaging, stable desktop application ID, AppStream metadata, scalable/raster icons, checksummed CI bundles, and X11/Wayland VTE/Block smoke tests.\n- A Flatpak host-command bridge so shells, SSH, Git probes, AI curl requests, and desktop notifications operate on the host instead of the application sandbox.\n",
)
replace_once(
    "docs/ARCHITECTURE.md",
    "\n## Configuration and observability\n",
    r'''

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
    container.

    ## Configuration and observability
    ''',
)
replace_once(
    "docs/RELEASING.md",
    "5. Publish only artifacts that were built by a documented, reproducible packaging path. The raw Cargo binary dynamically links GTK/libadwaita/VTE and should not be described as a portable Linux binary. Until Flatpak/AppImage/native packages are implemented, use source archives and explicit distro dependencies.\n6. After publishing, run the installed build's `--version`, `--doctor`, and one interactive terminal smoke test, then mark the changelog comparison links.",
    "5. Build `packaging/flatpak/io.github.beamiter.jterm4.yml` from a clean checkout, verify the committed Cargo source manifest, validate desktop/AppStream metadata, and publish the Flatpak bundle together with its SHA-256 file. The raw Cargo binary dynamically links GTK/libadwaita/VTE and must not be described as portable.\n6. Install the produced Flatpak in a clean user account. Run `--version`, `--doctor`, VTE and Block launches under Wayland and X11, SSH-agent access, host working-directory/file-tree access, notifications, and AI networking before publishing.\n7. After publishing, verify uninstall/data-retention behavior and mark the changelog comparison links.",
)
replace_once(
    "CONTRIBUTING.md",
    "shellcheck scripts/install.sh scripts/uninstall.sh\n```",
    "shellcheck scripts/install.sh scripts/uninstall.sh scripts/smoke-flatpak.sh\n```\n\nFor packaging changes, also run `desktop-file-validate`, `appstreamcli validate --no-net`, regenerate `packaging/flatpak/cargo-sources.json`, build the Flatpak manifest, and execute `scripts/smoke-flatpak.sh` inside a D-Bus session.",
)

user_guide = ROOT / "docs/USER_GUIDE.md"
text = user_guide.read_text()
if "## 7. Flatpak 与桌面安装" not in text:
    text = text.replace(
        "## 7. SSH 远程会话",
        r'''## 7. Flatpak 与桌面安装

Flatpak 应用 ID 为 `io.github.beamiter.jterm4`。打包版本会通过
`flatpak-spawn --host` 启动宿主 Shell、SSH、Git、curl 和通知工具，避免用户
误以为终端命令运行在真实系统、实际却被限制在应用沙箱。该设计也意味着
jterm4 Flatpak 不是命令隔离边界。

```bash
flatpak run io.github.beamiter.jterm4 --doctor
flatpak run io.github.beamiter.jterm4
```

文件树需要宿主文件系统权限；AI 密钥必须通过可信启动器或显式 Flatpak 环境
覆盖提供。完整权限说明和构建流程见 `docs/FLATPAK.md`。

## 8. SSH 远程会话''',
    )
    text = text.replace("## 8. 工作流模板", "## 9. 工作流模板")
    text = text.replace("## 9. AI 面板", "## 10. AI 面板")
    text = text.replace("## 10. 配置与快捷键", "## 11. 配置与快捷键")
    user_guide.write_text(text)

security = ROOT / "SECURITY.md"
security_text = security.read_text()
flatpak_security = dedent(
    r'''

    ## Flatpak host authority

    The Flatpak intentionally uses `flatpak-spawn --host` and host filesystem access
    so terminal commands operate on the user's real account. Reports about unintended
    privilege expansion, command-argument confusion, leaked environment values, or a
    host process surviving after its owning pane closes are security-sensitive.
    ''').rstrip() + "\n"
if "## Flatpak host authority" not in security_text:
    security.write_text(security_text.rstrip() + flatpak_security)

gitignore = ROOT / ".gitignore"
gitignore_text = gitignore.read_text().rstrip()
for entry in [".flatpak-builder/", "flatpak-build/", "flatpak-repo/", "*.flatpak", "flatpak-smoke-logs/"]:
    if entry not in gitignore_text.splitlines():
        gitignore_text += f"\n{entry}"
gitignore.write_text(gitignore_text + "\n")

for executable in ["scripts/install.sh", "scripts/uninstall.sh", "scripts/smoke-flatpak.sh"]:
    (ROOT / executable).chmod(0o755)

print("Linux desktop packaging migration applied")
