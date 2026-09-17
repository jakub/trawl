// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wasm-only `localStorage` + `<html data-*>` glue around the pure
//! parsers in [`super::prefs`].
//!
//! The CSS in `styles/fleet-ui.css` reads `[data-theme]` and
//! `[data-rowstyle]` selectors. Writing them on
//! `<html>` (not `<body>`) matches the design spec and keeps the
//! cascade authoritative for `:root` token overrides. Prefs that no
//! global selector reads get no attribute: the sidebar collapse state
//! is a class on `nav.rail`, so it is persisted here and applied there.
//!
//! [`install`] takes a `storage_key` so each consuming app uses its own
//! localStorage namespace — `"trawl.ui"` for trawl-web, `"coastwatch.ui"`
//! for coastwatch-web, and so on.

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};

use super::prefs::{
    Details, ParseOutcome, RowStyle, Rows, Sidebar, Stored, Theme, ThemePreference, parse_stored,
};

/// Reactive UI preferences, a read-only resolved theme, and owner-scoped
/// effects that project appearance and persist preference changes.
///
/// Construction is sealed: [`install`] is the only way to get a
/// `UiPrefs`. [`UiPrefs::theme`] is binary and read-only; select an appearance
/// through [`UiPrefs::select_theme`] and read that choice through
/// [`UiPrefs::theme_preference`]. Reading preferences are exposed via
/// [`UiPrefs::rowstyle`], [`UiPrefs::sidebar`], [`UiPrefs::details`]
/// and [`UiPrefs::rows`] — each returns the underlying [`RwSignal`] so
/// consumers can read with `.get()`, write with `.set()`, and feed into
/// derived signals or effects.
///
/// Stash the returned value with `provide_context` from a top-level
/// component so descendants can pick it up via `use_context`.
#[derive(Debug, Clone, Copy)]
pub struct UiPrefs {
    theme: Signal<Theme>,
    theme_preference: RwSignal<ThemePreference>,
    pub(crate) rowstyle: RwSignal<RowStyle>,
    pub(crate) sidebar: RwSignal<Sidebar>,
    pub(crate) details: RwSignal<Details>,
    pub(crate) rows: RwSignal<Rows>,
}

impl UiPrefs {
    /// The resolved Light or Dark appearance for CSS, charts, and backdrops.
    /// System follows the current OS appearance; fixed preferences ignore it.
    /// Change the preference with [`Self::select_theme`].
    #[must_use]
    pub fn theme(self) -> Signal<Theme> {
        self.theme
    }

    /// The selected preference, independent of System's resolved appearance.
    #[must_use]
    pub fn theme_preference(self) -> Signal<ThemePreference> {
        self.theme_preference.into()
    }

    /// Select an appearance for this session and attempt to persist a changed
    /// preference. Selecting the current choice performs no storage write.
    /// A storage failure does not roll back the session choice and is not
    /// retried by OS changes or by selecting the same preference again.
    pub fn select_theme(self, preference: ThemePreference) {
        if self.theme_preference.get_untracked() != preference {
            self.theme_preference.set(preference);
        }
    }

    #[must_use]
    pub fn rowstyle(self) -> RwSignal<RowStyle> {
        self.rowstyle
    }

    #[must_use]
    pub fn sidebar(self) -> RwSignal<Sidebar> {
        self.sidebar
    }

    #[must_use]
    pub fn details(self) -> RwSignal<Details> {
        self.details
    }

    #[must_use]
    pub fn rows(self) -> RwSignal<Rows> {
        self.rows
    }
}

/// Initialize preference signals from `localStorage` + install the
/// effects that keep `<html>` attributes and persisted preferences in sync.
/// Reads storage and current media state afresh, independently of the early
/// bootstrap. Owns one dark-scheme listener until the current owner is disposed.
/// Initialization and OS changes do not write storage. Missing or invalid
/// stored themes select System; valid legacy Light/Dark values stay fixed.
///
/// `storage_key` is the localStorage key under which a JSON snapshot
/// is persisted — pass `"trawl.ui"`, `"coastwatch.ui"`, etc.
///
/// Call once at app boot from inside the `<App/>` body so the effects and
/// listener belong to that reactive owner. For first styled appearance,
/// also load `js/theme-bootstrap.js` before styles and Wasm with this same
/// key in `data-storage-key`. Runtime installation alone makes no first-paint
/// guarantee. Both paths leave `color-scheme` to CSS.
#[must_use]
pub fn install(storage_key: &'static str) -> UiPrefs {
    let stored = load(storage_key);
    let theme_preference = RwSignal::new(stored.theme);
    let system_dark = install_system_theme();
    let theme = Signal::derive(move || {
        let preference = theme_preference.get();
        // Fixed choices do not subscribe DOM readers to OS changes.
        preference.resolve(preference == ThemePreference::System && system_dark.get())
    });
    let prefs = UiPrefs {
        theme,
        theme_preference,
        rowstyle: RwSignal::new(stored.rowstyle),
        sidebar: RwSignal::new(stored.sidebar),
        details: RwSignal::new(stored.details),
        rows: RwSignal::new(stored.rows),
    };

    // Apply the fresh installation sample immediately, including a changed OS
    // appearance since the bootstrap ran. CSS continues to own color-scheme.
    apply_to_dom(theme.get_untracked(), stored.rowstyle);
    Effect::new(move |_| apply_to_dom(prefs.theme.get(), prefs.rowstyle.get()));

    // This tracks observed session preferences, not successful persistence.
    // Starting at the loaded snapshot preserves malformed raw storage. A failed
    // write is not retried on reselect or an OS change; only another preference
    // change attempts a new write.
    let last_observed = StoredValue::new(stored);

    Effect::new(move |_| {
        let snap = Stored {
            theme: prefs.theme_preference.get(),
            rowstyle: prefs.rowstyle.get(),
            sidebar: prefs.sidebar.get(),
            details: prefs.details.get(),
            rows: prefs.rows.get(),
        };
        if last_observed.with_value(|w| *w != snap) {
            last_observed.set_value(snap);
            write_stored(storage_key, snap);
        }
    });

    prefs
}

/// Register before the final sample so a change during installation cannot
/// leave the runtime stuck on an earlier value. The local handle keeps the
/// callback alive and removes that exact callback before dropping it.
fn install_system_theme() -> RwSignal<bool> {
    let dark = RwSignal::new(false);
    let Some(query) = web_sys::window().and_then(|window| {
        window
            .match_media("(prefers-color-scheme: dark)")
            .ok()
            .flatten()
    }) else {
        return dark;
    };
    let callback_query = query.clone();
    let callback = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        // try_set also makes an already-queued event harmless after disposal.
        let _ = dark.try_set(callback_query.matches());
    });
    if query
        .add_event_listener_with_callback("change", callback.as_ref().unchecked_ref())
        .is_err()
    {
        return dark;
    }
    dark.set(query.matches());
    let listener = StoredValue::new_local(Some((query, callback)));
    on_cleanup(move || {
        listener.update_value(|slot| {
            if let Some((query, callback)) = slot.take() {
                let _ = query.remove_event_listener_with_callback(
                    "change",
                    callback.as_ref().unchecked_ref(),
                );
            }
        });
    });
    dark
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
        warn("fleet-ui: cannot access localStorage; changed preferences apply only this session.");
        return;
    };
    let payload = serde_json::json!({
        "theme":    s.theme.as_attr(),
        "rowstyle": s.rowstyle.as_attr(),
        "sidebar":  s.sidebar.as_attr(),
        "details":  s.details.as_attr(),
        "rows":     s.rows.as_attr(),
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

fn apply_to_dom(theme: Theme, rowstyle: RowStyle) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(html) = doc.document_element() else {
        return;
    };
    let _ = html.set_attribute("data-theme", theme.as_attr());
    let _ = html.set_attribute("data-rowstyle", rowstyle.as_attr());
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

fn warn(msg: &str) {
    web_sys::console::warn_1(&JsValue::from_str(msg));
}
