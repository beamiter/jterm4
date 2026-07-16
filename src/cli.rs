//! Dependency-light command-line contract.  Parsing and utility commands run
//! before GTK initialisation so they also work over SSH and in CI.

use gtk4::glib;
use serde::Serialize;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config::{
    choose_shell_argv, config_file_path, load_config, validate_config_contents, ConfigIssue,
    ConfigIssueLevel,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Block,
    Vte,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReportFormat {
    Human,
    Json,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LaunchOptions {
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) execute: Option<Vec<String>>,
    pub(crate) no_restore: bool,
    pub(crate) safe_mode: bool,
    pub(crate) mode: Option<Mode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellIntegration {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EarlyCommand {
    Help,
    Version,
    CheckConfig(Option<PathBuf>, ReportFormat),
    Doctor(ReportFormat),
    RestoreConfigBackup,
    ConfigPath,
    InitConfig,
    PrintDefaultConfig,
    PrintShellIntegration(ShellIntegration),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ParsedArgs {
    config: Option<PathBuf>,
    command: Option<EarlyCommand>,
    launch: LaunchOptions,
}

static LAUNCH_OPTIONS: OnceLock<LaunchOptions> = OnceLock::new();

pub(crate) fn launch_options() -> &'static LaunchOptions {
    LAUNCH_OPTIONS.get_or_init(LaunchOptions::default)
}

pub(crate) const HELP: &str = r#"jterm4 — a session-aware GTK4 terminal workspace

Usage:
  jterm4 [OPTIONS] [DIRECTORY]
  jterm4 [OPTIONS] --execute COMMAND [ARG...]

Launch options:
  -c, --config PATH           Use an alternate config file
  -d, --working-directory DIR
                              Start in DIR
  -e, --execute COMMAND ...   Run a command instead of the configured shell
      --mode block|vte        Override the terminal backend for this window
      --no-restore            Start a fresh workspace
      --safe-mode             Use isolated VTE defaults without restore or persistence

Utilities:
      --doctor [--json]       Run headless environment diagnostics
      --check-config [PATH] [--json]
                              Validate syntax, values, and keybindings
      --restore-config-backup Restore the newest valid rotating config backup
      --config-path           Print the effective config path
      --init-config           Create a documented config without overwriting one
      --print-default-config  Print the bundled example configuration
      --shell-integration SH  Print integration for bash, zsh, fish, or pwsh
  -h, --help                  Print help
  -V, --version               Print version

Examples:
  jterm4 ~/project
  jterm4 --mode block --no-restore
  jterm4 --safe-mode
  jterm4 -d /tmp -e bash -lc 'printf "hello\\n"'
  source <(jterm4 --shell-integration bash)

Environment overrides include JTERM4_CONFIG, JTERM4_MODE, JTERM4_THEME,
JTERM4_FONT, JTERM4_OPACITY, and JTERM4_LOG.
"#;

fn set_command(parsed: &mut ParsedArgs, command: EarlyCommand) -> Result<(), String> {
    if parsed.command.is_some() {
        return Err("only one utility command may be used at a time".to_string());
    }
    parsed.command = Some(command);
    Ok(())
}

fn parse_shell(value: &str) -> Result<ShellIntegration, String> {
    match value.to_ascii_lowercase().as_str() {
        "bash" => Ok(ShellIntegration::Bash),
        "zsh" => Ok(ShellIntegration::Zsh),
        "fish" => Ok(ShellIntegration::Fish),
        "powershell" | "pwsh" | "ps1" => Ok(ShellIntegration::PowerShell),
        _ => Err(format!(
            "unsupported shell '{value}' (use bash, zsh, fish, or pwsh)"
        )),
    }
}

fn parse_mode(value: &str) -> Result<Mode, String> {
    match value.to_ascii_lowercase().as_str() {
        "block" => Ok(Mode::Block),
        "vte" => Ok(Mode::Vte),
        _ => Err(format!(
            "invalid terminal mode '{value}' (use block or vte)"
        )),
    }
}

fn os_to_string(value: OsString, what: &str) -> Result<String, String> {
    value
        .into_string()
        .map_err(|_| format!("{what} must be valid UTF-8"))
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<ParsedArgs, String> {
    let args: Vec<OsString> = args.into_iter().collect();
    let mut parsed = ParsedArgs::default();
    let mut report_json = false;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index]
            .to_str()
            .ok_or_else(|| "options must be valid UTF-8".to_string())?;
        match arg {
            "-h" | "--help" => set_command(&mut parsed, EarlyCommand::Help)?,
            "-V" | "--version" => set_command(&mut parsed, EarlyCommand::Version)?,
            "--doctor" => set_command(&mut parsed, EarlyCommand::Doctor(ReportFormat::Human))?,
            "--restore-config-backup" => {
                set_command(&mut parsed, EarlyCommand::RestoreConfigBackup)?
            }
            "--config-path" | "--print-config-path" => {
                set_command(&mut parsed, EarlyCommand::ConfigPath)?
            }
            "--init-config" => set_command(&mut parsed, EarlyCommand::InitConfig)?,
            "--print-default-config" => set_command(&mut parsed, EarlyCommand::PrintDefaultConfig)?,
            "--json" => report_json = true,
            "-c" | "--config" => {
                index += 1;
                let path = args
                    .get(index)
                    .ok_or_else(|| format!("{arg} requires a path"))?;
                if path.to_string_lossy().starts_with('-') {
                    return Err(format!("{arg} requires a path"));
                }
                parsed.config = Some(PathBuf::from(path));
            }
            "--check-config" => {
                let path = args.get(index + 1).and_then(|next| {
                    (!next.to_string_lossy().starts_with('-')).then(|| PathBuf::from(next))
                });
                if path.is_some() {
                    index += 1;
                }
                set_command(
                    &mut parsed,
                    EarlyCommand::CheckConfig(path, ReportFormat::Human),
                )?;
            }
            "--shell-integration" => {
                index += 1;
                let shell = args
                    .get(index)
                    .ok_or_else(|| "--shell-integration requires a shell".to_string())?
                    .to_str()
                    .ok_or_else(|| "shell name must be valid UTF-8".to_string())?;
                set_command(
                    &mut parsed,
                    EarlyCommand::PrintShellIntegration(parse_shell(shell)?),
                )?;
            }
            "--no-restore" => parsed.launch.no_restore = true,
            "--safe-mode" => {
                parsed.launch.safe_mode = true;
                parsed.launch.no_restore = true;
            }
            "-d" | "--working-directory" => {
                index += 1;
                let path = args
                    .get(index)
                    .ok_or_else(|| format!("{arg} requires a path"))?;
                parsed.launch.working_directory = Some(PathBuf::from(path));
            }
            "--mode" => {
                index += 1;
                let value = args
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "--mode requires 'block' or 'vte'".to_string())?;
                parsed.launch.mode = Some(parse_mode(value)?);
            }
            "-e" | "--execute" | "--" => {
                let command = args[index + 1..]
                    .iter()
                    .cloned()
                    .map(|value| os_to_string(value, "command arguments"))
                    .collect::<Result<Vec<_>, _>>()?;
                if command.is_empty() {
                    return Err(format!("{arg} requires a command"));
                }
                parsed.launch.execute = Some(command);
                break;
            }
            _ if arg.starts_with("--config=") => {
                let value = arg.trim_start_matches("--config=");
                if value.is_empty() {
                    return Err("--config requires a path".to_string());
                }
                parsed.config = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--check-config=") => {
                let value = arg.trim_start_matches("--check-config=");
                if value.is_empty() {
                    return Err("--check-config requires a non-empty path".to_string());
                }
                set_command(
                    &mut parsed,
                    EarlyCommand::CheckConfig(Some(PathBuf::from(value)), ReportFormat::Human),
                )?;
            }
            _ if arg.starts_with("--shell-integration=") => {
                let value = arg.trim_start_matches("--shell-integration=");
                set_command(
                    &mut parsed,
                    EarlyCommand::PrintShellIntegration(parse_shell(value)?),
                )?;
            }
            _ if arg.starts_with("--mode=") => {
                parsed.launch.mode = Some(parse_mode(arg.trim_start_matches("--mode="))?);
            }
            _ if arg.starts_with("--working-directory=") => {
                let value = arg.trim_start_matches("--working-directory=");
                if value.is_empty() {
                    return Err("--working-directory requires a path".to_string());
                }
                parsed.launch.working_directory = Some(PathBuf::from(value));
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}")),
            _ => {
                if parsed.launch.working_directory.is_some() {
                    return Err("only one working directory may be specified".to_string());
                }
                parsed.launch.working_directory = Some(PathBuf::from(&args[index]));
            }
        }
        index += 1;
    }

    // A config-check path may appear after `--json`, or before the utility
    // itself. Until we know that the selected utility is `--check-config`, the
    // general parser has to retain such a positional argument as a possible
    // GUI working directory. Reclassify that one otherwise-unused positional
    // value now so argument order does not change the command's meaning.
    if let Some(EarlyCommand::CheckConfig(path @ None, _)) = parsed.command.as_mut() {
        if parsed.launch.execute.is_none()
            && !parsed.launch.no_restore
            && !parsed.launch.safe_mode
            && parsed.launch.mode.is_none()
        {
            *path = parsed.launch.working_directory.take();
        }
    }

    if report_json {
        parsed.command = match parsed.command.take() {
            Some(EarlyCommand::Doctor(_)) => Some(EarlyCommand::Doctor(ReportFormat::Json)),
            Some(EarlyCommand::CheckConfig(path, _)) => {
                Some(EarlyCommand::CheckConfig(path, ReportFormat::Json))
            }
            Some(other) => {
                parsed.command = Some(other);
                return Err("--json is only valid with --doctor or --check-config".to_string());
            }
            None => return Err("--json requires --doctor or --check-config".to_string()),
        };
    }

    if parsed.command.is_some() && parsed.launch != LaunchOptions::default() {
        return Err("launch options cannot be combined with a utility command".to_string());
    }
    if parsed.launch.safe_mode {
        if parsed.launch.mode.is_some() {
            return Err("--safe-mode cannot be combined with --mode".to_string());
        }
        if parsed.launch.execute.is_some() {
            return Err("--safe-mode cannot be combined with --execute".to_string());
        }
    }
    Ok(parsed)
}

fn print_issues(issues: &[ConfigIssue]) {
    for issue in issues {
        eprintln!("{issue}");
    }
}

#[derive(Serialize)]
struct JsonIssue<'a> {
    level: &'static str,
    path: &'a str,
    message: &'a str,
}

#[derive(Serialize)]
struct ConfigReport<'a> {
    path: String,
    valid: bool,
    errors: usize,
    warnings: usize,
    issues: Vec<JsonIssue<'a>>,
}

fn check_config(path: &Path, format: ReportFormat) -> bool {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) => {
            if format == ReportFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({
                        "path": path.display().to_string(),
                        "valid": false,
                        "errors": 1,
                        "warnings": 0,
                        "issues": [{"level":"error", "path":"$", "message":err.to_string()}]
                    })
                );
            } else {
                eprintln!("error: {}: {err}", path.display());
            }
            return false;
        }
    };
    match validate_config_contents(&contents) {
        Ok(issues) => {
            let errors = issues.iter().filter(|issue| issue.is_error()).count();
            let warnings = issues.len() - errors;
            if format == ReportFormat::Json {
                let json_issues = issues
                    .iter()
                    .map(|issue| JsonIssue {
                        level: match issue.level {
                            ConfigIssueLevel::Warning => "warning",
                            ConfigIssueLevel::Error => "error",
                        },
                        path: &issue.path,
                        message: &issue.message,
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ConfigReport {
                        path: path.display().to_string(),
                        valid: errors == 0,
                        errors,
                        warnings,
                        issues: json_issues,
                    })
                    .expect("config report is serializable")
                );
            } else {
                print_issues(&issues);
                if errors == 0 {
                    println!(
                        "OK: {} ({} warning{})",
                        path.display(),
                        warnings,
                        if warnings == 1 { "" } else { "s" }
                    );
                } else {
                    eprintln!(
                        "FAILED: {} ({} error{}, {} warning{})",
                        path.display(),
                        errors,
                        if errors == 1 { "" } else { "s" },
                        warnings,
                        if warnings == 1 { "" } else { "s" }
                    );
                }
            }
            errors == 0
        }
        Err(_err) => {
            if format == ReportFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({
                        "path": path.display().to_string(),
                        "valid": false,
                        "errors": 1,
                        "warnings": 0,
                        "issues": [{"level":"error", "path":"$", "message":"invalid TOML"}]
                    })
                );
            } else {
                // A TOML decoder error can include the offending source line.
                // Keep the explicit file path useful without echoing config
                // contents into logs or copied diagnostic output.
                eprintln!("error: {}: invalid TOML", path.display());
            }
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

/// Support-bundle mode keeps the checks useful while suppressing local paths
/// and user-authored values. It is intentionally environment-only and is not
/// advertised as a general CLI switch.
fn diagnostics_redacted() -> bool {
    std::env::var_os("JTERM4_DIAGNOSTICS_REDACT")
        .is_some_and(|value| !value.is_empty() && value != "0")
}

fn diagnostic_path(path: &Path) -> String {
    if diagnostics_redacted() {
        "<config-file>".to_string()
    } else {
        path.display().to_string()
    }
}

fn command_available(name: &str, flatpak: bool) -> bool {
    if flatpak {
        crate::host::command_available(name)
    } else {
        find_on_path(name).is_some()
    }
}

fn executable_available(executable: &str, flatpak: bool) -> bool {
    let path = Path::new(executable);
    if path.components().count() <= 1 {
        return command_available(executable, flatpak);
    }
    if flatpak {
        return crate::host::command("test")
            .args(["-x", executable])
            .status()
            .is_ok_and(|status| status.success());
    }
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn workflow_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "toml" | "yaml" | "yml"
            )
        })
}

fn workflow_discovery() -> (usize, usize, usize, usize) {
    let dirs = crate::workflows::workflow_dirs();
    let mut existing_dirs = 0;
    let mut candidate_files = 0;
    let mut parsed_files = 0;
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        existing_dirs += 1;
        candidate_files += entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter(|entry| workflow_file(&entry.path()))
            .count();
        parsed_files += crate::workflows::load_all_from(dir).len();
    }
    (
        crate::workflows::load_all().len(),
        existing_dirs,
        dirs.len(),
        candidate_files.saturating_sub(parsed_files),
    )
}

fn config_backup_health() -> (usize, usize, usize) {
    let mut present = 0;
    let mut valid = 0;
    let mut invalid_or_unreadable = 0;
    for path in crate::config_store::backup_paths() {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                present += 1;
                match validate_config_contents(&contents) {
                    Ok(issues) if !issues.iter().any(ConfigIssue::is_error) => valid += 1,
                    Ok(_) | Err(_) => invalid_or_unreadable += 1,
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                present += 1;
                invalid_or_unreadable += 1;
            }
        }
    }
    (present, valid, invalid_or_unreadable)
}

#[derive(Serialize)]
struct DoctorCheck {
    name: &'static str,
    status: &'static str,
    detail: String,
}

fn doctor(format: ReportFormat) -> bool {
    let path = config_file_path();
    let mut checks = Vec::new();
    if path.exists() {
        let contents = std::fs::read_to_string(&path);
        match contents {
            Ok(contents) => match validate_config_contents(&contents) {
                Ok(issues) => {
                    let errors = issues.iter().filter(|issue| issue.is_error()).count();
                    let warnings = issues.len() - errors;
                    checks.push(DoctorCheck {
                        name: "config",
                        status: if errors > 0 {
                            "error"
                        } else if warnings > 0 {
                            "warning"
                        } else {
                            "ok"
                        },
                        detail: format!(
                            "{} ({} errors, {} warnings)",
                            diagnostic_path(&path),
                            errors,
                            warnings
                        ),
                    });
                }
                Err(_err) => {
                    checks.push(DoctorCheck {
                        name: "config",
                        status: "error",
                        // `toml::de::Error` can embed the offending source
                        // line. Doctor reports must never echo configuration
                        // contents; the explicit check command is the local,
                        // user-requested detailed view.
                        detail: format!(
                            "{}: invalid TOML; run --check-config locally",
                            diagnostic_path(&path)
                        ),
                    });
                }
            },
            Err(err) => {
                checks.push(DoctorCheck {
                    name: "config",
                    status: "error",
                    detail: if diagnostics_redacted() {
                        "<config-file>: unreadable".to_string()
                    } else {
                        format!("{}: {err}", path.display())
                    },
                });
            }
        }
    } else {
        checks.push(DoctorCheck {
            name: "config",
            status: "warning",
            detail: format!(
                "{} does not exist (built-in defaults)",
                diagnostic_path(&path)
            ),
        });
    }

    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(&path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        checks.push(DoctorCheck {
            name: "config permissions",
            status: if mode & 0o077 == 0 { "ok" } else { "warning" },
            detail: if mode & 0o077 == 0 {
                format!("{mode:04o} (owner-only)")
            } else {
                format!("{mode:04o} (recommended: 0600)")
            },
        });
    }

    let (present_backups, valid_backups, bad_backups) = config_backup_health();
    checks.push(DoctorCheck {
        name: "config backups",
        status: if bad_backups > 0 || valid_backups == 0 {
            "warning"
        } else {
            "ok"
        },
        detail: if present_backups == 0 {
            "none yet; rotating backups are created after in-app saves".to_string()
        } else {
            format!("{valid_backups} valid, {bad_backups} invalid or unreadable")
        },
    });

    let (lock_status, lock_detail) = match crate::config_store::lock_status() {
        crate::config_store::ConfigLockStatus::Clear => ("ok", "clear"),
        crate::config_store::ConfigLockStatus::Active => {
            ("warning", "another process may currently be saving")
        }
        crate::config_store::ConfigLockStatus::Unavailable => {
            ("warning", "status could not be inspected")
        }
    };
    checks.push(DoctorCheck {
        name: "config write lock",
        status: lock_status,
        detail: lock_detail.to_string(),
    });

    let flatpak = crate::host::is_flatpak();
    let bridge_ok = crate::host::bridge_available();
    checks.push(DoctorCheck {
        name: "runtime",
        status: if !flatpak || bridge_ok { "ok" } else { "error" },
        detail: if flatpak {
            format!(
                "flatpak; host bridge {}",
                if bridge_ok { "available" } else { "missing" }
            )
        } else {
            "native".to_string()
        },
    });

    let (ready, active) = crate::state::session_snapshot_counts();
    checks.push(DoctorCheck {
        name: "session snapshots",
        status: "ok",
        detail: format!("{ready} ready, {active} active"),
    });
    let (config, _, _) = load_config();
    let shell_argv = choose_shell_argv(config.shell.as_deref());
    let shell = shell_argv.first().map(String::as_str).unwrap_or_default();
    let shell_available = executable_available(shell, flatpak);
    checks.push(DoctorCheck {
        name: "shell",
        status: if shell_available { "ok" } else { "error" },
        detail: if !shell_available && diagnostics_redacted() {
            "configured shell is not executable".to_string()
        } else if !shell_available {
            format!("not executable: {shell}")
        } else if diagnostics_redacted() {
            shell_argv
                .first()
                .and_then(|shell| Path::new(shell).file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("available")
                .to_string()
        } else {
            shell_argv.join(" ")
        },
    });

    let display = if env_presence("WAYLAND_DISPLAY") == "set" {
        Some("Wayland display is available")
    } else if env_presence("DISPLAY") == "set" {
        Some("X11 display is available")
    } else {
        None
    };
    checks.push(DoctorCheck {
        name: "display",
        status: if display.is_some() { "ok" } else { "warning" },
        detail: display
            .unwrap_or("DISPLAY and WAYLAND_DISPLAY are unset")
            .to_string(),
    });

    let curl_available = command_available("curl", flatpak);
    for (name, purpose) in [
        ("git", "repository status"),
        ("ssh", "remote sessions"),
        ("curl", "AI panel"),
        ("notify-send", "long-command notifications"),
    ] {
        let available = command_available(name, flatpak);
        checks.push(DoctorCheck {
            name,
            status: if available { "ok" } else { "warning" },
            detail: if available {
                format!("available ({purpose})")
            } else {
                format!("not found ({purpose} unavailable)")
            },
        });
    }

    if config.command_history_enabled {
        checks.push(DoctorCheck {
            name: "command history",
            status: if config.command_history_path.is_some() {
                "ok"
            } else {
                "warning"
            },
            detail: if config.command_history_path.is_some() {
                "enabled; metadata only".to_string()
            } else {
                "enabled but no path is available".to_string()
            },
        });
    } else {
        checks.push(DoctorCheck {
            name: "command history",
            status: "warning",
            detail: "disabled by configuration".to_string(),
        });
    }

    let (workflow_count, workflow_dirs, workflow_search_dirs, rejected_workflows) =
        workflow_discovery();
    checks.push(DoctorCheck {
        name: "workflows",
        status: if workflow_count == 0 || rejected_workflows > 0 {
            "warning"
        } else {
            "ok"
        },
        detail: format!(
            "{workflow_count} available; {workflow_dirs}/{workflow_search_dirs} search locations readable; {rejected_workflows} invalid or unreadable file(s)"
        ),
    });

    let welcome_notebook = crate::workflows::welcome_notebook_path();
    checks.push(DoctorCheck {
        name: "welcome notebook",
        status: if welcome_notebook.is_some() {
            "ok"
        } else {
            "warning"
        },
        detail: if welcome_notebook.is_some() {
            "available in configured/user/system/source assets".to_string()
        } else {
            "not found in configured/user/system/source assets".to_string()
        },
    });

    if !config.ai_enabled {
        checks.push(DoctorCheck {
            name: "AI provider",
            status: "warning",
            detail: "disabled by configuration".to_string(),
        });
    } else {
        match crate::ai::AiClient::from_config(&config) {
            Ok(client) => checks.push(DoctorCheck {
                name: "AI provider",
                status: if curl_available { "ok" } else { "warning" },
                detail: format!(
                    "{} configured; API key {}; curl {}",
                    client.provider.display_name(),
                    if client.api_key.is_some() {
                        "present"
                    } else {
                        "not set (optional for local/compatible endpoints)"
                    },
                    if curl_available {
                        "available"
                    } else {
                        "missing"
                    }
                ),
            }),
            Err(error) => checks.push(DoctorCheck {
                name: "AI provider",
                status: "warning",
                detail: if diagnostics_redacted() {
                    "provider configuration or credentials are incomplete".to_string()
                } else {
                    error.to_string()
                },
            }),
        }
    }

    let ssh_available = command_available("ssh", flatpak);
    checks.push(DoctorCheck {
        name: "remote hosts",
        status: if config.remote_hosts.is_empty() || ssh_available {
            "ok"
        } else {
            "error"
        },
        detail: if config.remote_hosts.is_empty() {
            "none configured".to_string()
        } else {
            format!(
                "{} configured; ssh {}",
                config.remote_hosts.len(),
                if ssh_available {
                    "available"
                } else {
                    "missing"
                }
            )
        },
    });

    checks.push(DoctorCheck {
        name: "terminal mode",
        status: "ok",
        detail: match config.terminal_mode {
            crate::config::TerminalMode::Block => "block",
            crate::config::TerminalMode::Vte => "vte",
        }
        .to_string(),
    });

    let errors = checks
        .iter()
        .filter(|check| check.status == "error")
        .count();
    let warnings = checks
        .iter()
        .filter(|check| check.status == "warning")
        .count();
    let healthy = errors == 0;
    if format == ReportFormat::Json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "healthy": healthy,
                "errors": errors,
                "warnings": warnings,
                "checks": checks,
            }))
            .expect("doctor report is serializable")
        );
    } else {
        println!("jterm4 {} doctor", env!("CARGO_PKG_VERSION"));
        println!("application id: {}", crate::host::APP_ID);
        for check in &checks {
            println!("{}: {} ({})", check.name, check.detail, check.status);
        }
        println!("summary: {errors} error(s), {warnings} warning(s)");
        println!("GTK_IM_MODULE: {}", env_presence("GTK_IM_MODULE"));
    }
    healthy
}

fn init_config_file() -> Result<(), String> {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} already exists; it was not overwritten", path.display())
        } else {
            format!("cannot create {}: {err}", path.display())
        }
    })?;
    file.write_all(include_str!("../config.toml.example").as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|err| format!("cannot sync {}: {err}", parent.display()))?;
    }
    println!("Created {}", path.display());
    Ok(())
}

fn print_shell_integration(shell: ShellIntegration) {
    let script = match shell {
        ShellIntegration::Bash => include_str!("../scripts/shell-integration/jterm4.bash"),
        ShellIntegration::Zsh => include_str!("../scripts/shell-integration/jterm4.zsh"),
        ShellIntegration::Fish => include_str!("../scripts/shell-integration/jterm4.fish"),
        ShellIntegration::PowerShell => {
            include_str!("../scripts/shell-integration/jterm4.ps1")
        }
    };
    print!("{script}");
}

fn validate_launch_options(options: &LaunchOptions) -> Result<(), String> {
    if let Some(directory) = &options.working_directory {
        if !directory.is_dir() {
            return Err(format!(
                "working directory does not exist or is not a directory: {}",
                directory.display()
            ));
        }
    }
    if let Some(argv) = &options.execute {
        let executable = argv.first().expect("parser rejects empty commands");
        let path = Path::new(executable);
        let found = if path.components().count() > 1 {
            path.is_file()
        } else {
            find_on_path(executable).is_some()
        };
        if !found {
            return Err(format!("command not found: {executable}"));
        }
    }
    Ok(())
}

/// Handle the command-line before GTK starts. Returns `None` for a normal GUI
/// launch and stores its validated options for `app::run` to consume.
pub(crate) fn handle_early_args() -> Option<glib::ExitCode> {
    let parsed = match parse_args(std::env::args_os().skip(1)) {
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

    let Some(command) = parsed.command else {
        if let Err(err) = validate_launch_options(&parsed.launch) {
            eprintln!("jterm4: {err}");
            return Some(glib::ExitCode::new(2));
        }
        if let Some(mode) = parsed.launch.mode {
            let value = match mode {
                Mode::Block => "block",
                Mode::Vte => "vte",
            };
            // SAFETY: no threads or configuration reads exist yet.
            unsafe { std::env::set_var("JTERM4_MODE", value) };
        }
        if parsed.launch.safe_mode {
            // SAFETY: consumed during single-threaded application startup.
            unsafe {
                std::env::set_var("JTERM4_MODE", "vte");
                std::env::set_var("JTERM4_SAFE_MODE", "1");
            }
        }
        let _ = LAUNCH_OPTIONS.set(parsed.launch);
        return None;
    };

    let success = match command {
        EarlyCommand::Help => {
            print!("{HELP}");
            true
        }
        EarlyCommand::Version => {
            println!("jterm4 {}", env!("CARGO_PKG_VERSION"));
            true
        }
        EarlyCommand::ConfigPath => {
            println!("{}", config_file_path().display());
            true
        }
        EarlyCommand::PrintDefaultConfig => {
            print!("{}", include_str!("../config.toml.example"));
            true
        }
        EarlyCommand::CheckConfig(path, format) => {
            check_config(&path.unwrap_or_else(config_file_path), format)
        }
        EarlyCommand::Doctor(format) => doctor(format),
        EarlyCommand::InitConfig => match init_config_file() {
            Ok(()) => true,
            Err(err) => {
                eprintln!("jterm4: {err}");
                false
            }
        },
        EarlyCommand::PrintShellIntegration(shell) => {
            print_shell_integration(shell);
            true
        }
        EarlyCommand::RestoreConfigBackup => match crate::config_store::restore_backup() {
            Ok((source, _revision)) => {
                println!(
                    "Restored {} from {}",
                    config_file_path().display(),
                    source.display()
                );
                true
            }
            Err(err) => {
                eprintln!("jterm4: {err}");
                false
            }
        },
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
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn no_args_launches_gui() {
        assert_eq!(parse(&[]).unwrap(), ParsedArgs::default());
    }

    #[test]
    fn parses_launch_options_and_execute_remainder() {
        let parsed = parse(&[
            "--mode", "block", "-d", "/tmp", "-e", "bash", "-lc", "echo hi",
        ])
        .unwrap();
        assert_eq!(
            parsed.launch,
            LaunchOptions {
                working_directory: Some(PathBuf::from("/tmp")),
                execute: Some(vec!["bash".into(), "-lc".into(), "echo hi".into()]),
                no_restore: false,
                safe_mode: false,
                mode: Some(Mode::Block),
            }
        );
    }

    #[test]
    fn config_can_be_combined_with_check_and_json() {
        let parsed = parse(&["--config", "/tmp/custom.toml", "--check-config", "--json"]).unwrap();
        assert_eq!(parsed.config, Some(PathBuf::from("/tmp/custom.toml")));
        assert_eq!(
            parsed.command,
            Some(EarlyCommand::CheckConfig(None, ReportFormat::Json))
        );
    }

    #[test]
    fn config_check_path_is_order_independent_around_json() {
        for args in [
            ["--check-config", "/tmp/explicit.toml", "--json"],
            ["--check-config", "--json", "/tmp/explicit.toml"],
            ["/tmp/explicit.toml", "--check-config", "--json"],
        ] {
            let parsed = parse(&args).unwrap();
            assert_eq!(
                parsed.command,
                Some(EarlyCommand::CheckConfig(
                    Some(PathBuf::from("/tmp/explicit.toml")),
                    ReportFormat::Json
                ))
            );
            assert_eq!(parsed.launch, LaunchOptions::default());
        }
    }

    #[cfg(unix)]
    #[test]
    fn executable_probe_checks_explicit_shell_paths() {
        assert!(executable_available("/bin/sh", false));
        assert!(!executable_available(
            "/definitely/missing/jterm4-shell",
            false
        ));
    }

    #[test]
    fn safe_mode_implies_fresh_workspace_and_rejects_overrides() {
        let parsed = parse(&["--safe-mode"]).unwrap();
        assert!(parsed.launch.safe_mode);
        assert!(parsed.launch.no_restore);
        assert!(parse(&["--safe-mode", "--mode", "block"]).is_err());
        assert!(parse(&["--safe-mode", "-e", "bash"]).is_err());
    }

    #[test]
    fn parses_shell_alias_and_config_utilities() {
        assert_eq!(
            parse(&["--shell-integration", "pwsh"]).unwrap().command,
            Some(EarlyCommand::PrintShellIntegration(
                ShellIntegration::PowerShell
            ))
        );
        assert_eq!(
            parse(&["--restore-config-backup"]).unwrap().command,
            Some(EarlyCommand::RestoreConfigBackup)
        );
        assert_eq!(
            parse(&["--config-path"]).unwrap().command,
            Some(EarlyCommand::ConfigPath)
        );
    }

    #[test]
    fn rejects_unknown_conflicting_and_stray_json_options() {
        assert!(parse(&["--wat"]).is_err());
        assert!(parse(&["--doctor", "--version"]).is_err());
        assert!(parse(&["--json"]).is_err());
        assert!(parse(&["--doctor", "--mode", "block"]).is_err());
    }
}
