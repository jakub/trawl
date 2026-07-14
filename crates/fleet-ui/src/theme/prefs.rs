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

use std::fmt;
use std::str::FromStr;

/// Returned by [`FromStr`] impls when the input doesn't name a known
/// variant. Carries the offending string so logs / warnings can show
/// what came in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseThemeError(pub String);

impl fmt::Display for ParseThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown theme/density/rowstyle value: `{}`", self.0)
    }
}

impl std::error::Error for ParseThemeError {}

/// Color theme — light is canonical, dark is parity.
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

    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
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

/// Row density — compact is the default ops-tool feel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Compact,
    Comfortable,
}

impl Density {
    #[must_use]
    pub fn as_attr(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
        }
    }

    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Compact => Self::Comfortable,
            Self::Comfortable => Self::Compact,
        }
    }
}

impl FromStr for Density {
    type Err = ParseThemeError;
    fn from_str(s: &str) -> Result<Self, ParseThemeError> {
        match s {
            "comfortable" => Ok(Self::Comfortable),
            "compact" => Ok(Self::Compact),
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
    pub(crate) theme: Theme,
    pub(crate) density: Density,
    pub(crate) rowstyle: RowStyle,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            theme: Theme::Light,
            density: Density::Compact,
            rowstyle: RowStyle::Bordered,
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

/// Parse a `localStorage` payload. Missing fields fall back to defaults
/// silently (older blobs predate newer fields); malformed JSON or
/// unknown values are recorded in `warnings` and the field is left at
/// its default.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn parse_stored(raw: &str) -> ParseOutcome {
    let mut warnings = Vec::new();
    let value: serde_json::Value = match serde_json::from_str(raw) {
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
    parse_field(&value, "density", &mut warnings, |d| out.density = d);
    parse_field(&value, "rowstyle", &mut warnings, |r| out.rowstyle = r);
    ParseOutcome {
        stored: out,
        warnings,
    }
}

fn parse_field<T, F>(value: &serde_json::Value, field: &str, warnings: &mut Vec<String>, mut set: F)
where
    T: FromStr<Err = ParseThemeError>,
    F: FnMut(T),
{
    let Some(s) = value.get(field).and_then(|v| v.as_str()) else {
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
    fn density_round_trips() {
        for v in [Density::Compact, Density::Comfortable] {
            assert_eq!(Density::from_str(v.as_attr()), Ok(v));
        }
    }

    #[test]
    fn rowstyle_round_trips() {
        for v in [RowStyle::Bordered, RowStyle::Striped, RowStyle::Plain] {
            assert_eq!(RowStyle::from_str(v.as_attr()), Ok(v));
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
        let out = parse_stored(r#"{"theme":"midnight"}"#);
        assert_eq!(
            out.stored,
            Stored::default(),
            "unknown theme value must not mutate the snapshot"
        );
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("midnight"));
        assert!(out.warnings[0].contains("theme"));
    }

    #[test]
    fn parse_stored_unknown_variant_preserves_other_fields() {
        let out = parse_stored(r#"{"theme":"midnight","density":"comfortable"}"#);
        assert_eq!(out.stored.theme, Theme::Light, "rejected, default kept");
        assert_eq!(
            out.stored.density,
            Density::Comfortable,
            "valid sibling field must still apply"
        );
        assert_eq!(out.stored.rowstyle, RowStyle::Bordered);
    }

    #[test]
    fn parse_stored_full_payload_round_trips() {
        let raw = r#"{"theme":"dark","density":"comfortable","rowstyle":"plain"}"#;
        let out = parse_stored(raw);
        assert_eq!(
            out.stored,
            Stored {
                theme: Theme::Dark,
                density: Density::Comfortable,
                rowstyle: RowStyle::Plain,
            }
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn parse_stored_wrong_field_type_silently_keeps_default() {
        // theme is a number rather than a string — `as_str()` returns None and
        // parse_field bails without warning. Same path as missing field.
        let out = parse_stored(r#"{"theme": 42}"#);
        assert_eq!(out.stored, Stored::default());
        assert!(out.warnings.is_empty());
    }
}
