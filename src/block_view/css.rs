//! css — extracted from block_view (mechanical split, no logic changes)
use crate::config::Config;
use gtk4::gdk::RGBA;
use std::cell::RefCell;

/// Vertical chrome the `.block-active` holder adds around the live VTE:
/// 4px top margin + 4px bottom margin + 1px top border + 1px bottom border +
/// 2px top padding + 2px bottom padding = 14px.
///
/// Used by `update_input_height` to subtract this from the visible page size
/// before computing how many VTE rows fit. Must stay in sync with the
/// `.block-active` rule below; if the margin/border/padding here changes,
/// update this constant too.
pub(crate) const BLOCK_ACTIVE_VCHROME_PX: i32 = 14;

pub(crate) fn rgba_to_hex(c: &RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8,
    )
}

pub(crate) fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    let parts: Vec<&str> = display.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        display
    } else {
        format!("…/{}", parts[parts.len() - 2..].join("/"))
    }
}

/// Cheap git-branch lookup for the context chip: walk up from `cwd` to find a
/// `.git` dir (or `.git` file for worktrees/submodules), then read `HEAD`. No
/// subprocess, no dirty-state — just the branch name (or short SHA if detached).
pub(crate) fn git_branch_for(cwd: &str) -> Option<String> {
    use std::path::{Path, PathBuf};
    let mut dir: Option<&Path> = Some(Path::new(cwd));
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        let head_path: Option<PathBuf> = if dot_git.is_dir() {
            Some(dot_git.join("HEAD"))
        } else if dot_git.is_file() {
            // "gitdir: <path>" → real git dir lives elsewhere
            std::fs::read_to_string(&dot_git).ok().and_then(|c| {
                c.strip_prefix("gitdir:").map(|p| {
                    let g = Path::new(p.trim());
                    if g.is_absolute() {
                        g.join("HEAD")
                    } else {
                        d.join(g).join("HEAD")
                    }
                })
            })
        } else {
            None
        };
        if let Some(hp) = head_path {
            if let Ok(head) = std::fs::read_to_string(&hp) {
                let head = head.trim();
                if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
                    return Some(branch.to_string());
                }
                // Detached HEAD: show short SHA.
                if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(head[..7].to_string());
                }
                return None;
            }
        }
        dir = d.parent();
    }
    None
}

pub(crate) fn chrono_local_offset_secs() -> i64 {
    use nix::libc;
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_gmtoff
    }
}

// ─── CSS ──────────────────────────────────────────────────────────────────────

pub(crate) fn install_block_css(config: &Config) {
    let fg = &config.foreground;
    let bg = &config.background;
    let bg_hex = rgba_to_hex(bg);
    let fg_hex = rgba_to_hex(fg);
    let dim_fg = format!(
        "rgba({},{},{},0.55)",
        (fg.red() * 255.0) as u8,
        (fg.green() * 255.0) as u8,
        (fg.blue() * 255.0) as u8,
    );
    // Accent color for active chevron (use palette color 2 = green-ish)
    let accent = rgba_to_hex(&config.palette[2]);
    // Error color for bad exit codes — use the theme's red (palette 1) so it
    // matches what VTE would render, instead of a hard-coded swatch.
    let err = &config.palette[1];
    let err_hex = rgba_to_hex(err);
    let err_bg = format!(
        "rgba({},{},{},0.18)",
        (err.red() * 255.0) as u8,
        (err.green() * 255.0) as u8,
        (err.blue() * 255.0) as u8,
    );

    // Status-stripe colors derived from the theme palette: green (palette 2) for
    // success, red (palette 1) for failure. Kept semi-transparent so the stripe
    // reads as an accent rather than a hard bar.
    let ok = &config.palette[2];
    let ok_stripe = format!(
        "rgba({},{},{},0.55)",
        (ok.red() * 255.0) as u8,
        (ok.green() * 255.0) as u8,
        (ok.blue() * 255.0) as u8,
    );
    let ok_hex = rgba_to_hex(ok);
    let err_stripe = format!(
        "rgba({},{},{},0.70)",
        (err.red() * 255.0) as u8,
        (err.green() * 255.0) as u8,
        (err.blue() * 255.0) as u8,
    );

    // Per-channel components for the success/error/accent colors, used to build
    // tinted backgrounds and focus glows directly in the CSS template.
    let ok_r = (ok.red() * 255.0) as u8;
    let ok_g = (ok.green() * 255.0) as u8;
    let ok_b = (ok.blue() * 255.0) as u8;
    let err_r = (err.red() * 255.0) as u8;
    let err_g = (err.green() * 255.0) as u8;
    let err_b = (err.blue() * 255.0) as u8;
    // Accent == palette[2] (same green as success); reused for the active-card
    // focus ring and prompt chevron.
    let acc = &config.palette[2];
    let acc_r = (acc.red() * 255.0) as u8;
    let acc_g = (acc.green() * 255.0) as u8;
    let acc_b = (acc.blue() * 255.0) as u8;

    let fg_r = (fg.red() * 255.0) as u8;
    let fg_g = (fg.green() * 255.0) as u8;
    let fg_b = (fg.blue() * 255.0) as u8;

    // Slightly different background for finished blocks (3% toward fg)
    let bg_r = (bg.red() * 255.0) as u8;
    let bg_g = (bg.green() * 255.0) as u8;
    let bg_b = (bg.blue() * 255.0) as u8;
    let block_bg_hex = format!(
        "#{:02x}{:02x}{:02x}",
        (bg_r as f32 + (fg_r as f32 - bg_r as f32) * 0.03) as u8,
        (bg_g as f32 + (fg_g as f32 - bg_g as f32) * 0.03) as u8,
        (bg_b as f32 + (fg_b as f32 - bg_b as f32) * 0.03) as u8,
    );

    // Parse font description to extract font family and size
    // Format: "FontName Style Size" e.g. "SauceCodePro Nerd Font Mono 14"
    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let (font_family, base_size) = if parts.len() >= 2 {
        // Last part is usually the size. Pango allows float sizes ("Fira Code 12.5"),
        // so parse as f64 and round rather than rejecting non-integer sizes.
        if let Ok(size) = parts[parts.len() - 1].parse::<f64>() {
            let family = parts[..parts.len() - 1].join(" ");
            (family, size.round().max(1.0) as i32)
        } else {
            (config.font_desc.clone(), 14)
        }
    } else {
        (config.font_desc.clone(), 14)
    };
    // Escape the family name so a quote/backslash in the font name can't break the
    // surrounding CSS string and silently disable the whole stylesheet.
    let font_family = font_family.replace('\\', "\\\\").replace('"', "\\\"");

    // Apply font scale to the base size
    let scaled_size = (base_size as f64 * config.default_font_scale)
        .round()
        .max(1.0) as i32;
    let font_size = format!("{}pt", scaled_size);

    let css = format!(
        r#"
        .block-scroll {{
            background-color: {bg_hex};
        }}
        .block-list {{
            background-color: {bg_hex};
        }}
        .block-finished {{
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.08);
            border-left: 3px solid transparent;
            border-radius: 10px;
            background-color: {block_bg_hex};
            min-height: 40px;
            transition: background-color 140ms ease, border-color 140ms ease, box-shadow 140ms ease;
        }}
        .block-success {{
            border-left-color: {ok_stripe};
        }}
        .block-failed {{
            border-left-color: {err_stripe};
            background-color: rgba({err_r},{err_g},{err_b},0.11);
            box-shadow: inset 2px 0 0 0 {err_stripe};
        }}
        .block-hovered {{
            background-color: rgba({fg_r},{fg_g},{fg_b},0.05);
            border-top-color: rgba({fg_r},{fg_g},{fg_b},0.16);
            border-right-color: rgba({fg_r},{fg_g},{fg_b},0.16);
            border-bottom-color: rgba({fg_r},{fg_g},{fg_b},0.16);
            box-shadow: 0 4px 14px rgba(0,0,0,0.22);
        }}
        .block-selected {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.12);
            border-color: rgba({acc_r},{acc_g},{acc_b},0.85);
            box-shadow: inset 0 0 0 2px {accent}, 0 0 0 1px rgba({acc_r},{acc_g},{acc_b},0.55);
        }}
        .block-active {{
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.32);
            border-left: 3px solid rgba({acc_r},{acc_g},{acc_b},0.85);
            border-radius: 10px;
            margin: 4px 8px;
            padding: 2px 0;
            background-color: {bg_hex};
            box-shadow: 0 2px 8px rgba(0,0,0,0.18);
        }}
        .block-prompt-chevron {{
            color: {accent};
            font-family: "{font_family}";
            font-size: {font_size};
            font-weight: bold;
            margin-left: 10px;
            margin-right: 6px;
        }}
        .block-chip {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.07);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.10);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 1px 9px;
        }}
        .block-bookmark-star {{
            color: #e5c07b;
            font-family: "{font_family}";
            font-size: 0.82em;
            margin-right: 2px;
        }}
        .block-bookmarked {{
            box-shadow: inset 3px 0 0 0 #e5c07b;
        }}
        .block-chip-git {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.10);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.22);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 1px 9px;
        }}
        .block-status-ok {{
            color: {ok_hex};
            background-color: rgba({ok_r},{ok_g},{ok_b},0.16);
            border-radius: 999px;
            min-width: 16px;
            min-height: 16px;
            padding: 1px 5px;
            font-family: "{font_family}";
            font-size: 0.82em;
            font-weight: bold;
        }}
        .block-status-bad {{
            color: {err_hex};
            background-color: rgba({err_r},{err_g},{err_b},0.18);
            border-radius: 999px;
            min-width: 16px;
            min-height: 16px;
            padding: 1px 5px;
            font-family: "{font_family}";
            font-size: 0.82em;
            font-weight: bold;
        }}
        .block-action-btn {{
            color: {dim_fg};
            min-width: 24px;
            min-height: 24px;
            padding: 0 4px;
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.9em;
            transition: background-color 120ms ease, color 120ms ease;
        }}
        .block-action-btn:hover {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.12);
        }}
        .block-action-active {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.18);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.34);
        }}
        .block-filter-row {{
            padding: 2px 0;
        }}
        .block-filter-toggle {{
            color: {dim_fg};
            min-width: 26px;
            min-height: 24px;
            padding: 0 4px;
            border-radius: 6px;
            font-family: "{font_family}";
            font-size: 0.8em;
        }}
        .block-filter-toggle:checked {{
            color: {fg_hex};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.35);
        }}
        .block-filter-status {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 0 6px;
        }}
        .block-filter-empty {{
            color: {err_hex};
        }}
        .block-header {{
            border-radius: 6px 6px 0 0;
        }}
        .block-header-label {{
            color: {dim_fg};
            font-size: 0.85em;
        }}
        .block-collapse-btn {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: 0.8em;
            min-width: 24px;
            min-height: 24px;
            padding: 0;
            border-radius: 999px;
            transition: background-color 120ms ease, color 120ms ease;
        }}
        .block-collapse-btn:hover {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.12);
        }}
        .block-prompt {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: {font_size};
            line-height: 1.0;
            margin: 0;
        }}
        .block-cmd {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
        }}
        .block-cmd-active {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
            background-color: {bg_hex};
            caret-color: {fg_hex};
        }}
        .block-cmd-active text {{
            background-color: {bg_hex};
            caret-color: {fg_hex};
        }}
        @keyframes blink {{
            0%, 49% {{ opacity: 1; }}
            50%, 100% {{ opacity: 0; }}
        }}
        .block-cmd-active text selection {{
            background-color: transparent;
        }}
        .block-cmd-finished {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
            background-color: {bg_hex};
        }}
        .block-cmd-finished text {{
            background-color: {bg_hex};
        }}
        .block-exit-bad {{
            color: {err_hex};
            background-color: {err_bg};
            border: 1px solid rgba({err_r},{err_g},{err_b},0.35);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            font-weight: bold;
            padding: 1px 8px;
        }}
        .block-meta-badge {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.08);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 1px 8px;
        }}
        .block-running-label {{
            color: {dim_fg};
            font-size: 0.85em;
            padding-right: 8px;
        }}
        .block-output {{
            background-color: {bg_hex};
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            min-height: 0;
            line-height: 1.0;
            padding: 0;
            margin: 0;
        }}
        .block-show-more {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.10);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.25);
            border-radius: 999px;
            margin-left: 12px;
            margin-top: 6px;
            margin-bottom: 4px;
            font-size: 0.82em;
            padding: 2px 12px;
            transition: background-color 120ms ease;
        }}
        .block-show-more:hover {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.18);
        }}
        .jump-bottom-fab {{
            color: {bg_hex};
            background-color: {accent};
            background-image: none;
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.55);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.92em;
            font-weight: bold;
            min-width: 18px;
            min-height: 18px;
            padding: 6px 12px;
            box-shadow: 0 4px 14px rgba(0,0,0,0.35);
            transition: background-color 120ms ease, box-shadow 120ms ease;
        }}
        .jump-bottom-fab:hover {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.85);
            box-shadow: 0 6px 18px rgba(0,0,0,0.45);
        }}
        .sticky-running-header {{
            background-color: {block_bg_hex};
            border-bottom: 1px solid rgba({acc_r},{acc_g},{acc_b},0.45);
            box-shadow: 0 3px 10px rgba(0,0,0,0.30);
            padding: 6px 14px;
        }}
        .sticky-running-label {{
            color: {accent};
            font-family: "{font_family}";
            font-size: 0.92em;
            font-weight: bold;
        }}
        .repo-strip {{
            color: rgba({acc_r},{acc_g},{acc_b},0.85);
            background-color: {block_bg_hex};
            font-family: "{font_family}";
            font-size: 0.85em;
            padding: 3px 14px;
            border-top: 1px solid rgba({acc_r},{acc_g},{acc_b},0.20);
        }}
        .command-palette > contents {{
            background-color: {block_bg_hex};
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.45);
            border-radius: 10px;
            padding: 10px;
            box-shadow: 0 10px 30px rgba(0,0,0,0.45);
        }}
        .command-palette-list {{
            background-color: transparent;
        }}
        .command-palette-list row {{
            padding: 0;
            border-radius: 6px;
        }}
        .command-palette-list row:selected {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.28);
        }}
        .command-palette-row {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: 0.92em;
            padding: 6px 10px;
        }}
        "#,
    );

    thread_local! {
        static BLOCK_CSS_PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
    }

    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css);
    let Some(display) = gtk4::gdk::Display::default() else {
        // No display (headless / CI). Nothing to style.
        return;
    };

    BLOCK_CSS_PROVIDER.with(|cell| {
        let mut prev = cell.borrow_mut();
        if let Some(old) = prev.take() {
            gtk4::style_context_remove_provider_for_display(&display, &old);
        }
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        *prev = Some(provider);
    });
}
