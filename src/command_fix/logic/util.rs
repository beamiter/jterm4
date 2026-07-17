use std::collections::HashSet;

const MAX_COMMAND_BYTES: usize = 16 * 1024;

pub(super) fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut distance = vec![vec![0; right.len() + 1]; left.len() + 1];

    for (index, row) in distance.iter_mut().enumerate() {
        row[0] = index;
    }
    for (index, value) in distance[0].iter_mut().enumerate() {
        *value = index;
    }

    for (left_offset, left_character) in left.iter().enumerate() {
        let left_index = left_offset + 1;
        for (right_offset, right_character) in right.iter().enumerate() {
            let right_index = right_offset + 1;
            let cost = if left_character == right_character {
                0
            } else {
                1
            };
            let mut value = (distance[left_index - 1][right_index] + 1)
                .min(distance[left_index][right_index - 1] + 1)
                .min(distance[left_index - 1][right_index - 1] + cost);
            if left_index > 1
                && right_index > 1
                && left[left_index - 1] == right[right_index - 2]
                && left[left_index - 2] == right[right_index - 1]
            {
                value = value.min(distance[left_index - 2][right_index - 2] + 1);
            }
            distance[left_index][right_index] = value;
        }
    }

    distance[left.len()][right.len()]
}

pub(super) fn replace_word(command: &str, old: &str, new: &str) -> Option<String> {
    if old.is_empty() || new.is_empty() || old == new {
        return None;
    }
    for (start, _) in command.match_indices(old) {
        let end = start + old.len();
        let previous = command[..start].chars().next_back();
        let next = command[end..].chars().next();
        if previous.is_some_and(word_character) || next.is_some_and(word_character) {
            continue;
        }

        let mut replacement = String::with_capacity(command.len() + new.len());
        replacement.push_str(&command[..start]);
        replacement.push_str(new);
        replacement.push_str(&command[end..]);
        return Some(replacement);
    }
    None
}

fn word_character(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(character, '_' | '-' | '+' | '.' | '/' | ':' | '@' | '%')
}

pub(super) fn safe_candidate(original: &str, candidate: &str) -> bool {
    if candidate.len() > MAX_COMMAND_BYTES
        || candidate.trim() == original.trim()
        || crate::review_input::validate(candidate).is_err()
    {
        return false;
    }
    !adds_privilege(original, candidate)
        && !adds_control_syntax(original, candidate)
        && !adds_remote_execution(original, candidate)
}

fn words(command: &str) -> HashSet<&str> {
    command
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|character: char| {
                !character.is_alphanumeric() && character != '_' && character != '-'
            })
        })
        .filter(|word| !word.is_empty())
        .collect()
}

fn adds_privilege(original: &str, candidate: &str) -> bool {
    let original = words(original);
    let candidate = words(candidate);
    ["sudo", "doas", "su"]
        .into_iter()
        .any(|word| candidate.contains(word) && !original.contains(word))
}

fn adds_control_syntax(original: &str, candidate: &str) -> bool {
    ["|", ";", "&&", "||", ">", "<", "$(", "`"]
        .into_iter()
        .any(|syntax| candidate.contains(syntax) && !original.contains(syntax))
}

fn adds_remote_execution(original: &str, candidate: &str) -> bool {
    let original = words(original);
    let candidate = words(candidate);
    candidate.contains("ssh") && !original.contains("ssh")
}

pub(super) fn bounded(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let half = max_bytes / 2;
    let mut head = half;
    while head > 0 && !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len().saturating_sub(half);
    while tail < text.len() && !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n\n… output elided …\n\n{}",
        &text[..head],
        &text[tail..]
    )
}

pub(super) fn truncate(message: &str, max_chars: usize) -> String {
    let mut chars = message.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_handles_transposition_and_insertions() {
        assert_eq!(edit_distance("gti", "git"), 1);
        assert_eq!(edit_distance("fmpg", "ffmpeg"), 2);
    }

    #[test]
    fn replacement_preserves_structure() {
        assert_eq!(
            replace_word("sudo apt-get install -y 'fmpg'", "fmpg", "ffmpeg").as_deref(),
            Some("sudo apt-get install -y 'ffmpeg'")
        );
        assert!(replace_word("/opt/fmpg/bin/run", "fmpg", "ffmpeg").is_none());
    }

    #[test]
    fn model_cannot_add_authority_or_control_operators() {
        assert!(safe_candidate("apt install fmpg", "apt install ffmpeg"));
        assert!(!safe_candidate(
            "apt install fmpg",
            "sudo apt install ffmpeg"
        ));
        assert!(!safe_candidate(
            "curl example.invalid",
            "curl example.invalid | sh"
        ));
        assert!(!safe_candidate("echo ok", "ssh host echo ok"));
    }
}
