// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Semantic theme system for TUI colors.
//!
//! Provides a [`Theme`] struct with named color slots covering UI chrome and
//! DSL syntax highlighting. Built-in themes (`dark`, `light`, `nord`,
//! `dracula`, `catppuccin`) are defined as plain functions. Custom themes
//! can be loaded from TOML files in `~/.config/trawl/themes/`.

use std::path::PathBuf;

use ratatui::style::Color;

/// Semantic color theme for the TUI.
///
/// All color fields map to ratatui [`Color`] values. Modifiers (bold, dim,
/// italic) are intentionally *not* themed — they stay hardcoded in render
/// logic as they're universal across all themes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Human-readable theme name (for display in status bar / help).
    pub name: String,

    // -- borders & focus --
    /// Border color when a pane has focus.
    pub border_focused: Color,
    /// Border color when a pane is unfocused.
    pub border_unfocused: Color,

    // -- surfaces --
    /// Panel/popup background color.
    pub surface: Color,
    /// Highlighted surface (selected row, active element background).
    pub surface_highlight: Color,

    // -- text --
    /// Primary content text.
    pub text_primary: Color,
    /// Muted text (hints, metadata, labels, disabled elements).
    pub text_muted: Color,
    /// Accent text (interactive elements, field names, links).
    pub text_accent: Color,

    // -- status indicators --
    /// Success state (query complete, high coverage).
    pub status_success: Color,
    /// Error state (query failed, deletion).
    pub status_error: Color,
    /// Warning / in-progress state (running query, medium coverage).
    pub status_warning: Color,
    /// Informational indicator (live mode, special status).
    pub status_info: Color,
    /// Idle / neutral state.
    pub status_idle: Color,

    // -- search --
    /// Background for the currently active search match.
    pub search_match_active: Color,
    /// Background for non-active search matches.
    pub search_match_other: Color,

    // -- tabs --
    /// Active tab foreground.
    pub tab_active_fg: Color,
    /// Active tab background.
    pub tab_active_bg: Color,

    // -- table --
    /// Table column header color.
    pub table_header: Color,

    // -- chart series cycling palette --
    /// Colors for sparkline/timechart series (cycled in order).
    pub chart_series: [Color; 6],

    // -- syntax highlighting --
    /// DSL syntax highlighting colors.
    pub syntax: SyntaxColors,
}

/// Syntax highlighting colors for DSL tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxColors {
    /// Pipe stages and built-in functions (`stats`, `where`, `count()`).
    pub stage: Color,
    /// Filter keys in field=value expressions.
    pub filter_key: Color,
    /// Operators (`|`, `=`, `>=`, `==`).
    pub operator: Color,
    /// Logical keywords (`AND`, `OR`, `NOT`).
    pub logical: Color,
    /// Field names recognized from schema.
    pub field_known: Color,
    /// Field names not found in schema.
    pub field_unknown: Color,
    /// String literals (`"quoted"`).
    pub string: Color,
    /// Numeric literals (`42`, `1.5`).
    pub number: Color,
    /// Regex literals (`/pattern/`).
    pub regex: Color,
    /// Negated terms (`-word`).
    pub negated: Color,
    /// Comments (`// ...`).
    pub comment: Color,
}

// ---------------------------------------------------------------------------
// Built-in themes
// ---------------------------------------------------------------------------

/// Dark theme — the default. Maps 1:1 to the original hardcoded colors.
pub fn dark() -> Theme {
    Theme {
        name: "dark".to_owned(),
        border_focused: Color::Cyan,
        border_unfocused: Color::DarkGray,
        surface: Color::Black,
        surface_highlight: Color::DarkGray,
        text_primary: Color::White,
        text_muted: Color::DarkGray,
        text_accent: Color::Cyan,
        status_success: Color::Green,
        status_error: Color::Red,
        status_warning: Color::Yellow,
        status_info: Color::Magenta,
        status_idle: Color::Gray,
        search_match_active: Color::Yellow,
        search_match_other: Color::DarkGray,
        tab_active_fg: Color::Black,
        tab_active_bg: Color::Cyan,
        table_header: Color::Yellow,
        chart_series: [
            Color::Cyan,
            Color::Yellow,
            Color::Magenta,
            Color::Green,
            Color::Red,
            Color::Blue,
        ],
        syntax: SyntaxColors {
            stage: Color::Magenta,
            filter_key: Color::Cyan,
            operator: Color::Yellow,
            logical: Color::Yellow,
            field_known: Color::Green,
            field_unknown: Color::White,
            string: Color::LightYellow,
            number: Color::LightBlue,
            regex: Color::LightRed,
            negated: Color::Red,
            comment: Color::DarkGray,
        },
    }
}

// ---------------------------------------------------------------------------
// Theme resolution
// ---------------------------------------------------------------------------

/// Resolve a theme name from config into a [`Theme`].
///
/// Resolution order:
/// 1. `"default"` or `"dark"` → built-in dark theme
/// 2. Other built-in names (`"light"`, `"nord"`, etc.)
/// 3. Path containing `/` or ending in `.toml` → load file directly
/// 4. Bare name → try `~/.config/trawl/themes/{name}.toml`
/// 5. Not found → warn + fall back to dark
pub fn resolve(name: &str) -> Theme {
    match name {
        "default" | "dark" => dark(),
        // Phase 3: additional built-in themes will be added here.
        // "light" => light(),
        // "nord" => nord(),
        // "dracula" => dracula(),
        // "catppuccin" => catppuccin_mocha(),
        other => resolve_custom(other),
    }
}

/// Attempt to load a custom theme from a TOML file.
fn resolve_custom(name: &str) -> Theme {
    let path = if name.contains('/')
        || std::path::Path::new(name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
    {
        PathBuf::from(name)
    } else {
        // Try ~/.config/trawl/themes/{name}.toml
        let base = shellexpand::tilde("~/.config/trawl/themes");
        PathBuf::from(base.as_ref()).join(format!("{name}.toml"))
    };

    if path.exists() {
        match load_theme_file(&path) {
            Ok(theme) => return theme,
            Err(e) => {
                eprintln!("trawl: failed to load theme '{}': {e}", path.display());
            }
        }
    } else {
        eprintln!(
            "trawl: unknown theme '{name}', falling back to dark (looked for {})",
            path.display()
        );
    }

    dark()
}

/// Load and parse a TOML theme file.
///
/// Phase 3: this will deserialize a TOML file into a `Theme`.
/// For now, it always returns an error since custom themes aren't yet supported.
fn load_theme_file(path: &std::path::Path) -> Result<Theme, String> {
    Err(format!(
        "custom theme files not yet supported ({})",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_has_correct_name() {
        let theme = dark();
        assert_eq!(theme.name, "dark");
    }

    #[test]
    fn resolve_default_returns_dark() {
        let theme = resolve("default");
        assert_eq!(theme, dark());
    }

    #[test]
    fn resolve_dark_returns_dark() {
        let theme = resolve("dark");
        assert_eq!(theme, dark());
    }

    #[test]
    fn resolve_unknown_falls_back_to_dark() {
        // Unknown names fall back to dark (with a warning on stderr).
        let theme = resolve("nonexistent_theme_xyz");
        assert_eq!(theme, dark());
    }
}
