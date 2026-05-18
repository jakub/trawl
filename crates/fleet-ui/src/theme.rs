// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Theme / density / row-style preferences synced to `localStorage`
//! and projected onto `<html data-*>` attributes.
//!
//! The CSS in `styles/fleet-ui.css` reads these as `[data-theme]`,
//! `[data-density]`, `[data-rowstyle]` selectors. Writing them on
//! `<html>` (not `<body>`) matches the design spec and keeps the
//! cascade authoritative for `:root` token overrides.
//!
//! `install()` takes a `storage_key` so each consuming app uses its
//! own localStorage namespace — `"trawl.ui"` for trawl-web, `"coastwatch.ui"`
//! for coastwatch-web, and so on.

use std::str::FromStr;

use leptos::prelude::*;
use wasm_bindgen::JsValue;

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
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            _ => Err(()),
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
}

impl FromStr for Density {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "comfortable" => Ok(Self::Comfortable),
            "compact" => Ok(Self::Compact),
            _ => Err(()),
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
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "striped" => Ok(Self::Striped),
            "plain" => Ok(Self::Plain),
            "bordered" => Ok(Self::Bordered),
            _ => Err(()),
        }
    }
}

/// Reactive UI preference signals + an effect that mirrors them onto
/// `<html data-*>` and persists them to `localStorage`.
///
/// Use the returned signals to read/write preferences from anywhere in
/// the app. Provide them via `provide_context` from a top-level
/// component so descendants can pick them up via `use_context`.
#[derive(Debug, Clone, Copy)]
pub struct UiPrefs {
    pub theme: RwSignal<Theme>,
    pub density: RwSignal<Density>,
    pub rowstyle: RwSignal<RowStyle>,
}

/// Initialize preference signals from `localStorage` + install the
/// effect that keeps `<html>` attributes in sync.
///
/// `storage_key` is the localStorage key under which a JSON snapshot
/// is persisted — pass `"trawl.ui"`, `"coastwatch.ui"`, etc.
///
/// Call once at app boot from inside the `<App/>` body so the effect
/// runs in the reactive context.
#[must_use]
pub fn install(storage_key: &'static str) -> UiPrefs {
    let stored = read_stored(storage_key);
    let prefs = UiPrefs {
        theme: RwSignal::new(stored.theme),
        density: RwSignal::new(stored.density),
        rowstyle: RwSignal::new(stored.rowstyle),
    };

    // Track the last value we persisted so we never overwrite storage with
    // what's already there. This matters for the corruption case: if
    // read_stored fell back to defaults because the blob was malformed,
    // last_written matches the in-memory snap and we leave the corrupt blob
    // in place — letting the user inspect it and giving the console warning
    // time to register before any change wipes the evidence.
    let last_written = StoredValue::new(stored);

    Effect::new(move |_| {
        let snap = Stored {
            theme: prefs.theme.get(),
            density: prefs.density.get(),
            rowstyle: prefs.rowstyle.get(),
        };
        apply_to_dom(snap);
        if last_written.with_value(|w| *w != snap) {
            write_stored(storage_key, snap);
            last_written.set_value(snap);
        }
    });

    prefs
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stored {
    theme: Theme,
    density: Density,
    rowstyle: RowStyle,
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

fn read_stored(storage_key: &str) -> Stored {
    let Some(storage) = local_storage() else {
        return Stored::default();
    };
    let Ok(Some(raw)) = storage.get_item(storage_key) else {
        return Stored::default();
    };
    let value = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => v,
        Err(err) => {
            warn(&format!(
                "fleet-ui: stored prefs at `{storage_key}` are not valid JSON \
                 ({err}); falling back to defaults. raw payload preserved \
                 in localStorage for inspection. value: {raw}"
            ));
            return Stored::default();
        }
    };
    let mut out = Stored::default();
    parse_field(&value, "theme", |t| out.theme = t);
    parse_field(&value, "density", |d| out.density = d);
    parse_field(&value, "rowstyle", |r| out.rowstyle = r);
    out
}

fn parse_field<T, F>(value: &serde_json::Value, field: &str, mut set: F)
where
    T: FromStr,
    F: FnMut(T),
{
    let Some(s) = value.get(field).and_then(|v| v.as_str()) else {
        // Field missing or wrong shape — keep the existing default. This isn't a
        // user-visible problem worth a warn; older blobs predate newer fields.
        return;
    };
    match s.parse::<T>() {
        Ok(parsed) => set(parsed),
        Err(_) => warn(&format!(
            "fleet-ui: unknown value `{s}` for stored pref `{field}`; \
             keeping default. (typo, future variant, or hand-edit?)"
        )),
    }
}

fn warn(msg: &str) {
    web_sys::console::warn_1(&JsValue::from_str(msg));
}

fn write_stored(storage_key: &str, s: Stored) {
    let Some(storage) = local_storage() else {
        return;
    };
    let payload = serde_json::json!({
        "theme":    s.theme.as_attr(),
        "density":  s.density.as_attr(),
        "rowstyle": s.rowstyle.as_attr(),
    });
    if let Err(err) = storage.set_item(storage_key, &payload.to_string()) {
        // QuotaExceededError (Safari private browsing, full storage) is the
        // realistic case. Warn so "my settings stopped sticking" surfaces in
        // devtools instead of going to ground.
        warn(&format!(
            "fleet-ui: failed to persist prefs to localStorage `{storage_key}`: \
             {err:?}. preferences are still applied this session but won't survive reload."
        ));
    }
}

fn apply_to_dom(s: Stored) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(html) = doc.document_element() else {
        return;
    };
    let _ = html.set_attribute("data-theme", s.theme.as_attr());
    let _ = html.set_attribute("data-density", s.density.as_attr());
    let _ = html.set_attribute("data-rowstyle", s.rowstyle.as_attr());
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}
