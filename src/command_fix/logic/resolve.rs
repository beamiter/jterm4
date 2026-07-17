use std::collections::HashSet;
use std::fs;
use std::process::Stdio;

use crate::ai::AiClient;

use super::model;
use super::util::{
    rank_names, replace_closest_token, replace_exact_token, safe_word, validate_candidate,
};
use super::{Candidate, Context, Evidence, Failure, SuggestionResult, MAX_CANDIDATES};

pub(super) fn suggest_blocking(
    context: &Context,
    failure: &Failure,
    ai_client: Option<&AiClient>,
) -> SuggestionResult {
    let mut candidates = local_candidates(context, failure);
    if candidates.is_empty() {
        if let Some(client) = ai_client {
            candidates = model::suggest_blocking(client, context, failure);
        }
    }

    let mut seen = HashSet::new();
    candidates.retain(|candidate| {
        validate_candidate(&context.command, &candidate.command)
            && seen.insert(candidate.command.clone())
    });
    candidates.truncate(MAX_CANDIDATES);

    if candidates.is_empty() {
        SuggestionResult::None
    } else {
        SuggestionResult::Candidates(candidates)
    }
}

fn local_candidates(context: &Context, failure: &Failure) -> Vec<Candidate> {
    match failure {
        Failure::ToolSuggestion { suggestion } => tool_suggestion(context, suggestion),
        Failure::AptPackageNotFound { package } if !context.remote => {
            apt_package_candidates(context, package)
        }
        Failure::CommandNotFound { executable } if !context.remote => {
            path_command_candidates(context, executable)
        }
        Failure::AptPackageNotFound { .. }
        | Failure::CommandNotFound { .. }
        | Failure::UnknownSubcommand { .. }
        | Failure::UnknownOption { .. } => Vec::new(),
    }
}

fn tool_suggestion(context: &Context, suggestion: &str) -> Vec<Candidate> {
    if !safe_word(suggestion) {
        return Vec::new();
    }
    let Some(command) = replace_closest_token(&context.command, suggestion) else {
        return Vec::new();
    };
    vec![Candidate {
        command,
        reason: "The failed tool printed this likely correction.".to_string(),
        evidence: Evidence::ToolOutput,
    }]
}

fn apt_package_candidates(context: &Context, package: &str) -> Vec<Candidate> {
    let Some(names) = apt_package_names() else {
        return Vec::new();
    };
    rank_names(package, names.lines())
        .into_iter()
        .filter_map(|candidate_package| {
            let command = replace_exact_token(&context.command, package, &candidate_package)?;
            Some(Candidate {
                command,
                reason: format!(
                    "The package name {package:?} is close to the package {candidate_package:?}."
                ),
                evidence: Evidence::PackageIndex {
                    package: candidate_package,
                },
            })
        })
        .collect()
}

fn apt_package_names() -> Option<String> {
    let output = crate::host::command("apt-cache")
        .arg("pkgnames")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > 16 * 1024 * 1024 {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn path_command_candidates(context: &Context, executable: &str) -> Vec<Candidate> {
    let commands = target_command_names().unwrap_or_else(native_path_command_names);
    rank_names(executable, commands.iter().map(String::as_str))
        .into_iter()
        .filter_map(|candidate_executable| {
            let command = replace_exact_token(
                &context.command,
                executable,
                &candidate_executable,
            )?;
            Some(Candidate {
                command,
                reason: format!(
                    "The command {executable:?} is close to the available command {candidate_executable:?}."
                ),
                evidence: Evidence::PathCommand {
                    executable: candidate_executable,
                },
            })
        })
        .collect()
}

fn target_command_names() -> Option<Vec<String>> {
    let output = crate::host::command("bash")
        .args([
            "--noprofile",
            "--norc",
            "-lc",
            "compgen -c | LC_ALL=C sort -u",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > 16 * 1024 * 1024 {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|name| safe_word(name))
            .take(100_000)
            .map(str::to_string)
            .collect(),
    )
}

fn native_path_command_names() -> Vec<String> {
    let mut names = HashSet::new();
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    for directory in std::env::split_paths(&path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten().take(20_000) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if safe_word(name) {
                names.insert(name.to_string());
            }
        }
    }
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(command: &str) -> Context {
        Context {
            command: command.to_string(),
            cwd: "/tmp".to_string(),
            exit_code: 1,
            output: "failure".to_string(),
            remote: false,
        }
    }

    #[test]
    fn tool_hint_changes_only_the_closest_token() {
        let suggestions = tool_suggestion(&context("git statsu"), "status");
        assert_eq!(suggestions[0].command, "git status");
        assert_eq!(suggestions[0].evidence, Evidence::ToolOutput);
    }

    #[test]
    fn remote_failures_do_not_use_local_package_or_path_state() {
        let mut context = context("apt install fmpg");
        context.remote = true;
        assert!(local_candidates(
            &context,
            &Failure::AptPackageNotFound {
                package: "fmpg".to_string()
            }
        )
        .is_empty());
    }
}
