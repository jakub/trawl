// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure pref types + parsers. No `web_sys`, no `leptos`, no I/O.
//!
//! The JSON shape this layer reads/writes is the contract with
//! `localStorage`. Round-trip tests (`as_attr` ↔ `FromStr`) and
//! corrupt-input tests live here so the contract is enforced on native
//! `cargo nextest run --workspace`, not just deferred to a wasm
//! integration suite.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::de::{Deserialize, Deserializer, Visitor};
use serde_json::value::RawValue;

/// Returned by [`FromStr`] impls when the input doesn't name a known
/// variant. Carries the offending string so logs / warnings can show
/// what came in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseThemeError(pub String);

impl fmt::Display for ParseThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown ui pref value: `{}`", self.0)
    }
}

impl std::error::Error for ParseThemeError {}

/// Resolved appearance for renderers. The user's choice is [`ThemePreference`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    #[must_use]
    pub fn as_attr(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

impl FromStr for Theme {
    type Err = ParseThemeError;
    fn from_str(s: &str) -> Result<Self, ParseThemeError> {
        match s {
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            _ => Err(ParseThemeError(s.to_owned())),
        }
    }
}

/// The user's stored appearance choice. System follows the operating system.
/// Renderers consume the binary [`Theme`] returned by [`Self::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    #[must_use]
    pub const fn as_attr(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Resolve against the current dark-scheme media query. Callers pass
    /// `false` when media-query access is unavailable.
    #[must_use]
    pub const fn resolve(self, system_dark: bool) -> Theme {
        match self {
            Self::Dark => Theme::Dark,
            Self::System if system_dark => Theme::Dark,
            Self::System | Self::Light => Theme::Light,
        }
    }
}

impl FromStr for ThemePreference {
    type Err = ParseThemeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "system" => Ok(Self::System),
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            _ => Err(ParseThemeError(s.to_owned())),
        }
    }
}

/// Table row decoration — bordered is the default; striped/plain are
/// user preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStyle {
    Bordered,
    Striped,
    Plain,
}

impl RowStyle {
    #[must_use]
    pub fn as_attr(self) -> &'static str {
        match self {
            Self::Bordered => "bordered",
            Self::Striped => "striped",
            Self::Plain => "plain",
        }
    }
}

impl FromStr for RowStyle {
    type Err = ParseThemeError;
    fn from_str(s: &str) -> Result<Self, ParseThemeError> {
        match s {
            "striped" => Ok(Self::Striped),
            "plain" => Ok(Self::Plain),
            "bordered" => Ok(Self::Bordered),
            _ => Err(ParseThemeError(s.to_owned())),
        }
    }
}

/// Sidebar presentation — expanded is the default; collapsed is
/// icon-only, with every label kept as screen-reader text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sidebar {
    Expanded,
    Collapsed,
}

impl Sidebar {
    #[must_use]
    pub fn as_attr(self) -> &'static str {
        match self {
            Self::Expanded => "expanded",
            Self::Collapsed => "collapsed",
        }
    }

    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Expanded => Self::Collapsed,
            Self::Collapsed => Self::Expanded,
        }
    }
}

impl FromStr for Sidebar {
    type Err = ParseThemeError;
    fn from_str(s: &str) -> Result<Self, ParseThemeError> {
        match s {
            "collapsed" => Ok(Self::Collapsed),
            "expanded" => Ok(Self::Expanded),
            _ => Err(ParseThemeError(s.to_owned())),
        }
    }
}

/// How a result row's fields are read — inline is the default (the
/// row expands in place), inspector docks a panel beside the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Details {
    Inline,
    Inspector,
}

impl Details {
    #[must_use]
    pub fn as_attr(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Inspector => "inspector",
        }
    }
}

impl FromStr for Details {
    type Err = ParseThemeError;
    fn from_str(s: &str) -> Result<Self, ParseThemeError> {
        match s {
            "inspector" => Ok(Self::Inspector),
            "inline" => Ok(Self::Inline),
            _ => Err(ParseThemeError(s.to_owned())),
        }
    }
}

/// How a result row is laid out — compact is the default column table,
/// message-first promotes the message to full width with the rest of
/// the row as a muted secondary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rows {
    Compact,
    MessageFirst,
}

impl Rows {
    #[must_use]
    pub fn as_attr(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::MessageFirst => "message-first",
        }
    }
}

impl FromStr for Rows {
    type Err = ParseThemeError;
    fn from_str(s: &str) -> Result<Self, ParseThemeError> {
        match s {
            "message-first" => Ok(Self::MessageFirst),
            "compact" => Ok(Self::Compact),
            _ => Err(ParseThemeError(s.to_owned())),
        }
    }
}

/// Persisted preference snapshot — the JSON shape on disk in
/// `localStorage`. `pub(crate)` so the wasm `runtime` layer can build,
/// read, and write it without leaking the on-disk shape to consumers.
///
/// `allow(dead_code)` because the only non-test consumer (`runtime`) is
/// cfg-gated to wasm32; the type is exercised on every target through
/// the `#[cfg(test)]` block below.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stored {
    pub(crate) theme: ThemePreference,
    pub(crate) rowstyle: RowStyle,
    pub(crate) sidebar: Sidebar,
    pub(crate) details: Details,
    pub(crate) rows: Rows,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            theme: ThemePreference::System,
            rowstyle: RowStyle::Bordered,
            sidebar: Sidebar::Expanded,
            details: Details::Inline,
            rows: Rows::Compact,
        }
    }
}

/// Outcome of parsing a `localStorage` blob. The `Vec<String>` carries
/// human-readable diagnostics — top-level JSON failure or per-field
/// unknown-variant messages — so the wasm caller can `console.warn`
/// them and tests can assert on them.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseOutcome {
    pub(crate) stored: Stored,
    pub(crate) warnings: Vec<String>,
}

/// Only recognized keys need a Unicode string. JSON permits lone surrogate
/// escapes that JavaScript accepts but Rust strings cannot represent. Decode
/// keys through serde's byte visitor so an unknown key cannot discard valid
/// preferences. Escaped spellings of recognized keys still compare normally.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PrefKey {
    Known(&'static str),
    Unknown,
}

impl<'de> Deserialize<'de> for PrefKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;

        impl Visitor<'_> for KeyVisitor {
            type Value = PrefKey;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON preference key")
            }

            fn visit_bytes<E: serde::de::Error>(self, value: &[u8]) -> Result<PrefKey, E> {
                Ok(match value {
                    b"theme" => PrefKey::Known("theme"),
                    b"rowstyle" => PrefKey::Known("rowstyle"),
                    b"sidebar" => PrefKey::Known("sidebar"),
                    b"details" => PrefKey::Known("details"),
                    b"rows" => PrefKey::Known("rows"),
                    _ => PrefKey::Unknown,
                })
            }
        }

        deserializer.deserialize_bytes(KeyVisitor)
    }
}

/// Validate all JSON grammar, but do not interpret unrelated values as Rust
/// numbers or strings. In particular, a valid theme survives an unrelated
/// `1e400` or lone-surrogate string. `RawValue` leaves `serde_json::Value`'s numeric
/// behavior unchanged for every other workspace consumer.
fn stored_fields(raw: &str) -> Result<BTreeMap<PrefKey, &RawValue>, serde_json::Error> {
    let value = serde_json::from_str::<&RawValue>(raw)?;
    if value.get().starts_with('{') {
        // BTreeMap keeps the last duplicate field, as JSON.parse does.
        serde_json::from_str(value.get())
    } else {
        Ok(BTreeMap::new())
    }
}

/// Parse a `localStorage` payload. Missing fields fall back to defaults
/// silently (older blobs predate newer fields); malformed JSON or
/// unknown values are recorded in `warnings` and the field is left at
/// its default.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn parse_stored(raw: &str) -> ParseOutcome {
    let mut warnings = Vec::new();
    let value = match stored_fields(raw) {
        Ok(v) => v,
        Err(err) => {
            warnings.push(format!(
                "stored prefs are not valid JSON ({err}); falling back to \
                 defaults. raw payload preserved in localStorage for \
                 inspection. value: {raw}"
            ));
            return ParseOutcome {
                stored: Stored::default(),
                warnings,
            };
        }
    };
    let mut out = Stored::default();
    parse_field(&value, "theme", &mut warnings, |t| out.theme = t);
    parse_field(&value, "rowstyle", &mut warnings, |r| out.rowstyle = r);
    parse_field(&value, "sidebar", &mut warnings, |s| out.sidebar = s);
    parse_field(&value, "details", &mut warnings, |d| out.details = d);
    parse_field(&value, "rows", &mut warnings, |r| out.rows = r);
    ParseOutcome {
        stored: out,
        warnings,
    }
}

fn parse_field<T, F>(
    value: &BTreeMap<PrefKey, &RawValue>,
    field: &'static str,
    warnings: &mut Vec<String>,
    mut set: F,
) where
    T: FromStr<Err = ParseThemeError>,
    F: FnMut(T),
{
    let Some(s) = value
        .get(&PrefKey::Known(field))
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
    else {
        return;
    };
    match s.parse::<T>() {
        Ok(parsed) => set(parsed),
        Err(ParseThemeError(bad)) => warnings.push(format!(
            "unknown value `{bad}` for stored pref `{field}`; keeping \
             default. (typo, future variant, or hand-edit?)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_round_trips() {
        for v in [Theme::Light, Theme::Dark] {
            assert_eq!(Theme::from_str(v.as_attr()), Ok(v));
        }
    }

    #[test]
    fn shared_preference_fixtures() {
        use std::collections::HashSet;

        let table: serde_json::Value =
            serde_json::from_str(include_str!("preference-fixtures.json")).unwrap();
        let rows = table.as_array().expect("fixture table must be an array");
        assert!(!rows.is_empty(), "fixture table must not be empty");
        let mut ids = HashSet::new();
        for row in rows {
            let fields = row.as_object().expect("fixture must be an object");
            let expected_keys = ["id", "raw", "preference", "media", "resolved"];
            assert_eq!(fields.len(), expected_keys.len(), "unknown fixture fields");
            for key in expected_keys {
                assert!(fields.contains_key(key), "missing fixture field {key}");
            }
            let id = fields["id"].as_str().expect("id must be a string");
            assert!(!id.is_empty() && ids.insert(id), "empty or duplicate id");
            let raw = &fields["raw"];
            assert!(raw.is_null() || raw.is_string(), "{id}: invalid raw value");
            let expected = fields["preference"]
                .as_str()
                .expect("preference must be a string")
                .parse::<ThemePreference>()
                .expect("unknown expected preference");
            let resolved = fields["resolved"]
                .as_str()
                .expect("resolved must be a string")
                .parse::<Theme>()
                .expect("unknown expected resolved theme");
            let media = fields["media"].as_str().expect("media must be a string");
            assert!(matches!(media, "light" | "dark" | "unavailable" | "throws"));
            let stored = raw
                .as_str()
                .map_or_else(Stored::default, |s| parse_stored(s).stored);
            assert_eq!(stored.theme, expected, "{id}: preference");
            assert_eq!(
                stored.theme.resolve(media == "dark"),
                resolved,
                "{id}: resolution"
            );
        }
    }

    #[test]
    fn preference_round_trips() {
        for preference in [
            ThemePreference::System,
            ThemePreference::Light,
            ThemePreference::Dark,
        ] {
            assert_eq!(preference.as_attr().parse(), Ok(preference));
        }
    }

    #[test]
    fn valid_theme_survives_invalid_reading_preferences() {
        let out = parse_stored(
            r#"{"theme":"dark","rowstyle":42,"sidebar":"bad","details":[],"rows":false}"#,
        );
        assert_eq!(
            out.stored,
            Stored {
                theme: ThemePreference::Dark,
                ..Stored::default()
            }
        );
    }

    #[test]
    fn rowstyle_round_trips() {
        for v in [RowStyle::Bordered, RowStyle::Striped, RowStyle::Plain] {
            assert_eq!(RowStyle::from_str(v.as_attr()), Ok(v));
        }
    }

    #[test]
    fn sidebar_round_trips() {
        for v in [Sidebar::Expanded, Sidebar::Collapsed] {
            assert_eq!(Sidebar::from_str(v.as_attr()), Ok(v));
            assert_eq!(v.toggled().toggled(), v);
        }
    }

    #[test]
    fn details_round_trips() {
        for v in [Details::Inline, Details::Inspector] {
            assert_eq!(Details::from_str(v.as_attr()), Ok(v));
        }
    }

    #[test]
    fn rows_round_trips() {
        for v in [Rows::Compact, Rows::MessageFirst] {
            assert_eq!(Rows::from_str(v.as_attr()), Ok(v));
        }
    }

    #[test]
    fn parse_error_carries_offending_input() {
        let err = Theme::from_str("midnight").unwrap_err();
        assert_eq!(err.0, "midnight");
        let msg = err.to_string();
        assert!(msg.contains("midnight"), "display lost the input: {msg}");
    }

    #[test]
    fn parse_stored_empty_object_returns_defaults() {
        let out = parse_stored("{}");
        assert_eq!(out.stored, Stored::default());
        assert!(out.warnings.is_empty(), "no fields = no warnings");
    }

    #[test]
    fn parse_stored_malformed_json_returns_defaults_with_warning() {
        let out = parse_stored("not json");
        assert_eq!(out.stored, Stored::default());
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("not valid JSON"));
        assert!(
            out.warnings[0].contains("not json"),
            "warning should echo the raw payload"
        );
    }

    #[test]
    fn parse_stored_unknown_variant_warns_and_keeps_default() {
        for (raw, field, value) in [
            (r#"{"theme":"midnight"}"#, "theme", "midnight"),
            (r#"{"sidebar":"hidden"}"#, "sidebar", "hidden"),
            (r#"{"details":"popover"}"#, "details", "popover"),
            (r#"{"rows":"roomy"}"#, "rows", "roomy"),
        ] {
            let out = parse_stored(raw);
            assert_eq!(
                out.stored,
                Stored::default(),
                "unknown {field} value must not mutate the snapshot"
            );
            assert_eq!(out.warnings.len(), 1, "{raw}");
            assert!(out.warnings[0].contains(value), "{raw}");
            assert!(out.warnings[0].contains(field), "{raw}");
        }
    }

    #[test]
    fn parse_stored_unknown_variant_preserves_other_fields() {
        let out = parse_stored(r#"{"theme":"midnight","rowstyle":"plain"}"#);
        assert_eq!(
            out.stored.theme,
            ThemePreference::System,
            "rejected, default kept"
        );
        assert_eq!(
            out.stored.rowstyle,
            RowStyle::Plain,
            "valid sibling field must still apply"
        );
    }

    #[test]
    fn parse_stored_ignores_retired_density_field() {
        // Older blobs carry a `density` field from before the pref was
        // removed; it must be skipped silently, not warned about.
        let out = parse_stored(r#"{"theme":"dark","density":"compact"}"#);
        assert_eq!(out.stored.theme, ThemePreference::Dark);
        assert!(out.warnings.is_empty(), "retired field must not warn");
    }

    #[test]
    fn parse_stored_full_payload_round_trips() {
        let raw = r#"{"theme":"dark","rowstyle":"plain","sidebar":"collapsed","details":"inspector","rows":"message-first"}"#;
        let out = parse_stored(raw);
        assert_eq!(
            out.stored,
            Stored {
                theme: ThemePreference::Dark,
                rowstyle: RowStyle::Plain,
                sidebar: Sidebar::Collapsed,
                details: Details::Inspector,
                rows: Rows::MessageFirst,
            }
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn parse_stored_wrong_field_type_silently_keeps_default() {
        // The recognized field is not a JSON string, so parse_field leaves its
        // default without warning, as it does for a missing field.
        let out = parse_stored(r#"{"theme": 42}"#);
        assert_eq!(out.stored, Stored::default());
        assert!(out.warnings.is_empty());
    }
}
