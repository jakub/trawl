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
use serde::Deserialize;

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

/// Helper to construct an RGB color from a hex value.
const fn rgb(hex: u32) -> Color {
    Color::Rgb(
        ((hex >> 16) & 0xFF) as u8,
        ((hex >> 8) & 0xFF) as u8,
        (hex & 0xFF) as u8,
    )
}

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

/// Light theme — inverted surfaces with darker accents for light terminals.
pub fn light() -> Theme {
    Theme {
        name: "light".to_owned(),
        border_focused: rgb(0x00_5F_87), // dark cyan
        border_unfocused: rgb(0xAF_AF_AF),
        surface: Color::White,
        surface_highlight: rgb(0xD7_D7_D7),
        text_primary: rgb(0x1C_1C_1C),
        text_muted: rgb(0x87_87_87),
        text_accent: rgb(0x00_5F_87),
        status_success: rgb(0x00_5F_00),
        status_error: rgb(0xAF_00_00),
        status_warning: rgb(0x87_5F_00),
        status_info: rgb(0x5F_00_87),
        status_idle: rgb(0x87_87_87),
        search_match_active: rgb(0xFF_D7_00),
        search_match_other: rgb(0xE4_E4_E4),
        tab_active_fg: Color::White,
        tab_active_bg: rgb(0x00_5F_87),
        table_header: rgb(0x87_5F_00),
        chart_series: [
            rgb(0x00_5F_87),
            rgb(0x87_5F_00),
            rgb(0x5F_00_87),
            rgb(0x00_5F_00),
            rgb(0xAF_00_00),
            rgb(0x00_5F_5F),
        ],
        syntax: SyntaxColors {
            stage: rgb(0x5F_00_87),
            filter_key: rgb(0x00_5F_87),
            operator: rgb(0x87_5F_00),
            logical: rgb(0x87_5F_00),
            field_known: rgb(0x00_5F_00),
            field_unknown: rgb(0x1C_1C_1C),
            string: rgb(0x5F_5F_00),
            number: rgb(0x00_5F_5F),
            regex: rgb(0xAF_00_00),
            negated: rgb(0xAF_00_00),
            comment: rgb(0xAF_AF_AF),
        },
    }
}

/// Nord theme — based on the Nord color palette (arctic, north-bluish).
pub fn nord() -> Theme {
    // Nord palette: https://www.nordtheme.com/docs/colors-and-palettes
    let polar0 = rgb(0x2E_34_40); // nord0 - background
    let polar1 = rgb(0x3B_42_52); // nord1 - elevated surface
    let polar2 = rgb(0x43_4C_5E); // nord2 - selection
    let polar3 = rgb(0x4C_56_6A); // nord3 - comments/muted
    let snow0 = rgb(0xD8_DE_E9); // nord4 - primary text
    let frost1 = rgb(0x88_C0_D0); // nord8 - cyan accent
    let frost3 = rgb(0x5E_81_AC); // nord10 - dark blue
    let aurora0 = rgb(0xBF_61_6A); // nord11 - red
    let aurora1 = rgb(0xD0_87_70); // nord12 - orange
    let aurora2 = rgb(0xEB_CB_8B); // nord13 - yellow
    let aurora3 = rgb(0xA3_BE_8C); // nord14 - green
    let aurora4 = rgb(0xB4_8E_AD); // nord15 - purple

    Theme {
        name: "nord".to_owned(),
        border_focused: frost1,
        border_unfocused: polar3,
        surface: polar0,
        surface_highlight: polar2,
        text_primary: snow0,
        text_muted: polar3,
        text_accent: frost1,
        status_success: aurora3,
        status_error: aurora0,
        status_warning: aurora2,
        status_info: aurora4,
        status_idle: polar3,
        search_match_active: aurora2,
        search_match_other: polar1,
        tab_active_fg: polar0,
        tab_active_bg: frost1,
        table_header: aurora2,
        chart_series: [frost1, aurora2, aurora4, aurora3, aurora0, frost3],
        syntax: SyntaxColors {
            stage: aurora4,
            filter_key: frost1,
            operator: aurora2,
            logical: aurora2,
            field_known: aurora3,
            field_unknown: snow0,
            string: aurora2,
            number: aurora4,
            regex: aurora1,
            negated: aurora0,
            comment: polar3,
        },
    }
}

/// Dracula theme — based on the Dracula color palette.
pub fn dracula() -> Theme {
    // Dracula palette: https://draculatheme.com/contribute
    let bg = rgb(0x28_2A_36);
    let current = rgb(0x44_47_5A);
    let fg = rgb(0xF8_F8_F2);
    let comment = rgb(0x62_72_A4);
    let cyan = rgb(0x8B_E9_FD);
    let green = rgb(0x50_FA_7B);
    let orange = rgb(0xFF_B8_6C);
    let pink = rgb(0xFF_79_C6);
    let purple = rgb(0xBD_93_F9);
    let red = rgb(0xFF_55_55);
    let yellow = rgb(0xF1_FA_8C);

    Theme {
        name: "dracula".to_owned(),
        border_focused: purple,
        border_unfocused: comment,
        surface: bg,
        surface_highlight: current,
        text_primary: fg,
        text_muted: comment,
        text_accent: cyan,
        status_success: green,
        status_error: red,
        status_warning: yellow,
        status_info: pink,
        status_idle: comment,
        search_match_active: yellow,
        search_match_other: current,
        tab_active_fg: bg,
        tab_active_bg: purple,
        table_header: purple,
        chart_series: [cyan, yellow, pink, green, red, orange],
        syntax: SyntaxColors {
            stage: pink,
            filter_key: cyan,
            operator: pink,
            logical: pink,
            field_known: green,
            field_unknown: fg,
            string: yellow,
            number: purple,
            regex: red,
            negated: red,
            comment,
        },
    }
}

/// Catppuccin Mocha theme — based on Catppuccin's warmest flavor.
pub fn catppuccin_mocha() -> Theme {
    // Catppuccin Mocha: https://catppuccin.com/palette
    let base = rgb(0x1E_1E_2E);
    let surface0 = rgb(0x31_32_44);
    let surface1 = rgb(0x45_47_5A);
    let overlay0 = rgb(0x6C_70_86);
    let text = rgb(0xCD_D6_F4);
    let pink = rgb(0xF5_C2_E7);
    let mauve = rgb(0xCB_A6_F7);
    let red = rgb(0xF3_8B_A8);
    let maroon = rgb(0xEB_A0_AC);
    let peach = rgb(0xFA_B3_87);
    let yellow = rgb(0xF9_E2_AF);
    let green = rgb(0xA6_E3_A1);
    let sky = rgb(0x89_DC_EB);
    let sapphire = rgb(0x74_C7_EC);

    Theme {
        name: "catppuccin".to_owned(),
        border_focused: mauve,
        border_unfocused: surface1,
        surface: base,
        surface_highlight: surface0,
        text_primary: text,
        text_muted: overlay0,
        text_accent: sapphire,
        status_success: green,
        status_error: red,
        status_warning: yellow,
        status_info: pink,
        status_idle: overlay0,
        search_match_active: yellow,
        search_match_other: surface0,
        tab_active_fg: base,
        tab_active_bg: mauve,
        table_header: mauve,
        chart_series: [sapphire, yellow, pink, green, red, peach],
        syntax: SyntaxColors {
            stage: mauve,
            filter_key: sapphire,
            operator: sky,
            logical: sky,
            field_known: green,
            field_unknown: text,
            string: green,
            number: peach,
            regex: red,
            negated: maroon,
            comment: overlay0,
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
        "light" => light(),
        "nord" => nord(),
        "dracula" => dracula(),
        "catppuccin" | "catppuccin-mocha" | "catppuccin_mocha" => catppuccin_mocha(),
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
fn load_theme_file(path: &std::path::Path) -> Result<Theme, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let raw: RawTheme = toml::from_str(&content).map_err(|e| e.to_string())?;
    raw.into_theme()
}

// ---------------------------------------------------------------------------
// TOML deserialization types
// ---------------------------------------------------------------------------

/// Intermediate deserialization target for custom theme files.
///
/// Colors are strings that accept: named ANSI colors (`"cyan"`, `"dark_gray"`),
/// hex RGB (`"#268bd2"`), or 256-color index as integer.
#[derive(Deserialize)]
struct RawTheme {
    name: Option<String>,

    border_focused: String,
    border_unfocused: String,
    surface: String,
    surface_highlight: String,
    text_primary: String,
    text_muted: String,
    text_accent: String,
    status_success: String,
    status_error: String,
    status_warning: String,
    status_info: String,
    status_idle: String,
    search_match_active: String,
    search_match_other: String,
    tab_active_fg: String,
    tab_active_bg: String,
    table_header: String,
    chart_series: Vec<String>,

    syntax: RawSyntaxColors,
}

#[derive(Deserialize)]
struct RawSyntaxColors {
    stage: String,
    filter_key: String,
    operator: String,
    logical: String,
    field_known: String,
    field_unknown: String,
    string: String,
    number: String,
    regex: String,
    negated: String,
    comment: String,
}

impl RawTheme {
    fn into_theme(self) -> Result<Theme, String> {
        let chart: Vec<Color> = self
            .chart_series
            .iter()
            .map(|s| parse_color(s))
            .collect::<Result<_, _>>()?;
        let chart_arr: [Color; 6] = chart.try_into().map_err(|v: Vec<Color>| {
            format!("chart_series must have exactly 6 colors, got {}", v.len())
        })?;

        Ok(Theme {
            name: self.name.unwrap_or_else(|| "custom".to_owned()),
            border_focused: parse_color(&self.border_focused)?,
            border_unfocused: parse_color(&self.border_unfocused)?,
            surface: parse_color(&self.surface)?,
            surface_highlight: parse_color(&self.surface_highlight)?,
            text_primary: parse_color(&self.text_primary)?,
            text_muted: parse_color(&self.text_muted)?,
            text_accent: parse_color(&self.text_accent)?,
            status_success: parse_color(&self.status_success)?,
            status_error: parse_color(&self.status_error)?,
            status_warning: parse_color(&self.status_warning)?,
            status_info: parse_color(&self.status_info)?,
            status_idle: parse_color(&self.status_idle)?,
            search_match_active: parse_color(&self.search_match_active)?,
            search_match_other: parse_color(&self.search_match_other)?,
            tab_active_fg: parse_color(&self.tab_active_fg)?,
            tab_active_bg: parse_color(&self.tab_active_bg)?,
            table_header: parse_color(&self.table_header)?,
            chart_series: chart_arr,
            syntax: SyntaxColors {
                stage: parse_color(&self.syntax.stage)?,
                filter_key: parse_color(&self.syntax.filter_key)?,
                operator: parse_color(&self.syntax.operator)?,
                logical: parse_color(&self.syntax.logical)?,
                field_known: parse_color(&self.syntax.field_known)?,
                field_unknown: parse_color(&self.syntax.field_unknown)?,
                string: parse_color(&self.syntax.string)?,
                number: parse_color(&self.syntax.number)?,
                regex: parse_color(&self.syntax.regex)?,
                negated: parse_color(&self.syntax.negated)?,
                comment: parse_color(&self.syntax.comment)?,
            },
        })
    }
}

/// Parse a color string. Accepts:
/// - Named ANSI: `"black"`, `"red"`, `"green"`, `"yellow"`, `"blue"`,
///   `"magenta"`, `"cyan"`, `"gray"`, `"dark_gray"`, `"light_red"`, etc.
/// - Hex RGB: `"#2e3440"` or `"2e3440"`
/// - 256-color index: `"208"` (pure digits)
fn parse_color(s: &str) -> Result<Color, String> {
    let s = s.trim();

    // Try named ANSI colors first.
    match s.to_lowercase().as_str() {
        "black" => return Ok(Color::Black),
        "red" => return Ok(Color::Red),
        "green" => return Ok(Color::Green),
        "yellow" => return Ok(Color::Yellow),
        "blue" => return Ok(Color::Blue),
        "magenta" => return Ok(Color::Magenta),
        "cyan" => return Ok(Color::Cyan),
        "gray" | "grey" => return Ok(Color::Gray),
        "dark_gray" | "dark_grey" | "darkgray" | "darkgrey" => return Ok(Color::DarkGray),
        "light_red" | "lightred" => return Ok(Color::LightRed),
        "light_green" | "lightgreen" => return Ok(Color::LightGreen),
        "light_yellow" | "lightyellow" => return Ok(Color::LightYellow),
        "light_blue" | "lightblue" => return Ok(Color::LightBlue),
        "light_magenta" | "lightmagenta" => return Ok(Color::LightMagenta),
        "light_cyan" | "lightcyan" => return Ok(Color::LightCyan),
        "white" => return Ok(Color::White),
        _ => {}
    }

    // Try hex RGB (#RRGGBB or RRGGBB).
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        let val =
            u32::from_str_radix(hex, 16).map_err(|e| format!("invalid hex color '{s}': {e}"))?;
        return Ok(rgb(val));
    }

    // Try 256-color index.
    if let Ok(idx) = s.parse::<u8>() {
        return Ok(Color::Indexed(idx));
    }

    Err(format!(
        "unknown color '{s}' — use a named color, #RRGGBB hex, or 0-255 index"
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

    #[test]
    fn resolve_builtin_names() {
        assert_eq!(resolve("light").name, "light");
        assert_eq!(resolve("nord").name, "nord");
        assert_eq!(resolve("dracula").name, "dracula");
        assert_eq!(resolve("catppuccin").name, "catppuccin");
        assert_eq!(resolve("catppuccin-mocha").name, "catppuccin");
        assert_eq!(resolve("catppuccin_mocha").name, "catppuccin");
    }

    #[test]
    fn all_builtins_have_six_chart_colors() {
        for theme in [dark(), light(), nord(), dracula(), catppuccin_mocha()] {
            assert_eq!(
                theme.chart_series.len(),
                6,
                "theme '{}' chart_series",
                theme.name
            );
        }
    }

    #[test]
    fn parse_named_colors() {
        assert_eq!(parse_color("cyan").unwrap(), Color::Cyan);
        assert_eq!(parse_color("dark_gray").unwrap(), Color::DarkGray);
        assert_eq!(parse_color("DarkGray").unwrap(), Color::DarkGray);
        assert_eq!(parse_color("white").unwrap(), Color::White);
    }

    #[test]
    fn parse_hex_colors() {
        assert_eq!(
            parse_color("#2e3440").unwrap(),
            Color::Rgb(0x2e, 0x34, 0x40)
        );
        assert_eq!(parse_color("ff5500").unwrap(), Color::Rgb(0xff, 0x55, 0x00));
    }

    #[test]
    fn parse_indexed_colors() {
        assert_eq!(parse_color("208").unwrap(), Color::Indexed(208));
    }

    #[test]
    fn parse_invalid_color() {
        assert!(parse_color("not_a_color").is_err());
    }
}
