//! Pure fuzzy-ranking layer for the unified command palette.
//!
//! Prefixes narrow the source: `>` actions, `@` persisted history, `:`
//! workflows and `?` natural-language command generation.

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use std::path::{Path, PathBuf};

use crate::command_history;
use crate::keybindings::{Action, KeybindingMap};
use crate::workflows::Workflow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteMode {
    All,
    Commands,
    History,
    Ai,
    Workflows,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query {
    pub(crate) mode: PaletteMode,
    pub(crate) text: String,
}

impl Query {
    pub(crate) fn parse(raw: &str, default_mode: PaletteMode) -> Self {
        let trimmed = raw.trim_start();
        for (prefix, mode) in [
            ('>', PaletteMode::Commands),
            ('@', PaletteMode::History),
            ('?', PaletteMode::Ai),
            (':', PaletteMode::Workflows),
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                return Self {
                    mode,
                    text: rest.trim_start().to_string(),
                };
            }
        }
        Self {
            mode: default_mode,
            text: trimmed.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Accept {
    Action(Action),
    TypeCommand(String),
    AskAi(String),
    RunWorkflow(PathBuf),
}

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) tier: u8,
    pub(crate) score: i64,
    pub(crate) label: String,
    pub(crate) sublabel: Option<String>,
    pub(crate) right: Option<String>,
    pub(crate) accept: Accept,
}

pub(crate) fn gather(
    query: &Query,
    keybindings: &KeybindingMap,
    history_path: Option<&Path>,
    workflows: &[Workflow],
    limit: usize,
) -> Vec<Entry> {
    let matcher = SkimMatcherV2::default().smart_case();
    let mut entries = Vec::new();

    if matches!(query.mode, PaletteMode::All | PaletteMode::Commands) {
        for (action, binding) in keybindings.all_bound_actions() {
            push_if_match(
                &matcher,
                &query.text,
                Entry {
                    tier: 0,
                    score: 0,
                    label: action.name().to_string(),
                    sublabel: None,
                    right: (!binding.is_empty()).then_some(binding),
                    accept: Accept::Action(action),
                },
                &mut entries,
            );
        }
    }

    if matches!(query.mode, PaletteMode::All | PaletteMode::Workflows) {
        for workflow in workflows {
            let tag_text = workflow.tags.join(",");
            let searchable = if tag_text.is_empty() {
                workflow.description.clone()
            } else if workflow.description.is_empty() {
                tag_text.clone()
            } else {
                format!("{} · {tag_text}", workflow.description)
            };
            push_if_match(
                &matcher,
                &query.text,
                Entry {
                    tier: 1,
                    score: 0,
                    label: format!("⚙ {}", workflow.name),
                    sublabel: Some(if searchable.is_empty() {
                        workflow.command.clone()
                    } else {
                        searchable
                    }),
                    right: (!tag_text.is_empty()).then(|| format!(":{tag_text}")),
                    accept: Accept::RunWorkflow(workflow.source_path.clone()),
                },
                &mut entries,
            );
        }
    }

    if query.mode == PaletteMode::Ai {
        let text = query.text.trim();
        entries.push(Entry {
            tier: 0,
            score: i64::MAX,
            label: if text.is_empty() {
                "Type a natural-language request after ?".to_string()
            } else {
                format!("Ask AI: {text}")
            },
            sublabel: Some(if text.is_empty() {
                "e.g. ? find files modified today".to_string()
            } else {
                "Generates a shell command for review before running".to_string()
            }),
            right: Some("?".to_string()),
            accept: if text.is_empty() {
                Accept::TypeCommand(String::new())
            } else {
                Accept::AskAi(text.to_string())
            },
        });
        return entries;
    }

    if matches!(query.mode, PaletteMode::All | PaletteMode::History) {
        if let Some(path) = history_path {
            let history = command_history::read_recent(path, 2_000).unwrap_or_default();
            let count = history.len();
            for (index, item) in history.into_iter().enumerate() {
                let cwd = item.cwd.clone().unwrap_or_default();
                let status = if item.exit_code == 0 {
                    "success".to_string()
                } else {
                    format!("exit {}", item.exit_code)
                };
                push_if_match(
                    &matcher,
                    &query.text,
                    Entry {
                        tier: 2,
                        score: (count - index) as i64,
                        label: item.command.clone(),
                        sublabel: Some(if cwd.is_empty() {
                            status
                        } else {
                            format!("{status} · {cwd}")
                        }),
                        right: None,
                        accept: Accept::TypeCommand(item.command),
                    },
                    &mut entries,
                );
            }
        }
    }

    entries.sort_by(|a, b| a.tier.cmp(&b.tier).then(b.score.cmp(&a.score)));
    entries.truncate(limit);
    entries
}

fn push_if_match(
    matcher: &SkimMatcherV2,
    needle: &str,
    mut entry: Entry,
    entries: &mut Vec<Entry>,
) {
    if needle.is_empty() {
        entries.push(entry);
        return;
    }
    let primary = matcher.fuzzy_match(&entry.label, needle);
    let secondary = entry
        .sublabel
        .as_deref()
        .and_then(|value| matcher.fuzzy_match(value, needle));
    if let Some(score) = match (primary, secondary) {
        (Some(a), Some(b)) => Some(a.max(b / 2)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b / 2),
        (None, None) => None,
    } {
        entry.score = entry.score.saturating_add(score.saturating_mul(10_000));
        entries.push(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_select_sources() {
        assert_eq!(
            Query::parse("  @ cargo", PaletteMode::All),
            Query {
                mode: PaletteMode::History,
                text: "cargo".into()
            }
        );
        assert_eq!(
            Query::parse(":deploy", PaletteMode::All).mode,
            PaletteMode::Workflows
        );
        assert_eq!(
            Query::parse("? explain", PaletteMode::All).mode,
            PaletteMode::Ai
        );
        assert_eq!(
            Query::parse("> close", PaletteMode::All).mode,
            PaletteMode::Commands
        );
    }

    #[test]
    fn fuzzy_actions_are_ranked_and_limited() {
        let entries = gather(
            &Query::parse("> newtab", PaletteMode::All),
            &KeybindingMap::from_defaults(),
            None,
            &[],
            5,
        );
        assert!(!entries.is_empty());
        assert!(entries[0].label.to_ascii_lowercase().contains("new tab"));
        assert!(entries.len() <= 5);
    }

    #[test]
    fn ai_query_never_auto_executes() {
        let entries = gather(
            &Query::parse("? list large files", PaletteMode::All),
            &KeybindingMap::from_defaults(),
            None,
            &[],
            10,
        );
        assert!(matches!(&entries[0].accept, Accept::AskAi(text) if text == "list large files"));
    }
}
