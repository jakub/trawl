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

    Effect::new(move |_| {
        let snap = Stored {
            theme: prefs.theme.get(),
            density: prefs.density.get(),
            rowstyle: prefs.rowstyle.get(),
        };
        apply_to_dom(snap);
        write_stored(storage_key, snap);
    });

    prefs
}

#[derive(Clone, Copy)]
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
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Stored::default();
    };
    let mut out = Stored::default();
    if let Some(s) = value.get("theme").and_then(|v| v.as_str())
        && let Ok(t) = s.parse()
    {
        out.theme = t;
    }
    if let Some(s) = value.get("density").and_then(|v| v.as_str())
        && let Ok(d) = s.parse()
    {
        out.density = d;
    }
    if let Some(s) = value.get("rowstyle").and_then(|v| v.as_str())
        && let Ok(r) = s.parse()
    {
        out.rowstyle = r;
    }
    out
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
    let _ = storage.set_item(storage_key, &payload.to_string());
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
