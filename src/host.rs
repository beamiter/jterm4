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
        assert_eq!(
            wrap_argv_for(false, &argv, Some("/tmp"), &[("LESS", "R")]),
            argv
        );
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

    #[test]
    fn flatpak_shell_identity_reaches_the_host_child() {
        let argv = vec!["bash".to_string(), "-l".to_string()];
        let wrapped = wrap_argv_for(true, &argv, None, &[("TERM_PROGRAM", "jterm4")]);
        assert!(wrapped
            .iter()
            .any(|argument| argument == "--env=TERM_PROGRAM=jterm4"));
        assert_eq!(&wrapped[wrapped.len() - 2..], ["bash", "-l"]);
    }
}
