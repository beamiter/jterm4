//! css — extracted from block_view (mechanical split, no logic changes)
use crate::config::Config;
use gtk4::gdk::RGBA;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_GIT_POINTER_BYTES: u64 = 16 * 1024;
const MAX_BRANCH_DISPLAY_CHARS: usize = 256;
/// A negative lookup is useful while restoring many cards, but must not hide a
/// repository created immediately afterwards for long.
const GIT_NEGATIVE_CACHE_TTL: Duration = Duration::from_millis(200);
/// Distinct working directories remembered at once.
const GIT_BRANCH_CACHE_ENTRIES: usize = 64;

#[cfg(test)]
thread_local! {
    static GIT_BRANCH_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// Cards are built on the GTK thread only, so this needs no lock. Positive
    /// entries remember only the HEAD locator: HEAD itself is read for every
    /// card so a branch switch is visible immediately. Negative entries get a
    /// short TTL because walking a non-repository path is the expensive miss.
    static GIT_BRANCH_CACHE: RefCell<GitBranchCache> =
        const { RefCell::new(GitBranchCache::new()) };
}

#[derive(Clone, Debug)]
enum GitHeadCacheValue {
    Found(PathBuf),
    Missing { resolved_at: Instant },
}

#[derive(Debug)]
struct GitBranchCache {
    entries: VecDeque<(String, GitHeadCacheValue)>,
}

#[derive(Debug)]
enum GitHeadCacheHit {
    Found(PathBuf),
    Missing,
    None,
}

impl GitBranchCache {
    const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    fn get(&mut self, cwd: &str, now: Instant) -> GitHeadCacheHit {
        let Some(index) = self.entries.iter().position(|(key, _)| key == cwd) else {
            return GitHeadCacheHit::None;
        };
        let (_, value) = self.entries.remove(index).expect("cache index exists");
        match value {
            GitHeadCacheValue::Found(head) => {
                self.entries
                    .push_back((cwd.to_string(), GitHeadCacheValue::Found(head.clone())));
                GitHeadCacheHit::Found(head)
            }
            GitHeadCacheValue::Missing { resolved_at }
                if now.saturating_duration_since(resolved_at) < GIT_NEGATIVE_CACHE_TTL =>
            {
                self.entries
                    .push_back((cwd.to_string(), GitHeadCacheValue::Missing { resolved_at }));
                GitHeadCacheHit::Missing
            }
            GitHeadCacheValue::Missing { .. } => GitHeadCacheHit::None,
        }
    }

    fn remove(&mut self, cwd: &str) {
        if let Some(index) = self.entries.iter().position(|(key, _)| key == cwd) {
            self.entries.remove(index);
        }
    }

    fn insert(&mut self, cwd: &str, value: GitHeadCacheValue) {
        self.remove(cwd);
        self.entries.push_back((cwd.to_string(), value));
        while self.entries.len() > GIT_BRANCH_CACHE_ENTRIES {
            self.entries.pop_front();
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Vertical chrome the `.block-active` holder adds around the live VTE:
/// 4px top margin + 4px bottom margin + 3px top padding + 3px bottom padding
/// = 14px. The top/bottom borders became `outline`, which does not take part in
/// layout, and the 1px each used to contribute moved into the padding.
///
/// Used by `update_input_height` to subtract this from the visible page size
/// before computing how many VTE rows fit. Must stay in sync with the
/// `.block-active` rule below; if the margin/border/padding here changes,
/// update this constant too.
pub(crate) const BLOCK_ACTIVE_VCHROME_PX: i32 = 14;
/// Compact mode: 1px top/bottom margin + 1px top/bottom padding (the border
/// that used to supply those pixels is now a layout-free `outline`).
pub(crate) const BLOCK_ACTIVE_COMPACT_VCHROME_PX: i32 = 4;

pub(crate) fn rgba_to_hex(c: &RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8,
    )
}

fn shorten_path_with_home(path: &str, home: Option<&std::path::Path>) -> String {
    let path_obj = std::path::Path::new(path);
    let display = match home.and_then(|home| path_obj.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.to_string_lossy()),
        None => path.to_string(),
    };

    let parts: Vec<&str> = display.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() <= 3 {
        display
    } else {
        format!("…/{}", parts[parts.len() - 2..].join("/"))
    }
}

pub(crate) fn shorten_path(path: &str) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    shorten_path_with_home(path, home.as_deref())
}

/// Branch for the card's context chip, with its HEAD locator memoized.
///
/// The chip is built once per card and a restored session builds every card in
/// one pass. Remembering the locator avoids repeating the directory walk, while
/// reading HEAD on each call makes branch switches visible without a stale TTL.
pub(crate) fn git_branch_for(cwd: &str) -> Option<String> {
    git_branch_for_at(cwd, Instant::now())
}

fn git_branch_for_at(cwd: &str, now: Instant) -> Option<String> {
    match GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().get(cwd, now)) {
        GitHeadCacheHit::Found(head_path) => match read_git_head(&head_path) {
            Some(branch) => return branch,
            None => {
                // A worktree can replace its `.git` pointer. If the remembered
                // HEAD disappeared (or became unsafe), discard the locator and
                // perform exactly one fresh resolution below.
                GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().remove(cwd));
            }
        },
        GitHeadCacheHit::Missing => return None,
        GitHeadCacheHit::None => {}
    }

    let head_path = git_head_locator_uncached(cwd);
    match head_path {
        Some(head_path) => {
            GIT_BRANCH_CACHE.with(|cache| {
                cache
                    .borrow_mut()
                    .insert(cwd, GitHeadCacheValue::Found(head_path.clone()));
            });
            read_git_head(&head_path).flatten()
        }
        None => {
            GIT_BRANCH_CACHE.with(|cache| {
                cache
                    .borrow_mut()
                    .insert(cwd, GitHeadCacheValue::Missing { resolved_at: now });
            });
            None
        }
    }
}

/// Cheap git-branch lookup for the context chip: walk up from `cwd` to find a
/// `.git` dir (or `.git` file for worktrees/submodules), then read `HEAD`. No
/// subprocess, no dirty-state — just the branch name (or short SHA if detached).
pub(crate) fn git_branch_uncached(cwd: &str) -> Option<String> {
    let head_path = git_head_locator_uncached(cwd)?;
    read_git_head(&head_path).flatten()
}

fn git_head_locator_uncached(cwd: &str) -> Option<PathBuf> {
    #[cfg(test)]
    GIT_BRANCH_WALKS.with(|walks| walks.set(walks.get().saturating_add(1)));

    let mut dir: Option<&Path> = Some(Path::new(cwd));
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        let file_type = std::fs::symlink_metadata(&dot_git)
            .ok()
            .map(|metadata| metadata.file_type());
        let head_path: Option<PathBuf> = match file_type {
            Some(file_type) if file_type.is_dir() => Some(dot_git.join("HEAD")),
            Some(file_type) if file_type.is_file() => {
                // "gitdir: <path>" → real git dir lives elsewhere
                read_small_git_file(&dot_git).and_then(|c| {
                    c.strip_prefix("gitdir:").map(|p| {
                        let g = Path::new(p.trim());
                        if g.is_absolute() {
                            g.join("HEAD")
                        } else {
                            d.join(g).join("HEAD")
                        }
                    })
                })
            }
            _ => None,
        };
        if let Some(head_path) = head_path {
            return Some(head_path);
        }
        dir = d.parent();
    }
    None
}

/// `None` is an I/O or safety failure; `Some(None)` is readable HEAD content
/// which is neither a branch ref nor a detached commit.
fn read_git_head(path: &Path) -> Option<Option<String>> {
    let head = read_small_git_file(path)?;
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return Some(sanitize_branch(branch));
    }
    // Detached HEAD: show short SHA.
    if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(Some(head[..7].to_string()));
    }
    Some(None)
}

fn read_small_git_file(path: &Path) -> Option<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_GIT_POINTER_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_GIT_POINTER_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_GIT_POINTER_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn sanitize_branch(branch: &str) -> Option<String> {
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }
    let mut output = String::new();
    let mut chars = branch.chars();
    for ch in chars.by_ref().take(MAX_BRANCH_DISPLAY_CHARS) {
        if ch.is_control() || jterm_core::review_input::is_visual_spoofing_character(ch) {
            output.push('\u{fffd}');
        } else {
            output.push(ch);
        }
    }
    if chars.next().is_some() {
        output.push('…');
    }
    Some(output)
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
    let css = block_css(config);

    thread_local! {
        static BLOCK_CSS_PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
        /// The stylesheet currently installed. The provider is display-wide and
        /// every pane installs the same one, so a window with four Block panes
        /// used to parse and swap it four times per font-zoom notch — each swap
        /// a full style invalidation of every widget on the display. Panes call
        /// this whenever their own config changes; only a change in the
        /// generated text is worth acting on.
        static BLOCK_CSS_TEXT: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    if BLOCK_CSS_TEXT.with(|cell| cell.borrow().as_deref() == Some(css.as_str())) {
        return;
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
    BLOCK_CSS_TEXT.with(|cell| *cell.borrow_mut() = Some(css));
}

/// Build the pane stylesheet for `config`.
///
/// Separated from installing it so the cascade — which state class wins which
/// property when a card is failed *and* hovered *and* bookmarked — can be
/// asserted without a display.
pub(crate) fn block_css(config: &Config) -> String {
    let bg = &config.background;
    let semantic = config.semantic_colors();
    let fg = &semantic.foreground;
    let bg_hex = rgba_to_hex(bg);
    let fg_hex = rgba_to_hex(fg);
    let dim_fg = rgba_to_hex(&semantic.muted);
    // ANSI colors inside VTE remain exact. When the same hues become ordinary
    // UI text, adjust them just enough to remain readable on the theme's
    // background (especially the three light themes).
    let accent = rgba_to_hex(&semantic.accent);
    let err = &semantic.error;
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
    let ok = &semantic.success;
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
    let acc = &semantic.accent;
    let acc_r = (acc.red() * 255.0) as u8;
    let acc_g = (acc.green() * 255.0) as u8;
    let acc_b = (acc.blue() * 255.0) as u8;

    let fg_r = (fg.red() * 255.0) as u8;
    let fg_g = (fg.green() * 255.0) as u8;
    let fg_b = (fg.blue() * 255.0) as u8;

    // Unknown-outcome blocks use the theme's yellow (palette 3): a command whose
    // exit status the shell never reported is neither the green of a success nor
    // the red of a failure, and borrowing either colour would state something the
    // terminal does not know.
    let warn = &semantic.warning;
    let warn_hex = rgba_to_hex(warn);
    let warn_r = (warn.red() * 255.0) as u8;
    let warn_g = (warn.green() * 255.0) as u8;
    let warn_b = (warn.blue() * 255.0) as u8;
    let warn_stripe = format!("rgba({warn_r},{warn_g},{warn_b},0.62)");

    // Shell Agent inline cards use the theme's blue (palette 4) so they read
    // distinctly from success/correction accents (palette 2).
    let agent = &semantic.info;
    let agent_hex = rgba_to_hex(agent);
    let agent_r = (agent.red() * 255.0) as u8;
    let agent_g = (agent.green() * 255.0) as u8;
    let agent_b = (agent.blue() * 255.0) as u8;

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
    // Format: "FontName Style Size" e.g. "Monospace 14"
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
    // Block chrome draws command text through GTK labels, so it needs the same
    // icon fallback the VTE surfaces get; otherwise a prompt's Nerd-Font glyph
    // lands on whichever font fontconfig happens to sort first. The stack also
    // escapes each family, so a quote/backslash in a font name can't break the
    // surrounding CSS string and silently disable the whole stylesheet.
    let font_stack = crate::font::css_font_stack(&font_family, crate::font::icon_family(config));

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
        .block-failure-markers {{
            color: {err_hex};
        }}
        .block-list {{
            background-color: {bg_hex};
        }}
        .block-onboarding {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.055);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.12);
            border-radius: 10px;
            padding: 10px 14px;
        }}
        .block-onboarding-title {{
            color: {fg_hex};
            font-family: {font_stack};
            font-size: 0.92em;
            font-weight: bold;
        }}
        .block-onboarding-body {{
            color: {dim_fg};
            font-family: {font_stack};
            font-size: 0.78em;
        }}
        .notice-dock {{
            background-color: {bg_hex};
            border-top: 1px solid rgba({fg_r},{fg_g},{fg_b},0.14);
        }}
        /* The status stripe and the hairline ring are deliberately drawn by two
           DIFFERENT CSS boxes. GTK's cairo renderer falls back to one
           `cairo_pattern_create_mesh` per rounded corner as soon as a rounded
           border has two different colours on adjacent sides (gtkrenderborder.c
           `render_frame_fill` only takes its single-fill path when all four
           colours are equal). A card with a 1px neutral border plus a 3px
           coloured left border hit that path on every repaint, and with the
           whole viewport repainting per frame while a command streams it cost
           roughly half of the streaming wall clock (measured on anvil, same
           chrome: `seq 1 200000` drain 200ms -> 99ms, converged 366ms -> 166ms).

           So the border keeps ONE colour and only the left side has width — the
           stripe — and the ring moves to `outline`, which is its own
           single-colour rounded ring. `outline` does not take part in layout,
           so the 1px the other three border sides used to reserve comes back as
           padding. */
        .block-finished {{
            border-style: solid;
            border-width: 0 0 0 3px;
            border-color: transparent;
            outline-style: solid;
            outline-width: 1px;
            outline-color: rgba({fg_r},{fg_g},{fg_b},0.08);
            outline-offset: -1px;
            padding: 1px 1px 1px 0;
            border-radius: 10px;
            background-color: {block_bg_hex};
            min-height: 40px;
            transition: background-color 140ms ease, border-color 140ms ease,
                        outline-color 140ms ease, box-shadow 140ms ease;
        }}
        .block-success {{
            border-color: {ok_stripe};
        }}
        /* Outcome, hover, selection and bookmark are four INDEPENDENT states a
           single card can hold at once, so they must not all express themselves
           through `box-shadow` and `background-color`: five single-class rules
           declaring the same property means the last one in this file wins and
           the other three states silently vanish. Hovering a failed card used
           to swap its red wash for the neutral hover wash — the card stopped
           looking failed exactly while the pointer was on it.

           The division of labour now: outcome owns the border stripe and the
           background COLOUR, bookmark owns a background IMAGE bar (a separate
           property, so it can never erase a ring), hover and selection share
           `box-shadow`, and every combination that a user can actually produce
           gets an explicit compound rule below. */
        .block-failed {{
            border-color: {err_stripe};
            background-color: rgba({err_r},{err_g},{err_b},0.11);
        }}
        .block-unknown {{
            border-color: {warn_stripe};
        }}
        /* Stopped, not broken. A muted stripe and no background wash: these
           cards must recede so the genuinely failed ones stand out. */
        .block-interrupted {{
            border-color: rgba({fg_r},{fg_g},{fg_b},0.35);
        }}
        .block-interrupted.block-hovered {{
            outline-color: rgba({fg_r},{fg_g},{fg_b},0.22);
        }}
        .block-hovered {{
            background-color: rgba({fg_r},{fg_g},{fg_b},0.05);
            /* The ring, not the stripe: recolouring three of four border sides
               is exactly the mixed-colour case the mesh fallback exists for. */
            outline-color: rgba({fg_r},{fg_g},{fg_b},0.16);
            box-shadow: 0 4px 14px rgba(0,0,0,0.22);
        }}
        /* Pointing at a failed card deepens its red instead of neutralising it. */
        .block-failed.block-hovered {{
            background-color: rgba({err_r},{err_g},{err_b},0.17);
            outline-color: rgba({err_r},{err_g},{err_b},0.34);
        }}
        .block-unknown.block-hovered {{
            outline-color: rgba({warn_r},{warn_g},{warn_b},0.30);
        }}
        /* The inset rings are anchored to the PADDING box, which moved one pixel
           outward when the top/right/bottom border became a layout-free
           `outline`. Widen each by that pixel so a selected card keeps the same
           2px band, and the active end its 3px one. */
        .block-selected {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.08);
            outline-color: rgba({acc_r},{acc_g},{acc_b},0.48);
            box-shadow: inset 0 0 0 2px rgba({acc_r},{acc_g},{acc_b},0.65);
        }}
        .block-selected.block-selection-active {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.14);
            outline-color: rgba({acc_r},{acc_g},{acc_b},0.92);
            box-shadow: inset 0 0 0 3px {accent}, 0 0 0 1px rgba({acc_r},{acc_g},{acc_b},0.55);
        }}
        /* A selected card under the pointer keeps BOTH its ring and the hover
           lift; `box-shadow` is one property, so the combination has to be
           written out rather than inherited from either rule alone. */
        .block-selected.block-hovered {{
            box-shadow: inset 0 0 0 2px rgba({acc_r},{acc_g},{acc_b},0.65),
                        0 4px 14px rgba(0,0,0,0.22);
        }}
        .block-selected.block-selection-active.block-hovered {{
            box-shadow: inset 0 0 0 3px {accent},
                        0 0 0 1px rgba({acc_r},{acc_g},{acc_b},0.55),
                        0 4px 14px rgba(0,0,0,0.22);
        }}
        /* Selection contributes an accent ring, but a failed card still owns
           its red outcome wash. These compound rules keep that wash through
           both the active selection endpoint and pointer hover. */
        .block-failed.block-selected {{
            background-color: rgba({err_r},{err_g},{err_b},0.11);
        }}
        .block-failed.block-selected.block-hovered {{
            background-color: rgba({err_r},{err_g},{err_b},0.17);
        }}
        .block-failed.block-selected.block-selection-active {{
            background-color: rgba({err_r},{err_g},{err_b},0.15);
        }}
        .block-failed.block-selected.block-selection-active.block-hovered {{
            background-color: rgba({err_r},{err_g},{err_b},0.20);
        }}
        /* Same split as `.block-finished`: one border colour so GTK never builds
           a corner mesh, ring on `outline`, and the layout the removed border
           sides used to contribute given back as padding. This is the card that
           repaints on every frame of a stream, so it is the one that pays. */
        .block-active {{
            border-style: solid;
            border-width: 0 0 0 3px;
            border-color: rgba({acc_r},{acc_g},{acc_b},0.85);
            outline-style: solid;
            outline-width: 1px;
            outline-color: rgba({acc_r},{acc_g},{acc_b},0.32);
            outline-offset: -1px;
            border-radius: 10px;
            margin: 4px 8px;
            padding: 3px 1px 3px 0;
            background-color: {bg_hex};
            box-shadow: 0 2px 8px rgba(0,0,0,0.18);
        }}
        .block-finished.block-background {{
            border-color: rgba({acc_r},{acc_g},{acc_b},0.72);
        }}
        .block-background-chip {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.10);
            border-radius: 999px;
            padding: 1px 7px;
            font-size: 0.88em;
        }}
        .block-status-background {{
            color: {accent};
        }}
        .block-status-interrupted {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.14);
            border-radius: 999px;
            min-width: 16px;
            min-height: 16px;
            padding: 1px 5px;
            font-family: {font_stack};
            font-size: 0.82em;
            font-weight: bold;
        }}
        .block-status-unknown {{
            color: {warn_hex};
            background-color: rgba({warn_r},{warn_g},{warn_b},0.18);
            border-radius: 999px;
            min-width: 16px;
            min-height: 16px;
            padding: 1px 5px;
            font-family: {font_stack};
            font-size: 0.82em;
            font-weight: bold;
        }}
        .block-exit-interrupted {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.10);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.22);
            border-radius: 999px;
            font-family: {font_stack};
            font-size: 0.78em;
            padding: 1px 8px;
        }}
        .block-exit-unknown {{
            color: {warn_hex};
            background-color: rgba({warn_r},{warn_g},{warn_b},0.18);
            border: 1px solid rgba({warn_r},{warn_g},{warn_b},0.35);
            border-radius: 999px;
            font-family: {font_stack};
            font-size: 0.78em;
            font-weight: bold;
            padding: 1px 8px;
        }}
        .block-correction, .command-suggestion, .command-review-standalone {{
            border-color: rgba({acc_r},{acc_g},{acc_b},0.85);
            background-color: rgba({acc_r},{acc_g},{acc_b},0.05);
        }}
        .block-integration-notice {{
            border-color: rgba({warn_r},{warn_g},{warn_b},0.85);
            background-color: rgba({warn_r},{warn_g},{warn_b},0.06);
        }}
        .integration-notice-code {{
            color: {accent};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.07);
            border-radius: 6px;
            font-family: {font_stack};
            font-size: 0.92em;
            padding: 6px 10px;
        }}
        .block-agent {{
            border-color: rgba({agent_r},{agent_g},{agent_b},0.85);
            background-color: rgba({agent_r},{agent_g},{agent_b},0.05);
        }}
        .block-organism {{
            border-color: rgba({agent_r},{agent_g},{agent_b},0.70);
            background-color: rgba({agent_r},{agent_g},{agent_b},0.035);
        }}
        .block-organism.organism-active {{
            border-color: rgba({acc_r},{acc_g},{acc_b},0.90);
            background-color: rgba({acc_r},{acc_g},{acc_b},0.07);
        }}
        .block-organism.organism-success {{
            border-color: {ok_stripe};
            background-color: rgba({ok_r},{ok_g},{ok_b},0.08);
        }}
        .block-organism.organism-error {{
            border-color: {err_stripe};
            background-color: rgba({err_r},{err_g},{err_b},0.10);
        }}
        .block-organism.organism-warning {{
            border-color: {warn_stripe};
            background-color: rgba({warn_r},{warn_g},{warn_b},0.07);
        }}
        .organism-sprite {{
            color: {agent_hex};
            font-family: {font_stack};
            font-weight: bold;
        }}
        .organism-live-body {{
            color: {agent_hex};
            background-color: rgba({bg_r},{bg_g},{bg_b},0.80);
            border: 1px solid rgba({agent_r},{agent_g},{agent_b},0.32);
            border-radius: 6px;
            padding: 3px 6px;
            font-family: {font_stack};
            font-size: {font_size};
            font-weight: bold;
        }}
        .organism-live-body.organism-active {{
            color: {accent};
            border-color: rgba({acc_r},{acc_g},{acc_b},0.50);
        }}
        .organism-live-body.organism-success {{
            color: {ok_hex};
            border-color: rgba({ok_r},{ok_g},{ok_b},0.50);
        }}
        .organism-live-body.organism-error {{
            color: {err_hex};
            border-color: rgba({err_r},{err_g},{err_b},0.55);
        }}
        .organism-live-body.organism-warning {{
            color: {warn_hex};
            border-color: rgba({warn_r},{warn_g},{warn_b},0.50);
        }}
        .organism-sticky-avatar {{
            color: {agent_hex};
            font-family: {font_stack};
            font-weight: bold;
            margin-right: 6px;
        }}
        .organism-sticky-avatar.organism-error {{ color: {err_hex}; }}
        .organism-sticky-avatar.organism-success {{ color: {ok_hex}; }}
        .organism-sticky-avatar.organism-warning {{ color: {warn_hex}; }}
        .organism-title {{
            color: {fg_hex};
            font-weight: bold;
        }}
        .organism-badge, .organism-state {{
            color: {dim_fg};
            font-family: {font_stack};
            font-size: 0.82em;
        }}
        .organism-status {{
            color: {fg_hex};
        }}
        .organism-error .organism-status {{
            color: {err_hex};
        }}
        .organism-success .organism-status {{
            color: {ok_hex};
        }}
        .agent-card-icon {{
            color: {agent_hex};
            font-family: {font_stack};
        }}
        .agent-card-title {{
            color: {fg_hex};
            font-weight: bold;
        }}
        .agent-card-binding {{
            color: {dim_fg};
            font-size: 0.85em;
        }}
        .agent-prompt-status {{
            border-radius: 999px;
            padding: 2px 7px;
            font-size: 0.82em;
        }}
        .agent-prompt-status.agent-prompt-ready {{
            color: {ok_hex};
            background-color: rgba({ok_r},{ok_g},{ok_b},0.10);
        }}
        .agent-prompt-status.agent-prompt-blocked {{
            color: {err_hex};
            background-color: rgba({err_r},{err_g},{err_b},0.10);
        }}
        .assistant-card-icon {{
            color: {accent};
            font-family: {font_stack};
        }}
        .assistant-card-title {{
            color: {fg_hex};
            font-weight: bold;
        }}
        .assistant-card-badge {{
            color: {dim_fg};
            font-size: 0.85em;
        }}
        .assistant-context-chip {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.055);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.12);
            border-radius: 8px;
            padding: 5px 8px;
            font-size: 0.88em;
        }}
        .assistant-status-row {{
            padding: 5px 0;
        }}
        .assistant-status {{
            color: {dim_fg};
        }}
        .command-review-embedded {{
            background-color: rgba({fg_r},{fg_g},{fg_b},0.045);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.34);
            border-radius: 9px;
        }}
        .command-review-description {{
            color: {fg_hex};
        }}
        .command-review-risk {{
            color: {dim_fg};
            font-size: 0.9em;
        }}
        .command-review-risk.error, .command-review-feedback.error {{
            color: {err_hex};
        }}
        .command-review-feedback {{
            color: {dim_fg};
            font-size: 0.9em;
        }}
        .command-review-entry {{
            font-family: {font_stack};
            font-size: {font_size};
            background-color: {bg_hex};
            color: {fg_hex};
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.18);
            border-radius: 6px;
            padding: 4px 8px;
        }}
        .command-review-entry:focus {{
            border-color: rgba({acc_r},{acc_g},{acc_b},0.75);
        }}
        .command-review-actions {{
            margin-top: 2px;
        }}
        .agent-msg-body {{
            font-family: {font_stack};
            font-size: {font_size};
            color: {fg_hex};
        }}
        .agent-msg-error {{
            color: {err_hex};
        }}
        .correction-icon {{
            color: {accent};
            font-family: {font_stack};
        }}
        .correction-title {{
            color: {fg_hex};
            font-weight: bold;
        }}
        .correction-evidence {{
            color: {dim_fg};
            font-size: 0.85em;
        }}
        .correction-message {{
            color: {fg_hex};
        }}
        .correction-warning {{
            color: {err_hex};
        }}
        .correction-error {{
            color: {err_hex};
            font-size: 0.9em;
        }}
        .correction-entry {{
            font-family: {font_stack};
            font-size: {font_size};
            background-color: {bg_hex};
            color: {fg_hex};
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.18);
            border-radius: 6px;
            padding: 4px 8px;
        }}
        .correction-entry:focus {{
            border-color: rgba({acc_r},{acc_g},{acc_b},0.75);
        }}
        .correction-run {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.18);
            color: {ok_hex};
            border-radius: 6px;
        }}
        .correction-run:hover {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.30);
        }}
        .block-finished.block-compact, .block-active.block-compact {{
            border-radius: 6px;
            /* Keep the pixel the layout-free `outline` no longer contributes. */
            padding: 1px 1px 1px 0;
            box-shadow: none;
        }}
        .block-finished.block-compact {{
            min-height: 32px;
        }}
        .block-active.block-compact {{
            margin: 1px 4px;
        }}
        .block-active.block-fullscreen {{
            border: none;
            outline-style: none;
            border-radius: 0;
            margin: 0;
            padding: 0;
            box-shadow: none;
        }}
        .block-output-scrollbar {{
            min-width: 10px;
            margin: 1px 3px 1px 1px;
            padding: 0;
            background-color: transparent;
        }}
        .block-output-scrollbar trough {{
            min-width: 8px;
            border-radius: 4px;
            background-color: rgba({fg_r},{fg_g},{fg_b},0.06);
        }}
        .block-output-scrollbar slider {{
            min-width: 6px;
            border-radius: 4px;
            background-color: rgba({fg_r},{fg_g},{fg_b},0.38);
        }}
        .block-output-scrollbar slider:hover {{
            background-color: rgba({acc_r},{acc_g},{acc_b},0.78);
        }}
        .block-prompt-chevron {{
            color: {accent};
            font-family: {font_stack};
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
            font-family: {font_stack};
            font-size: 0.78em;
            padding: 1px 9px;
        }}
        .block-bookmark-star {{
            color: {warn_hex};
            font-family: {font_stack};
            font-size: 0.82em;
            margin-right: 2px;
        }}
        /* The bookmark bar rides `background-image`, not `box-shadow`. As a
           box-shadow it was the last single-class rule in the file, so
           bookmarking a card erased its selection ring and its hover lift.
           A gradient stop is a channel nothing else in this stylesheet uses,
           and it composites over whichever background-colour the card's
           outcome or selection state chose. */
        .block-bookmarked {{
            background-image: linear-gradient(
                to right,
                {warn_hex} 0px,
                {warn_hex} 3px,
                transparent 3px
            );
        }}
        .block-chip-git {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.10);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.22);
            border-radius: 999px;
            font-family: {font_stack};
            font-size: 0.78em;
            padding: 1px 9px;
        }}
        .block-lifecycle-chip {{
            color: {warn_hex};
            background-color: rgba({warn_r},{warn_g},{warn_b},0.12);
            border: 1px solid rgba({warn_r},{warn_g},{warn_b},0.35);
            border-radius: 999px;
            font-family: {font_stack};
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
            font-family: {font_stack};
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
            font-family: {font_stack};
            font-size: 0.82em;
            font-weight: bold;
        }}
        .block-action-btn {{
            color: {dim_fg};
            min-width: 24px;
            min-height: 24px;
            padding: 0 4px;
            border-radius: 999px;
            font-family: {font_stack};
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
            font-family: {font_stack};
            font-size: 0.8em;
        }}
        .block-filter-toggle:checked {{
            color: {fg_hex};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.35);
        }}
        .block-filter-status {{
            color: {dim_fg};
            font-family: {font_stack};
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
        .block-selection-hint {{
            color: {accent};
            font-family: {font_stack};
            font-size: 0.76em;
            padding: 0 4px;
        }}
        .block-collapse-btn {{
            color: {dim_fg};
            font-family: {font_stack};
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
        .block-output-summary {{
            color: {dim_fg};
            font-family: {font_stack};
            font-size: 0.82em;
            padding: 2px 4px;
            border-radius: 5px;
        }}
        .block-output-summary:hover {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.12);
        }}
        .block-prompt {{
            color: {dim_fg};
            font-family: {font_stack};
            font-size: {font_size};
            line-height: 1.0;
            margin: 0;
        }}
        .block-cmd {{
            color: {fg_hex};
            font-family: {font_stack};
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
        }}
        .block-cmd-active {{
            color: {fg_hex};
            font-family: {font_stack};
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
        .block-cmd-active text selection {{
            background-color: transparent;
        }}
        .block-cmd-finished {{
            color: {fg_hex};
            font-family: {font_stack};
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
            font-family: {font_stack};
            font-size: 0.78em;
            font-weight: bold;
            padding: 1px 8px;
        }}
        .block-meta-badge {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.08);
            border-radius: 999px;
            font-family: {font_stack};
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
            font-family: {font_stack};
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
            font-family: {font_stack};
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
            font-family: {font_stack};
            font-size: 0.92em;
            font-weight: bold;
        }}
        .sticky-header-control {{
            min-width: 24px;
            min-height: 22px;
            padding: 0 5px;
            border-radius: 5px;
            color: {dim_fg};
        }}
        .sticky-header-control:hover {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.12);
        }}
        .sticky-running-header.sticky-minimized {{
            padding-left: 4px;
        }}
        .feed-hold-badge {{
            color: {bg_hex};
            background-color: {accent};
            background-image: none;
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.55);
            border-radius: 999px;
            font-family: {font_stack};
            font-size: 0.85em;
            font-weight: bold;
            padding: 4px 12px;
            box-shadow: 0 4px 14px rgba(0,0,0,0.35);
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
            font-family: {font_stack};
            font-size: 0.92em;
            padding: 6px 10px;
        }}
        "#,
    );
    css
}

#[cfg(test)]
mod tests {
    use super::{
        block_css, git_branch_for_at, git_branch_uncached, shorten_path_with_home,
        GIT_BRANCH_CACHE, GIT_BRANCH_CACHE_ENTRIES, GIT_BRANCH_WALKS, GIT_NEGATIVE_CACHE_TTL,
        MAX_GIT_POINTER_BYTES,
    };
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Collect the declarations of one rule, keyed by its exact selector text.
    fn rule_body<'a>(css: &'a str, selector: &str) -> &'a str {
        let needle = format!("\n        {selector} {{\n");
        let start = css
            .find(&needle)
            .unwrap_or_else(|| panic!("stylesheet has no `{selector}` rule"))
            + needle.len();
        let end = start
            + css[start..]
                .find("\n        }")
                .unwrap_or_else(|| panic!("`{selector}` rule is unterminated"));
        &css[start..end]
    }

    fn selectors_declaring<'a>(css: &'a str, property: &str) -> Vec<&'a str> {
        let mut out = Vec::new();
        let mut rest = css;
        while let Some(open) = rest.find(" {\n") {
            let selector = rest[..open].rsplit('\n').next().unwrap_or("").trim();
            let body_start = open + " {\n".len();
            let Some(close) = rest[body_start..].find("\n        }") else {
                break;
            };
            let body = &rest[body_start..body_start + close];
            if body
                .lines()
                .any(|line| line.trim_start().starts_with(&format!("{property}:")))
            {
                out.push(selector);
            }
            rest = &rest[body_start + close + 1..];
        }
        out
    }

    /// A stylesheet GTK cannot parse fails silently: the provider keeps the
    /// rules before the error and drops the rest, so a typo in a card rule
    /// shows up as missing chrome rather than as an error.
    #[test]
    #[ignore = "requires DISPLAY"]
    fn the_generated_stylesheet_parses_without_error() {
        use std::cell::RefCell as StdRefCell;
        use std::rc::Rc;

        gtk4::init().expect("gtk init");
        let config = crate::config::Config::safe_defaults();
        let errors: Rc<StdRefCell<Vec<String>>> = Rc::new(StdRefCell::new(Vec::new()));
        let provider = gtk4::CssProvider::new();
        {
            let errors = errors.clone();
            provider.connect_parsing_error(move |_, section, error| {
                errors
                    .borrow_mut()
                    .push(format!("{}: {error}", section.to_str()));
            });
        }
        provider.load_from_string(&block_css(&config));
        let errors = errors.borrow();
        assert!(errors.is_empty(), "stylesheet parse errors: {errors:?}");
    }

    /// Outcome, hover, selection and bookmark are four independent states one
    /// card can hold simultaneously. When they all expressed themselves through
    /// `box-shadow`, the last single-class rule in the file won and the others
    /// vanished: hovering a failed card removed its red, and bookmarking a
    /// selected card removed its ring. Every combination a user can produce
    /// must have a rule that says what it looks like.
    #[test]
    fn independent_card_states_do_not_overwrite_each_other() {
        let config = crate::config::Config::safe_defaults();
        let css = block_css(&config);

        // The bookmark bar left `box-shadow` entirely.
        let bookmarked = rule_body(&css, ".block-bookmarked");
        assert!(
            !bookmarked.contains("box-shadow"),
            "the bookmark bar must not compete for box-shadow: {bookmarked}"
        );
        assert!(bookmarked.contains("background-image"));

        // The failure wash survives hover, and reads stronger rather than
        // being replaced by the neutral hover wash.
        let failed = rule_body(&css, ".block-failed");
        let failed_hovered = rule_body(&css, ".block-failed.block-hovered");
        assert!(failed.contains("background-color"));
        assert!(
            failed_hovered.contains("background-color"),
            "a hovered failed card must restate its own wash: {failed_hovered}"
        );
        assert!(
            !failed.contains("box-shadow"),
            "the failure stripe is the border, not a second inset shadow"
        );

        // Selection is an outline/ring, never a replacement for an outcome
        // stripe. The failure wash is explicitly retained for every
        // selection/hover combination that can occur in the card widget.
        for selector in [".block-selected", ".block-selected.block-selection-active"] {
            let body = rule_body(&css, selector);
            assert!(
                !body.contains("border-color"),
                "`{selector}` must not overwrite the outcome stripe: {body}"
            );
        }
        for selector in [
            ".block-failed.block-selected",
            ".block-failed.block-selected.block-hovered",
            ".block-failed.block-selected.block-selection-active",
            ".block-failed.block-selected.block-selection-active.block-hovered",
        ] {
            let body = rule_body(&css, selector);
            assert!(
                body.contains("background-color"),
                "`{selector}` must retain the failure wash: {body}"
            );
        }

        // A hovered selection keeps both the ring and the lift.
        for selector in [
            ".block-selected.block-hovered",
            ".block-selected.block-selection-active.block-hovered",
        ] {
            let body = rule_body(&css, selector);
            assert!(
                body.contains("inset") && body.contains("14px"),
                "`{selector}` must carry the ring AND the hover elevation: {body}"
            );
        }

        // Nothing else may quietly join the box-shadow contest. Scoped to the
        // finished card: `.block-active` is the live surface, a different
        // widget that never carries hover/selection/bookmark classes.
        let mut owners = selectors_declaring(&css, "box-shadow");
        owners.retain(|selector| {
            selector.starts_with(".block-") && !selector.contains(".block-active")
        });
        owners.sort_unstable();
        assert_eq!(
            owners,
            vec![
                ".block-hovered",
                ".block-selected",
                ".block-selected.block-hovered",
                ".block-selected.block-selection-active",
                ".block-selected.block-selection-active.block-hovered",
            ],
            "a new card state must declare a compound rule, not a bare box-shadow"
        );
    }

    fn test_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "forge-css-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn shorten_path_uses_component_aware_home_prefix() {
        let home = Path::new("/home/alice");
        assert_eq!(shorten_path_with_home("/home/alice", Some(home)), "~");
        assert_eq!(
            shorten_path_with_home("/home/alice/projects/demo", Some(home)),
            "~/projects/demo"
        );
        assert_eq!(
            shorten_path_with_home("/home/alice2/project", Some(home)),
            "/home/alice2/project"
        );
    }

    #[test]
    fn shorten_path_keeps_only_the_last_two_components_when_long() {
        assert_eq!(
            shorten_path_with_home(
                "/home/alice/workspace/team/project",
                Some(Path::new("/home/alice")),
            ),
            "…/team/project"
        );
    }

    #[test]
    fn git_branch_is_bounded_and_makes_hidden_text_visible() {
        let root = test_root("branch");
        let git = root.join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(
            git.join("HEAD"),
            "ref: refs/heads/safe\u{202e}\u{fe0f}name\n",
        )
        .unwrap();
        assert_eq!(
            git_branch_uncached(root.to_str().unwrap()).as_deref(),
            Some("safe��name")
        );

        std::fs::write(
            git.join("HEAD"),
            format!("ref: refs/heads/{}\n", "x".repeat(300)),
        )
        .unwrap();
        let branch = git_branch_uncached(root.to_str().unwrap()).unwrap();
        assert_eq!(branch.chars().count(), 257);
        assert!(branch.ends_with('…'));

        std::fs::write(git.join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(
            git_branch_uncached(root.to_str().unwrap()).as_deref(),
            Some("0123456")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cached_head_locator_observes_branch_switch_without_another_walk() {
        let root = test_root("fresh-head");
        let repo = root.join("repo");
        let git = repo.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let head = git.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/first\n").unwrap();
        let cwd = repo.to_str().unwrap();
        GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().clear());
        GIT_BRANCH_WALKS.with(|walks| walks.set(0));
        let now = Instant::now();

        assert_eq!(git_branch_for_at(cwd, now).as_deref(), Some("first"));
        std::fs::write(&head, "ref: refs/heads/second\n").unwrap();
        for _ in 0..200 {
            assert_eq!(git_branch_for_at(cwd, now).as_deref(), Some("second"));
        }
        assert_eq!(GIT_BRANCH_WALKS.with(std::cell::Cell::get), 1);

        GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().clear());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_worktree_locator_is_evicted_and_resolved_once() {
        let root = test_root("worktree-repoint");
        let worktree = root.join("worktree");
        let first_git = root.join("first-git");
        let second_git = root.join("second-git");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&first_git).unwrap();
        std::fs::create_dir_all(&second_git).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../first-git\n").unwrap();
        std::fs::write(first_git.join("HEAD"), "ref: refs/heads/first\n").unwrap();
        std::fs::write(second_git.join("HEAD"), "ref: refs/heads/second\n").unwrap();
        let cwd = worktree.to_str().unwrap();
        GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().clear());
        GIT_BRANCH_WALKS.with(|walks| walks.set(0));
        let now = Instant::now();

        assert_eq!(git_branch_for_at(cwd, now).as_deref(), Some("first"));
        std::fs::write(worktree.join(".git"), "gitdir: ../second-git\n").unwrap();
        std::fs::remove_file(first_git.join("HEAD")).unwrap();
        assert_eq!(git_branch_for_at(cwd, now).as_deref(), Some("second"));
        assert_eq!(GIT_BRANCH_WALKS.with(std::cell::Cell::get), 2);

        GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().clear());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn negative_locator_cache_hits_then_expires_without_sleeping() {
        // `/proc/self` has no repository marker in its ancestor chain. Avoid a
        // temp directory here: another test process may intentionally create
        // `/tmp/.git` while probing hostile metadata.
        let cwd = "/proc/self";
        GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().clear());
        GIT_BRANCH_WALKS.with(|walks| walks.set(0));
        let now = Instant::now();

        assert_eq!(git_branch_for_at(cwd, now), None);
        assert_eq!(
            git_branch_for_at(cwd, now + Duration::from_millis(199)),
            None
        );
        assert_eq!(GIT_BRANCH_WALKS.with(std::cell::Cell::get), 1);

        assert_eq!(git_branch_for_at(cwd, now + GIT_NEGATIVE_CACHE_TTL), None);
        assert_eq!(GIT_BRANCH_WALKS.with(std::cell::Cell::get), 2);

        GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().clear());
    }

    #[test]
    fn git_locator_cache_is_bounded_to_64_working_directories() {
        let root = test_root("locator-capacity");
        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().clear());
        let now = Instant::now();
        let mut cwds = Vec::new();

        for index in 0..=GIT_BRANCH_CACHE_ENTRIES {
            let cwd = root.join(format!("dir-{index}"));
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_string_lossy().into_owned();
            assert_eq!(git_branch_for_at(&cwd, now).as_deref(), Some("main"));
            cwds.push(cwd);
        }

        GIT_BRANCH_CACHE.with(|cache| {
            let cache = cache.borrow();
            assert_eq!(cache.entries.len(), GIT_BRANCH_CACHE_ENTRIES);
            assert!(!cache.entries.iter().any(|(cwd, _)| cwd == &cwds[0]));
            assert!(cache.entries.iter().any(|(cwd, _)| cwd == &cwds[1]));
        });

        GIT_BRANCH_CACHE.with(|cache| cache.borrow_mut().clear());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_metadata_reader_rejects_oversized_and_linked_files() {
        let root = test_root("metadata");
        let git = root.join(".git");
        std::fs::create_dir(&git).unwrap();
        let head = git.join("HEAD");
        std::fs::write(&head, vec![b'x'; MAX_GIT_POINTER_BYTES as usize + 1]).unwrap();
        assert_eq!(git_branch_uncached(root.to_str().unwrap()), None);

        #[cfg(unix)]
        {
            let target = root.join("target");
            std::fs::write(&target, "ref: refs/heads/linked\n").unwrap();
            std::fs::remove_file(&head).unwrap();
            std::os::unix::fs::symlink(&target, &head).unwrap();
            assert_eq!(git_branch_uncached(root.to_str().unwrap()), None);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn git_metadata_reader_rejects_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let root = test_root("fifo");
        let git = root.join(".git");
        std::fs::create_dir(&git).unwrap();
        let head = git.join("HEAD");
        let path = CString::new(head.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a live NUL-terminated pathname and mode is valid.
        assert_eq!(unsafe { nix::libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(git_branch_uncached(root.to_str().unwrap()), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(root);
    }
}
