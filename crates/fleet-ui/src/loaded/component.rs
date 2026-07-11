// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `<Loaded>` render-prop wrapper. Wasm-only (pulls leptos) — the
//! pure [`LoadState`] mapping and the canonical copy live in
//! [`super::state`] so they are native-testable (issue #31 C4).

use leptos::prelude::*;

use super::state::{LoadState, error_copy, loading_copy};

/// Tri-state wrapper: renders the canonical loading / error hints and
/// hands the `Ready` payload to the `render` prop.
///
/// ```ignore
/// <Loaded
///     state=Signal::derive(move || LoadState::from_resource(nets.get()))
///     label="nets"
///     render=Box::new(move |resp| view! { … }.into_any())
/// />
/// ```
///
/// `label` names the resource in the canonical copy (`loading nets…` /
/// `couldn't load nets: …`); omit it for the bare `loading…` form.
/// `error` overrides the error arm for surfaces that deliberately
/// suppress error copy (trawl's facet sidebar and histogram render a
/// quiet `—` because the results table already shows the failure) —
/// the sanctioned C4 "explicit prop override".
#[component]
pub fn Loaded<T>(
    #[prop(into)] state: Signal<LoadState<T>>,
    #[prop(optional)] label: Option<&'static str>,
    render: Box<dyn Fn(T) -> AnyView + Send + Sync>,
    #[prop(optional)] error: Option<Box<dyn Fn(String) -> AnyView + Send + Sync>>,
) -> impl IntoView
where
    T: Clone + Send + Sync + 'static,
{
    view! {
        {move || match state.get() {
            LoadState::Loading => view! {
                <div class="load-hint">{loading_copy(label)}</div>
            }.into_any(),
            LoadState::Error(msg) => match &error {
                Some(render_err) => render_err(msg),
                None => view! {
                    <div class="load-hint error">{error_copy(label, &msg)}</div>
                }.into_any(),
            },
            LoadState::Ready(value) => render(value),
        }}
    }
}
