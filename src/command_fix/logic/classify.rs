use super::util::edit_distance;
use super::{Context, Failure};

pub(crate) fn classify(context: &Context) -> Option<Failure> {
    if let Some(package) = line_suffix(&context.output, "unable to locate package") {
        return Some(Failure::AptPackageNotFound(package));
    }

    let command_not_found = command_not_found(&context.output);
    let unknown_subcommand = unknown_token(
        &context.output,
        &[
            "unknown command",
            "unknown subcommand",
            "unrecognized command",
            "invalid choice",
        ],
    );
    let unknown_option = unknown_token(
        &context.output,
        &["unknown option", "unrecognized option", "invalid option"],
    );

    if let Some(new) = tool_suggestion(&context.output) {
        let old = command_not_found
            .clone()
            .or_else(|| unknown_subcommand.clone())
            .or_else(|| unknown_option.clone())
            .or_else(|| closest_word(&context.command, &new));
        if let Some(old) = old.filter(|old| old != &new) {
            return Some(Failure::ToolSuggestion { old, new });
        }
    }

    command_not_found
        .map(Failure::CommandNotFound)
        .or_else(|| unknown_subcommand.map(Failure::UnknownSubcommand))
        .or_else(|| unknown_option.map(Failure::UnknownOption))
}

fn line_suffix(output: &str, marker: &str) -> Option<String> {
    let marker = marker.to_ascii_lowercase();
    output.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        let index = lower.find(&marker)?;
        clean_token(&line[index + marker.len()..])
    })
}

fn command_not_found(output: &str) -> Option<String> {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(index) = lower.find("command not found:") {
            if let Some(token) = clean_token(&line[index + "command not found:".len()..]) {
                return Some(token);
            }
        }
        if let Some(index) = lower.find(": command not found") {
            let prefix = &line[..index];
            if let Some(token) = clean_token(prefix.rsplit(':').next().unwrap_or(prefix)) {
                return Some(token);
            }
        }
        if lower.contains("unknown command:")
            && let Some(token) = line_suffix(line, "unknown command:")
        {
            return Some(token);
        }
        if let Some(index) = lower.rfind(": not found") {
            let prefix = &line[..index];
            if let Some(token) = clean_token(prefix.rsplit(':').next().unwrap_or(prefix)) {
                return Some(token);
            }
        }
    }
    None
}

fn unknown_token(output: &str, markers: &[&str]) -> Option<String> {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        for marker in markers {
            if let Some(index) = lower.find(marker) {
                let tail = &line[index + marker.len()..];
                if let Some(token) = quoted(tail).into_iter().next() {
                    return Some(token);
                }
                if let Some(token) = clean_token(tail) {
                    return Some(token);
                }
            }
        }
    }
    None
}

fn tool_suggestion(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        let marker_end = if let Some(start) = lower.find("did you mean") {
            Some(start + "did you mean".len())
        } else if let Some(start) = lower.find("most similar command") {
            Some(start + "most similar command".len())
        } else {
            lower
                .find("perhaps you meant")
                .map(|start| start + "perhaps you meant".len())
        };
        let Some(marker_end) = marker_end else {
            continue;
        };

        if let Some(token) = quoted(line).into_iter().last() {
            return Some(token);
        }
        let suffix = line[marker_end..].trim().trim_start_matches(':').trim();
        if !suffix.is_empty()
            && !matches!(suffix.to_ascii_lowercase().as_str(), "is" | "is:")
            && let Some(token) = clean_token(suffix)
        {
            return Some(token);
        }
        if let Some(token) = lines
            .iter()
            .skip(index + 1)
            .map(|next| next.trim())
            .find(|next| !next.is_empty())
            .and_then(clean_token)
        {
            return Some(token);
        }
    }
    None
}

fn quoted(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut values = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let quote = chars[index];
        if !matches!(quote, '\'' | '"' | '`') {
            index += 1;
            continue;
        }
        let start = index + 1;
        index += 1;
        while index < chars.len() && chars[index] != quote {
            index += 1;
        }
        if index < chars.len() {
            let value: String = chars[start..index].iter().collect();
            if let Some(value) = clean_token(&value) {
                values.push(value);
            }
        }
        index += 1;
    }
    values
}

fn clean_token(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_start_matches(':')
        .trim()
        .trim_matches(trim_error_character);
    let value = value
        .split_whitespace()
        .next()?
        .trim_matches(trim_error_character);
    (!value.is_empty()).then(|| value.to_string())
}

fn trim_error_character(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '\'' | '"' | '`' | ':' | ';' | ',' | '.' | '?' | '(' | ')' | '[' | ']'
        )
}

fn closest_word(command: &str, suggestion: &str) -> Option<String> {
    command
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|character: char| {
                matches!(
                    character,
                    '\'' | '"' | '`' | ':' | ';' | ',' | '|' | '&' | '(' | ')'
                )
            })
        })
        .filter(|word| !word.is_empty() && !word.starts_with('-'))
        .filter(|word| !matches!(*word, "sudo" | "doas" | "env" | "command"))
        .min_by_key(|word| {
            edit_distance(
                &word.to_ascii_lowercase(),
                &suggestion.to_ascii_lowercase(),
            )
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(command: &str, output: &str) -> Context {
        Context {
            command: command.to_string(),
            output: output.to_string(),
            remote: false,
        }
    }

    #[test]
    fn recognizes_package_and_command_typos() {
        assert_eq!(
            classify(&context(
                "sudo apt install fmpg",
                "E: Unable to locate package fmpg"
            )),
            Some(Failure::AptPackageNotFound("fmpg".into()))
        );
        for output in [
            "bash: gti: command not found",
            "zsh: command not found: gti",
            "sh: 1: gti: not found",
            "fish: Unknown command: gti",
        ] {
            assert_eq!(
                classify(&context("gti status", output)),
                Some(Failure::CommandNotFound("gti".into()))
            );
        }
    }

    #[test]
    fn recognizes_tool_suggestion_on_following_line() {
        let context = context(
            "git statsu",
            "git: 'statsu' is not a git command.\n\nThe most similar command is\n\tstatus",
        );
        assert_eq!(
            classify(&context),
            Some(Failure::ToolSuggestion {
                old: "statsu".into(),
                new: "status".into()
            })
        );
    }
}
