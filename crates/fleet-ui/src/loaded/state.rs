// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt::Display;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState<T> {
    Loading,
    Missing,
    Error(String),
    Ready(T),
}

impl<T> LoadState<T> {
    pub fn from_resource<E: Display>(snapshot: Option<Result<T, E>>) -> Self {
        Self::from_resource_with(snapshot, std::string::ToString::to_string)
    }

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

    pub fn from_parts(loading: bool, error: Option<String>, ready: impl FnOnce() -> T) -> Self {
        match error {
            Some(msg) => Self::Error(msg),
            None if loading => Self::Loading,
            None => Self::Ready(ready()),
        }
    }
}

#[must_use]
pub fn loading_copy(label: Option<&str>) -> String {
    match label {
        Some(what) => format!("Loading {what}\u{2026}"),
        None => "Loading\u{2026}".to_string(),
    }
}

#[must_use]
pub fn error_copy(label: Option<&str>, msg: &str) -> String {
    match label {
        Some(what) => format!("Couldn't load {what}: {msg}"),
        None => format!("Couldn't load: {msg}"),
    }
}

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
        let err: LoadState<u32> =
            LoadState::from_parts(true, Some("boom".to_string()), || unreachable!());
        assert_eq!(err, LoadState::Error("boom".to_string()));

        let loading: LoadState<u32> = LoadState::from_parts(true, None, || unreachable!());
        assert_eq!(loading, LoadState::Loading);

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
