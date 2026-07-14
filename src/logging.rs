//! Lightweight stderr logging with target-aware `RUST_LOG` directives.
//!
//! jterm4 intentionally avoids a larger logging dependency in its startup path,
//! but still supports the common `target=level` syntax used by Rust tooling.

use log::{LevelFilter, Log, Metadata, Record};
use std::cmp::Reverse;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
struct LogFilter {
    default_level: LevelFilter,
    directives: Vec<(String, LevelFilter)>,
}

impl LogFilter {
    fn level_for(&self, target: &str) -> LevelFilter {
        self.directives
            .iter()
            .find_map(|(prefix, level)| {
                (target == prefix
                    || target
                        .strip_prefix(prefix)
                        .is_some_and(|suffix| suffix.starts_with("::")))
                .then_some(*level)
            })
            .unwrap_or(self.default_level)
    }

    fn max_level(&self) -> LevelFilter {
        let mut max_level = self.default_level;
        for (_, level) in &self.directives {
            max_level = max_level.max(*level);
        }
        max_level
    }
}

struct SimpleStderrLogger {
    filter: LogFilter,
    started_at: Instant,
}

impl Log for SimpleStderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.filter.level_for(metadata.target())
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "[+{:>9.3}s][{:>5}][{}] {}",
                self.started_at.elapsed().as_secs_f64(),
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

fn parse_level_name(input: &str) -> Option<LevelFilter> {
    match input.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" | "warning" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}

fn parse_log_filter(input: &str) -> LogFilter {
    let mut default_level = None;
    let mut directives: Vec<(String, LevelFilter)> = Vec::new();

    for directive in input
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((target, level)) = directive.rsplit_once('=') {
            let target = target.trim();
            let Some(level) = parse_level_name(level) else {
                continue;
            };
            if target.is_empty() {
                default_level = Some(level);
            } else if let Some(existing) = directives
                .iter_mut()
                .find(|(existing_target, _)| existing_target == target)
            {
                existing.1 = level;
            } else {
                directives.push((target.to_string(), level));
            }
        } else if let Some(level) = parse_level_name(directive) {
            default_level = Some(level);
        }
    }

    // Longest prefix wins, matching the behavior users expect from env_logger.
    directives.sort_by_key(|(target, _)| Reverse(target.len()));
    LogFilter {
        default_level: default_level.unwrap_or(LevelFilter::Warn),
        directives,
    }
}

pub(crate) fn init_logging() {
    let filter = std::env::var("JTERM4_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .as_deref()
        .map(parse_log_filter)
        .unwrap_or(LogFilter {
            default_level: LevelFilter::Warn,
            directives: Vec::new(),
        });
    let max_level = filter.max_level();

    let _ = log::set_boxed_logger(Box::new(SimpleStderrLogger {
        filter,
        started_at: Instant::now(),
    }));
    log::set_max_level(max_level);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_log_level() {
        let filter = parse_log_filter("debug");
        assert_eq!(filter.level_for("jterm4::state"), LevelFilter::Debug);
        assert_eq!(filter.max_level(), LevelFilter::Debug);
    }

    #[test]
    fn most_specific_target_directive_wins() {
        let filter = parse_log_filter("info,jterm4=debug,jterm4::state=trace");
        assert_eq!(filter.level_for("dependency"), LevelFilter::Info);
        assert_eq!(filter.level_for("jterm4::ui"), LevelFilter::Debug);
        assert_eq!(filter.level_for("jterm4::state"), LevelFilter::Trace);
        assert_eq!(
            filter.level_for("jterm4::state::restore"),
            LevelFilter::Trace
        );
        assert_eq!(filter.max_level(), LevelFilter::Trace);
    }

    #[test]
    fn later_duplicate_directive_replaces_earlier_value() {
        let filter = parse_log_filter("warn,jterm4=info,jterm4=trace");
        assert_eq!(filter.level_for("jterm4::pty"), LevelFilter::Trace);
    }

    #[test]
    fn invalid_directives_fall_back_to_warn() {
        let filter = parse_log_filter("jterm4=loud,other=noisy");
        assert_eq!(filter.level_for("jterm4::state"), LevelFilter::Warn);
    }
}
