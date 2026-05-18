// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wasm-only `localStorage` + `<html data-*>` glue around the pure
//! parsers in [`super::prefs`].
//!
//! The CSS in `styles/fleet-ui.css` reads `[data-theme]`,
//! `[data-density]`, `[data-rowstyle]` selectors. Writing them on
//! `<html>` (not `<body>`) matches the design spec and keeps the
//! cascade authoritative for `:root` token overrides.
//!
//! [`install`] takes a `storage_key` so each consuming app uses its own
//! localStorage namespace — `"trawl.ui"` for trawl-web, `"coastwatch.ui"`
//! for coastwatch-web, and so on.

use leptos::prelude::*;
use wasm_bindgen::JsValue;

use super::prefs::{Density, ParseOutcome, RowStyle, Stored, Theme, parse_stored};

/// Reactive UI preference signals + an effect that mirrors them onto
/// `<html data-*>` and persists them to `localStorage`.
///
/// Construction is sealed: [`install`] is the only way to get a
/// `UiPrefs`. The signals are exposed via [`UiPrefs::theme`],
/// [`UiPrefs::density`], and [`UiPrefs::rowstyle`] — each returns the
/// underlying [`RwSignal`] so consumers can read with `.get()`, write
/// with `.set()`, and feed into derived signals or effects.
///
/// Stash the returned value with `provide_context` from a top-level
/// component so descendants can pick it up via `use_context`.
#[derive(Debug, Clone, Copy)]
pub struct UiPrefs {
    pub(crate) theme: RwSignal<Theme>,
    pub(crate) density: RwSignal<Density>,
    pub(crate) rowstyle: RwSignal<RowStyle>,
}

impl UiPrefs {
    #[must_use]
    pub fn theme(self) -> RwSignal<Theme> {
        self.theme
    }

    #[must_use]
    pub fn density(self) -> RwSignal<Density> {
        self.density
    }

    #[must_use]
    pub fn rowstyle(self) -> RwSignal<RowStyle> {
        self.rowstyle
    }
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
    let stored = load(storage_key);
    let prefs = UiPrefs {
        theme: RwSignal::new(stored.theme),
        density: RwSignal::new(stored.density),
        rowstyle: RwSignal::new(stored.rowstyle),
    };

    // Track the last value we persisted so we never overwrite storage with
    // what's already there. For the corruption case (load fell back to
    // defaults because the blob was malformed) this leaves the corrupt
    // blob in place — letting the user inspect it and giving the console
    // warning time to register before any change wipes the evidence.
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

fn load(storage_key: &str) -> Stored {
    let Some(storage) = local_storage() else {
        return Stored::default();
    };
    let Ok(Some(raw)) = storage.get_item(storage_key) else {
        return Stored::default();
    };
    let ParseOutcome { stored, warnings } = parse_stored(&raw);
    for w in warnings {
        warn(&format!("fleet-ui: {w}"));
    }
    stored
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

fn warn(msg: &str) {
    web_sys::console::warn_1(&JsValue::from_str(msg));
}
