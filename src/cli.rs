//! Headless command-line entry points. These run before GTK initialization so
//! help, config validation, and diagnostics also work over SSH/CI without a
//! display server.

use gtk4::glib;
use std::path::{Path, PathBuf};

use crate::config::{
    choose_shell_argv, config_file_path, load_config, validate_config_contents, ConfigIssue,
};

const HELP: &str = "jterm4 — session-aware GTK4 terminal

Usage: jterm4 [OPTIONS]

Options:
  -c, --config PATH       Use an alternate config file
      --check-config [PATH]
                          Validate config syntax, values, and keybindings
      --doctor            Run headless environment diagnostics
      --print-config-path Print the effective config path
      --print-default-config
                          Print the bundled example configuration
  -h, --help              Print help
  -V, --version           Print version

Environment overrides include JTERM4_CONFIG, JTERM4_MODE, JTERM4_THEME,
JTERM4_FONT, JTERM4_OPACITY, and JTERM4_LOG.";

#[derive(Clone, Debug, PartialEq, Eq)]
enum EarlyCommand {
    Help,
    Version,
    CheckConfig(Option<PathBuf>),
    Doctor,
    PrintConfigPath,
    PrintDefaultConfig,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ParsedArgs {
    config: Option<PathBuf>,
    command: Option<EarlyCommand>,
}

fn set_command(parsed: &mut ParsedArgs, command: EarlyCommand) -> Result<(), String> {
    if parsed.command.is_some() {
        return Err("only one headless command may be used at a time".to_string());
    }
    parsed.command = Some(command);
    Ok(())
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<ParsedArgs, String> {
    let mut parsed = ParsedArgs::default();
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => set_command(&mut parsed, EarlyCommand::Help)?,
            "-V" | "--version" => set_command(&mut parsed, EarlyCommand::Version)?,
            "--doctor" => set_command(&mut parsed, EarlyCommand::Doctor)?,
            "--print-config-path" => set_command(&mut parsed, EarlyCommand::PrintConfigPath)?,
            "--print-default-config" => set_command(&mut parsed, EarlyCommand::PrintDefaultConfig)?,
            "-c" | "--config" => {
                let path = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a path"))?;
                if path.starts_with('-') {
                    return Err(format!("{arg} requires a path"));
                }
                parsed.config = Some(PathBuf::from(path));
            }
            "--check-config" => {
                let path = args
                    .next_if(|next| !next.starts_with('-'))
                    .map(PathBuf::from);
                set_command(&mut parsed, EarlyCommand::CheckConfig(path))?;
            }
            "--" => {
                if let Some(extra) = args.next() {
                    return Err(format!("unexpected positional argument: {extra}"));
                }
            }
            _ if arg.starts_with("--config=") => {
                let path = arg.trim_start_matches("--config=");
                if path.is_empty() {
                    return Err("--config requires a path".to_string());
                }
                parsed.config = Some(PathBuf::from(path));
            }
            _ if arg.starts_with("--check-config=") => {
                let path = arg.trim_start_matches("--check-config=");
                if path.is_empty() {
                    return Err("--check-config requires a non-empty path".to_string());
                }
                set_command(
                    &mut parsed,
                    EarlyCommand::CheckConfig(Some(PathBuf::from(path))),
                )?;
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    Ok(parsed)
}

fn print_issues(issues: &[ConfigIssue]) {
    for issue in issues {
        eprintln!("{issue}");
    }
}

fn check_config(path: &Path) -> bool {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!("error: {}: {err}", path.display());
            return false;
        }
    };
    match validate_config_contents(&contents) {
        Ok(issues) => {
            print_issues(&issues);
            let errors = issues.iter().filter(|issue| issue.is_error()).count();
            let warnings = issues.len() - errors;
            if errors == 0 {
                println!(
                    "OK: {} ({} warning{})",
                    path.display(),
                    warnings,
                    if warnings == 1 { "" } else { "s" }
                );
                true
            } else {
                eprintln!(
                    "FAILED: {} ({} error{}, {} warning{})",
                    path.display(),
                    errors,
                    if errors == 1 { "" } else { "s" },
                    warnings,
                    if warnings == 1 { "" } else { "s" }
                );
                false
            }
        }
        Err(err) => {
            eprintln!("error: {}: invalid TOML: {err}", path.display());
            false
        }
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn env_presence(name: &str) -> &'static str {
    if std::env::var_os(name).is_some_and(|value| !value.is_empty()) {
        "set"
    } else {
        "not set"
    }
}

fn doctor() -> bool {
    let path = config_file_path();
    println!("jterm4 {} doctor", env!("CARGO_PKG_VERSION"));
    println!("config: {}", path.display());

    let config_ok = if path.exists() {
        check_config(&path)
    } else {
        println!("config status: not created (built-in defaults will be used)");
        true
    };

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
        match find_on_path(name) {
            Some(found) => println!("{name}: {} ({purpose})", found.display()),
            None => println!("{name}: not found ({purpose} unavailable)"),
        }
    }
    config_ok
}

/// Handle options that must complete without initializing a display. Returns
/// `None` for a normal GUI launch and an exit code for all headless commands or
/// argument errors.
pub(crate) fn handle_early_args() -> Option<glib::ExitCode> {
    let parsed = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("jterm4: {err}\nTry 'jterm4 --help' for usage.");
            return Some(glib::ExitCode::new(2));
        }
    };

    if let Some(path) = parsed.config {
        // SAFETY: this runs before GTK, worker threads, and any config reads.
        unsafe { std::env::set_var("JTERM4_CONFIG", path) };
    }

    let command = parsed.command?;
    let success = match command {
        EarlyCommand::Help => {
            println!("{HELP}");
            true
        }
        EarlyCommand::Version => {
            println!("jterm4 {}", env!("CARGO_PKG_VERSION"));
            true
        }
        EarlyCommand::PrintConfigPath => {
            println!("{}", config_file_path().display());
            true
        }
        EarlyCommand::PrintDefaultConfig => {
            print!("{}", include_str!("../config.toml.example"));
            true
        }
        EarlyCommand::CheckConfig(path) => {
            let path = path.unwrap_or_else(config_file_path);
            check_config(&path)
        }
        EarlyCommand::Doctor => doctor(),
    };
    Some(if success {
        glib::ExitCode::SUCCESS
    } else {
        glib::ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<ParsedArgs, String> {
        parse_args(args.iter().map(|arg| (*arg).to_string()))
    }

    #[test]
    fn no_args_launches_gui() {
        assert_eq!(parse(&[]).unwrap(), ParsedArgs::default());
    }

    #[test]
    fn config_can_be_combined_with_check() {
        let parsed = parse(&["--config", "/tmp/custom.toml", "--check-config"]).unwrap();
        assert_eq!(parsed.config, Some(PathBuf::from("/tmp/custom.toml")));
        assert_eq!(parsed.command, Some(EarlyCommand::CheckConfig(None)));
    }

    #[test]
    fn check_config_accepts_inline_path() {
        let parsed = parse(&["--check-config=/tmp/a.toml"]).unwrap();
        assert_eq!(
            parsed.command,
            Some(EarlyCommand::CheckConfig(Some(PathBuf::from(
                "/tmp/a.toml"
            ))))
        );
    }

    #[test]
    fn rejects_unknown_and_conflicting_commands() {
        assert!(parse(&["--wat"]).is_err());
        assert!(parse(&["--doctor", "--version"]).is_err());
    }
}
