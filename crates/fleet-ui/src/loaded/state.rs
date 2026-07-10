// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure tri-state resource state. No `leptos`, no `web_sys` — builds
//! on every target (the [`theme::prefs`](crate::theme::prefs) template)
//! so the loading/error/ready mapping and the canonical copy strings
//! are exercised by native unit tests (issue #31 C4). The wasm-only
//! [`Loaded`](super::component::Loaded) wrapper renders this state.

use std::fmt::Display;

/// The three states every async-fetched surface passes through.
/// Trawl's 22 hand-rolled `match resource.get()` blocks collapse onto
/// this enum; `LocalResource::get()`'s `Option<Result<T, E>>` maps via
/// [`LoadState::from_resource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState<T> {
    Loading,
    Error(String),
    Ready(T),
}

impl<T> LoadState<T> {
    /// Map a leptos `LocalResource::get()` snapshot: `None` while the
    /// fetch is in flight, `Some(Err)` stringified via `Display`,
    /// `Some(Ok)` ready.
    pub fn from_resource<E: Display>(snapshot: Option<Result<T, E>>) -> Self {
        Self::from_resource_with(snapshot, std::string::ToString::to_string)
    }

    /// [`LoadState::from_resource`] with a custom error renderer, for
    /// sites that translate error variants into friendlier copy (e.g.
    /// HTTP 404 → "story not found").
    pub fn from_resource_with<E>(
        snapshot: Option<Result<T, E>>,
        render_err: impl FnOnce(&E) -> String,
    ) -> Self {
        match snapshot {
            None => Self::Loading,
            Some(Err(e)) => Self::Error(render_err(&e)),
            Some(Ok(v)) => Self::Ready(v),
        }
    }
}

/// Canonical loading copy: `loading…`, or `loading nets…` with a label.
#[must_use]
pub fn loading_copy(label: Option<&str>) -> String {
    match label {
        Some(what) => format!("loading {what}\u{2026}"),
        None => "loading\u{2026}".to_string(),
    }
}

/// Canonical error copy: `couldn't load: {msg}`, or
/// `couldn't load nets: {msg}` with a label.
#[must_use]
pub fn error_copy(label: Option<&str>, msg: &str) -> String {
    match label {
        Some(what) => format!("couldn't load {what}: {msg}"),
        None => format!("couldn't load: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{LoadState, error_copy, loading_copy};

    #[test]
    fn from_resource_maps_the_three_states() {
        let loading: LoadState<u32> = LoadState::from_resource(None::<Result<u32, String>>);
        assert_eq!(loading, LoadState::Loading);

        let err: LoadState<u32> = LoadState::from_resource(Some(Err("boom")));
        assert_eq!(err, LoadState::Error("boom".to_string()));

        let ready: LoadState<u32> = LoadState::from_resource(Some(Ok::<_, String>(7)));
        assert_eq!(ready, LoadState::Ready(7));
    }

    #[test]
    fn from_resource_with_customizes_error_copy() {
        let err: LoadState<u32> =
            LoadState::from_resource_with(Some(Err(404_u16)), |code| match code {
                404 => "story not found".to_string(),
                other => format!("error: {other}"),
            });
        assert_eq!(err, LoadState::Error("story not found".to_string()));
    }

    #[test]
    fn canonical_copy_strings() {
        assert_eq!(loading_copy(None), "loading\u{2026}");
        assert_eq!(loading_copy(Some("nets")), "loading nets\u{2026}");
        assert_eq!(error_copy(None, "boom"), "couldn't load: boom");
        assert_eq!(error_copy(Some("nets"), "boom"), "couldn't load nets: boom");
    }
}
