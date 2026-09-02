// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure tri-state resource state. No `leptos`, no `web_sys` — builds
//! on every target (the [`theme::prefs`](crate::theme::prefs) template)
//! so the loading/error/ready mapping and the canonical copy strings
//! are exercised by native unit tests. The wasm-only
//! [`Loaded`](super::component::Loaded) wrapper renders this state.

use std::fmt::Display;

/// The states every async-fetched surface passes through.
///
/// `LocalResource::get()`'s `Option<Result<T, E>>` maps via
/// [`LoadState::from_resource`].
///
/// [`LoadState::Missing`] models "the resource does not exist" — a 404
/// detail page — as its own state rather than an error: detail pages
/// render it as a neutral hint with explanatory copy, not a failure.
/// Sites opt in via [`LoadState::from_resource_with_missing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState<T> {
    Loading,
    Missing,
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

    /// [`LoadState::from_resource_with`] plus a missing-classifier:
    /// errors for which `is_missing` returns true map to
    /// [`LoadState::Missing`] (the resource doesn't exist — e.g.
    /// `ApiError::Status(404)`), everything else goes through
    /// `render_err` as usual.
    pub fn from_resource_with_missing<E>(
        snapshot: Option<Result<T, E>>,
        is_missing: impl FnOnce(&E) -> bool,
        render_err: impl FnOnce(&E) -> String,
    ) -> Self {
        match snapshot {
            None => Self::Loading,
            Some(Err(e)) if is_missing(&e) => Self::Missing,
            Some(Err(e)) => Self::Error(render_err(&e)),
            Some(Ok(v)) => Self::Ready(v),
        }
    }

    /// Map a manually-tracked fetch — a `loading` flag, an
    /// already-rendered `error` message, and a lazily-evaluated `ready`
    /// value — onto the tri-state. Sites that keep loading/error/data in
    /// separate signals (rather than one `LocalResource`) resolve those
    /// signals, then call this; `error` wins over `loading`, `loading`
    /// over ready. `ready` is only invoked in the ready branch, so stale
    /// data isn't cloned while a fetch is in flight (callers gate
    /// `loading` however they like — e.g. `loading && data.is_empty()` to
    /// keep showing stale rows during a background refresh).
    pub fn from_parts(loading: bool, error: Option<String>, ready: impl FnOnce() -> T) -> Self {
        match error {
            Some(msg) => Self::Error(msg),
            None if loading => Self::Loading,
            None => Self::Ready(ready()),
        }
    }
}

/// Canonical loading copy: `Loading…`, or `Loading nets…` with a label.
#[must_use]
pub fn loading_copy(label: Option<&str>) -> String {
    match label {
        Some(what) => format!("Loading {what}\u{2026}"),
        None => "Loading\u{2026}".to_string(),
    }
}

/// Canonical error copy: `Couldn't load: {msg}`, or
/// `Couldn't load nets: {msg}` with a label.
#[must_use]
pub fn error_copy(label: Option<&str>, msg: &str) -> String {
    match label {
        Some(what) => format!("Couldn't load {what}: {msg}"),
        None => format!("Couldn't load: {msg}"),
    }
}

/// Canonical missing copy: `Not found`, or `story not found` with a
/// label.
#[must_use]
pub fn missing_copy(label: Option<&str>) -> String {
    match label {
        Some(what) => format!("{what} not found"),
        None => "Not found".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{LoadState, error_copy, loading_copy, missing_copy};

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
    fn from_parts_prioritizes_error_then_loading_then_ready() {
        // error wins even while loading.
        let err: LoadState<u32> =
            LoadState::from_parts(true, Some("boom".to_string()), || unreachable!());
        assert_eq!(err, LoadState::Error("boom".to_string()));

        // loading with no error, ready closure left untouched.
        let loading: LoadState<u32> = LoadState::from_parts(true, None, || unreachable!());
        assert_eq!(loading, LoadState::Loading);

        // ready only when neither error nor loading.
        let ready: LoadState<u32> = LoadState::from_parts(false, None, || 7);
        assert_eq!(ready, LoadState::Ready(7));
    }

    #[test]
    fn canonical_copy_strings() {
        assert_eq!(loading_copy(None), "Loading\u{2026}");
        assert_eq!(loading_copy(Some("nets")), "Loading nets\u{2026}");
        assert_eq!(error_copy(None, "boom"), "Couldn't load: boom");
        assert_eq!(error_copy(Some("nets"), "boom"), "Couldn't load nets: boom");
    }

    #[test]
    fn from_resource_with_missing_classifies_the_four_states() {
        // A classifier splits the error domain into "the resource does
        // not exist" (Missing) and real failures (Error, still through
        // the custom renderer).
        let map = |snapshot: Option<Result<u32, u16>>| {
            LoadState::from_resource_with_missing(
                snapshot,
                |&code| code == 404,
                |code| format!("status {code}"),
            )
        };

        assert_eq!(map(None), LoadState::Loading);
        assert_eq!(map(Some(Err(404))), LoadState::Missing);
        assert_eq!(
            map(Some(Err(503))),
            LoadState::Error("status 503".to_string())
        );
        assert_eq!(map(Some(Ok(7))), LoadState::Ready(7));
    }

    #[test]
    fn missing_copy_strings() {
        assert_eq!(missing_copy(None), "Not found");
        assert_eq!(missing_copy(Some("story")), "story not found");
    }
}
