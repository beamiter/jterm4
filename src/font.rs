//! Terminal font resolution — keeping icon glyphs on an icon font.
//!
//! A configured family such as `Monospace` carries no Nerd-Font glyph, so
//! every icon a shell prompt or a vim statusline draws misses the primary
//! font and lands in Pango's generic fallback. That fallback is fontconfig's
//! global sort, and it is won by whichever installed font claims the Private
//! Use Area codepoint — which is rarely the icon font. On a box carrying the
//! Arphic CJK fonts, `AR PL UMing HK` answers for U+E0A0 (powerline branch),
//! U+E5FA (Seti file icons) and U+E700 (devicons), so lualine paints a row of
//! unrelated ideographs where its icons belong.
//!
//! Pango walks every family in a description's comma-separated list before it
//! reaches that generic fallback, so naming an installed Nerd Font as a
//! secondary family repairs the icons while leaving the text font the user
//! chose in charge of every glyph it actually covers.

use crate::config::Config;
use gtk4::pango::FontDescription;
use gtk4::prelude::*;
use std::sync::OnceLock;

/// Substring every Nerd Font family name carries.
const NERD_FONT_MARKER: &str = "Nerd Font";

/// Rank `name` as an icon fallback; lower is better, `None` rejects it.
///
/// The `Mono` cuts hold every icon to a single cell, which is the only shape
/// that lines up in a terminal grid; the proportional cuts overhang their
/// neighbours. Nerd Fonts also ships icon-only `Symbols` cuts that exist to be
/// somebody else's fallback, so those win over borrowing icons out of a full
/// text font that may disagree about metrics.
fn icon_family_rank(name: &str) -> Option<u8> {
    let variant = if name.ends_with("Nerd Font Mono") {
        0
    } else if name.ends_with("Nerd Font Propo") {
        4
    } else if name.ends_with(NERD_FONT_MARKER) {
        2
    } else {
        return None;
    };
    Some(variant + u8::from(!name.starts_with("Symbols")))
}

/// Best icon fallback among `families`, or `None` when none is installed.
///
/// Ties break on the family name so the choice cannot drift between runs with
/// the order fontconfig happens to enumerate in.
pub(crate) fn select_icon_family<I>(families: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    families
        .into_iter()
        .filter_map(|name| icon_family_rank(&name).map(|rank| (rank, name)))
        .min()
        .map(|(_, name)| name)
}

/// The icon family this process falls back to, resolved once.
///
/// Enumerating families walks every font file fontconfig knows about, and the
/// answer cannot change while the process runs.
fn detected_icon_family() -> Option<&'static str> {
    static DETECTED: OnceLock<Option<String>> = OnceLock::new();
    if let Some(detected) = DETECTED.get() {
        return detected.as_deref();
    }
    // Listing families needs a font map, which needs GTK up on this thread —
    // and unit tests reach the CSS builder with no toolkit at all. Answer
    // `None` there without memoizing it, so a verdict taken before GTK existed
    // cannot outlive the moment.
    if !gtk4::is_initialized_main_thread() {
        return None;
    }
    // Any widget hands out a context bound to the display's font map; an
    // unparented label needs neither a window nor realization.
    let context = gtk4::Label::new(None).pango_context();
    let detected = select_icon_family(
        context
            .list_families()
            .iter()
            .map(|family| family.name().to_string()),
    );
    DETECTED.get_or_init(|| detected).as_deref()
}

/// The icon family to append for `config`: the configured override, the
/// detected default, or `None` when the user turned the fallback off.
pub(crate) fn icon_family(config: &Config) -> Option<&str> {
    match config.icon_font.as_deref() {
        Some(name) if name.is_empty() || name.eq_ignore_ascii_case("none") => None,
        Some(name) => Some(name),
        None => detected_icon_family(),
    }
}

/// Extend a Pango family list with `icon_family`, or `None` to leave it alone.
///
/// A description that already names an icon font — or names nothing, and so
/// asks for the display default — is returned untouched: appending there would
/// change which font draws ordinary text, not just the icons.
fn family_list_with_icon(families: &str, icon_family: &str) -> Option<String> {
    if families.trim().is_empty() {
        return None;
    }
    let already_covered = families.split(',').any(|family| {
        let family = family.trim();
        family.contains(NERD_FONT_MARKER) || family.eq_ignore_ascii_case(icon_family)
    });
    (!already_covered).then(|| format!("{families}, {icon_family}"))
}

/// Parse `desc` and append `icon_family` behind the families it names.
///
/// Everything else in the description — size, weight, style — survives, so the
/// user's font string stays the one thing that decides how text looks.
pub(crate) fn font_description(desc: &str, icon_family: Option<&str>) -> FontDescription {
    let mut font = FontDescription::from_string(desc);
    let Some(icon_family) = icon_family else {
        return font;
    };
    let families = font.family().unwrap_or_default();
    if let Some(list) = family_list_with_icon(&families, icon_family) {
        font.set_family(&list);
    }
    font
}

/// The description every VTE surface should be given for `desc` under `config`.
pub(crate) fn terminal_font_description(desc: &str, config: &Config) -> FontDescription {
    font_description(desc, icon_family(config))
}

/// The CSS `font-family` value for `family`, with the icon fallback behind it.
///
/// Block chrome draws command text through GTK labels rather than the VTE, so
/// it needs the same fallback list spelled the way CSS wants it.
pub(crate) fn css_font_stack(family: &str, icon_family: Option<&str>) -> String {
    let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let primary = format!("\"{}\"", escape(family));
    match icon_family {
        Some(icon) if family_list_with_icon(family, icon).is_some() => {
            format!("{primary}, \"{}\"", escape(icon))
        }
        _ => primary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_cuts_outrank_proportional_and_symbols_outrank_text_fonts() {
        assert!(
            icon_family_rank("Symbols Nerd Font Mono") < icon_family_rank("Hack Nerd Font Mono")
        );
        assert!(icon_family_rank("Hack Nerd Font Mono") < icon_family_rank("Symbols Nerd Font"));
        assert!(icon_family_rank("Hack Nerd Font") < icon_family_rank("Hack Nerd Font Propo"));
        assert_eq!(icon_family_rank("DejaVu Sans Mono"), None);
        assert_eq!(icon_family_rank("AR PL UMing HK"), None);
    }

    #[test]
    fn selection_prefers_the_mono_cut_and_breaks_ties_by_name() {
        let installed = [
            "AR PL UMing HK",
            "JetBrainsMono Nerd Font Propo",
            "SauceCodePro Nerd Font",
            "JetBrainsMono Nerd Font Mono",
            "DejaVuSansM Nerd Font Mono",
        ]
        .map(str::to_string);
        assert_eq!(
            select_icon_family(installed),
            Some("DejaVuSansM Nerd Font Mono".to_string())
        );
    }

    #[test]
    fn selection_is_none_without_an_icon_font() {
        let installed = ["DejaVu Sans Mono", "Arial"].map(str::to_string);
        assert_eq!(select_icon_family(installed), None);
    }

    #[test]
    fn a_font_that_already_draws_icons_is_left_alone() {
        assert_eq!(
            family_list_with_icon("JetBrainsMono Nerd Font", "Symbols Nerd Font Mono"),
            None
        );
        assert_eq!(family_list_with_icon("", "Symbols Nerd Font Mono"), None);
        assert_eq!(
            family_list_with_icon("Symbols Nerd Font Mono", "Symbols Nerd Font Mono"),
            None
        );
    }

    #[test]
    fn the_icon_family_lands_behind_the_configured_one() {
        assert_eq!(
            family_list_with_icon("Monospace", "Symbols Nerd Font Mono"),
            Some("Monospace, Symbols Nerd Font Mono".to_string())
        );
    }

    #[test]
    fn size_and_style_survive_the_rewrite() {
        let font = font_description("Monospace Bold Italic 14", Some("Symbols Nerd Font Mono"));
        assert_eq!(
            font.family().map(|f| f.to_string()).unwrap_or_default(),
            "Monospace, Symbols Nerd Font Mono"
        );
        assert_eq!(font.size() / gtk4::pango::SCALE, 14);
        assert_eq!(font.weight(), gtk4::pango::Weight::Bold);
        assert_eq!(font.style(), gtk4::pango::Style::Italic);
    }

    #[test]
    fn no_icon_family_leaves_the_description_untouched() {
        let font = font_description("Monospace 14", None);
        assert_eq!(
            font.family().map(|f| f.to_string()).unwrap_or_default(),
            "Monospace"
        );
    }

    /// The wiring, not just the arithmetic: a live VTE built from a config
    /// whose font covers no icons must be handed the fallback family too.
    /// Needs a display, so it runs from `make test-display`.
    #[test]
    #[ignore = "requires a GTK display"]
    fn a_real_vte_is_given_the_icon_family_behind_the_configured_one() {
        use vte4::TerminalExt;

        gtk4::init().expect("GTK display");
        let config = Config::safe_defaults();
        let configured = FontDescription::from_string(&config.font_desc)
            .family()
            .map(|family| family.to_string())
            .expect("the default font names a family");
        let terminal = crate::terminal::create_terminal(&config);
        let families = terminal
            .font()
            .and_then(|font| font.family())
            .map(|family| family.to_string())
            .expect("the terminal carries a font");

        match icon_family(&config) {
            Some(icon) => assert_eq!(families, format!("{configured}, {icon}")),
            // A box with no Nerd Font installed has nothing to fall back to,
            // and the description must then be exactly what the user asked for.
            None => assert_eq!(families, configured),
        }
    }

    #[test]
    fn the_css_stack_quotes_and_escapes_both_families() {
        assert_eq!(
            css_font_stack("Monospace", Some("Symbols Nerd Font Mono")),
            "\"Monospace\", \"Symbols Nerd Font Mono\""
        );
        assert_eq!(css_font_stack("Monospace", None), "\"Monospace\"");
        assert_eq!(
            css_font_stack("Hack Nerd Font", Some("Symbols Nerd Font Mono")),
            "\"Hack Nerd Font\""
        );
        assert_eq!(
            css_font_stack("ev\"il", Some("Symbols Nerd Font Mono")),
            "\"ev\\\"il\", \"Symbols Nerd Font Mono\""
        );
    }
}
