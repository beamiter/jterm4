#!/usr/bin/env python3
"""Move logging into a tested module and support RUST_LOG target directives."""

from __future__ import annotations

from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one match in {path}, found {count}: {old[:100]!r}")
    path.write_text(text.replace(old, new, 1))


root = Path(__file__).resolve().parents[1]
main = root / "src/main.rs"
lib = root / "src/lib.rs"
readme = root / "README.md"
guide = root / "docs/USER_GUIDE.md"
logging = root / "src/logging.rs"

logging.write_text(
    r'''//! Lightweight stderr logging with target-aware `RUST_LOG` directives.
//!
//! jterm4 intentionally avoids a larger logging dependency in its startup path,
//! but still supports the common `target=level` syntax used by Rust tooling.

use log::{LevelFilter, Log, Metadata, Record};
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
        self.directives
            .iter()
            .map(|(_, level)| *level)
            .fold(self.default_level, std::cmp::max)
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

    for directive in input.split(',').map(str::trim).filter(|part| !part.is_empty()) {
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
    directives.sort_by(|left, right| right.0.len().cmp(&left.0.len()));
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
        assert_eq!(filter.level_for("jterm4::state::restore"), LevelFilter::Trace);
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
'''
)

replace_once(
    main,
    '''use log::{LevelFilter, Log, Metadata, Record};
''',
    '''''',
)
replace_once(
    main,
    '''use crate::keybindings::{normalize_key, Action, KeyCombo};
use crate::state::{''',
    '''use crate::keybindings::{normalize_key, Action, KeyCombo};
use crate::logging::init_logging;
use crate::state::{''',
)
replace_once(
    main,
    '''struct SimpleStderrLogger {
    level: LevelFilter,
}

impl Log for SimpleStderrLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

fn parse_level_filter(input: &str) -> LevelFilter {
    match input.trim().to_ascii_lowercase().as_str() {
        "off" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Warn,
    }
}

fn init_logging() {
    let level = std::env::var("JTERM4_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .as_deref()
        .map(parse_level_filter)
        .unwrap_or(LevelFilter::Warn);

    let _ = log::set_boxed_logger(Box::new(SimpleStderrLogger { level }));
    log::set_max_level(level);
}

''',
    '''''',
)

replace_once(
    lib,
    '''pub mod keybindings;
pub mod notify;''',
    '''pub mod keybindings;
pub mod logging;
pub mod notify;''',
)

replace_once(
    readme,
    '''配置文件保存后会自动热重载；`Ctrl+Shift+R` 可手动重载。无效的新配置不会覆盖当前正在运行的有效配置。
''',
    '''配置文件保存后会自动热重载；`Ctrl+Shift+R` 可手动重载。无效的新配置不会覆盖当前正在运行的有效配置。

日志支持普通级别和标准 target 指令，并输出进程内相对时间、级别与模块名：

```bash
JTERM4_LOG=debug jterm4
RUST_LOG='warn,jterm4=debug,jterm4::state=trace' jterm4
```

`JTERM4_LOG` 优先于 `RUST_LOG`；未知指令会被忽略，默认级别保持 `warn`。
''',
)

replace_once(
    guide,
    '''这些命令在 GTK 初始化前完成，因此可在 SSH、TTY 和 CI 中运行。使用其他配置文件：''',
    '''这些命令在 GTK 初始化前完成，因此可在 SSH、TTY 和 CI 中运行。`--doctor` 还会报告 ready / active 会话快照数量。日志可用 `JTERM4_LOG=debug`，或使用 `RUST_LOG='warn,jterm4=debug,jterm4::state=trace'` 按模块设置；每行包含相对时间、级别和 target。使用其他配置文件：''',
)

for path in (main, lib, logging, readme, guide):
    if path == logging:
        continue
    if "parse_level_filter" in path.read_text() or "SimpleStderrLogger" in path.read_text():
        raise SystemExit(f"stale inline logger remains in {path}")

print("structured logging migration applied")
