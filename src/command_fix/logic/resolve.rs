use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use super::util::{edit_distance, replace_word, safe_candidate};
use super::{Candidate, Context, Evidence, Failure};

const MAX_PROBE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CANDIDATES: usize = 3;

pub(super) fn local_suggestions(context: &Context, failure: &Failure) -> Vec<Candidate> {
    match failure {
        Failure::AptPackageNotFound(package) if !context.remote => {
            apt_candidates(&context.command, package)
        }
        Failure::CommandNotFound(executable) if !context.remote => {
            path_candidates(&context.command, executable)
        }
        Failure::ToolSuggestion { old, new } => replace_word(&context.command, old, new)
            .filter(|command| safe_candidate(&context.command, command))
            .map(|command| {
                vec![Candidate {
                    command,
                    reason: format!(
                        "The failing tool suggested replacing `{old}` with `{new}`."
                    ),
                    evidence: Evidence::TargetOutput,
                }]
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn apt_candidates(original: &str, package: &str) -> Vec<Candidate> {
    if !crate::host::command_available("apt-cache") {
        return Vec::new();
    }
    let Some(output) = capture("apt-cache", &["pkgnames"]) else {
        return Vec::new();
    };
    rank(
        package,
        output
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
    )
    .into_iter()
    .filter_map(|replacement| {
        let command = replace_word(original, package, &replacement)?;
        safe_candidate(original, &command).then_some(Candidate {
            command,
            reason: format!(
                "APT contains `{replacement}`, while the failed package was `{package}`."
            ),
            evidence: Evidence::AptIndex,
        })
    })
    .take(MAX_CANDIDATES)
    .collect()
}

fn path_candidates(original: &str, executable: &str) -> Vec<Candidate> {
    rank(executable, path_commands())
        .into_iter()
        .filter(|name| crate::host::command_available(name))
        .filter_map(|replacement| {
            let command = replace_word(original, executable, &replacement)?;
            safe_candidate(original, &command).then_some(Candidate {
                command,
                reason: format!(
                    "Executable `{replacement}` exists in this host's PATH and closely matches `{executable}`."
                ),
                evidence: Evidence::Path,
            })
        })
        .take(MAX_CANDIDATES)
        .collect()
}

fn capture(program: &str, args: &[&str]) -> Option<String> {
    let output = crate::host::command(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let end = output.stdout.len().min(MAX_PROBE_BYTES);
    Some(String::from_utf8_lossy(&output.stdout[..end]).into_owned())
}

fn path_commands() -> Vec<String> {
    if crate::host::command_available("bash") {
        if let Some(output) = capture(
            "bash",
            &[
                "--noprofile",
                "--norc",
                "-lc",
                "compgen -c | LC_ALL=C sort -u",
            ],
        ) {
            let names: Vec<String> = output
                .lines()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect();
            if !names.is_empty() {
                return names;
            }
        }
    }

    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut names = HashSet::new();
    for directory in std::env::split_paths(&path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                names.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    names.into_iter().collect()
}

struct Ranked {
    name: String,
    distance: usize,
    fuzzy: i64,
    length_delta: usize,
}

fn rank(needle: &str, names: impl IntoIterator<Item = String>) -> Vec<String> {
    let needle = needle.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let max_distance = if needle.chars().count() <= 7 { 2 } else { 3 };
    let first = needle.chars().next();
    let matcher = SkimMatcherV2::default();
    let mut seen = HashSet::new();
    let mut ranked = Vec::new();

    for name in names {
        let name = name.trim();
        if name.is_empty() || name.eq_ignore_ascii_case(needle.as_str()) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if !seen.insert(lower.clone()) {
            continue;
        }
        let distance = edit_distance(&needle, &lower);
        if distance > max_distance || (first != lower.chars().next() && distance > 1) {
            continue;
        }
        ranked.push(Ranked {
            name: name.to_string(),
            distance,
            fuzzy: matcher.fuzzy_match(&lower, &needle).unwrap_or(i64::MIN / 4),
            length_delta: lower.chars().count().abs_diff(needle.chars().count()),
        });
    }

    ranked.sort_by(|left, right| {
        left.distance
            .cmp(&right.distance)
            .then_with(|| right.fuzzy.cmp(&left.fuzzy))
            .then_with(|| left.length_delta.cmp(&right.length_delta))
            .then_with(|| left.name.cmp(&right.name))
    });
    ranked
        .into_iter()
        .take(MAX_CANDIDATES * 4)
        .map(|item| item.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking_handles_expected_typos() {
        assert_eq!(
            rank(
                "fmpg",
                ["fping", "ffmpeg", "imagemagick"]
                    .into_iter()
                    .map(str::to_string),
            )
            .first()
            .map(String::as_str),
            Some("ffmpeg")
        );
        assert_eq!(
            rank(
                "gti",
                ["git", "gio", "gtk4-demo"]
                    .into_iter()
                    .map(str::to_string),
            )
            .first()
            .map(String::as_str),
            Some("git")
        );
    }
}
