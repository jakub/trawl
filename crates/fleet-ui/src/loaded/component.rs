// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `<Loaded>` render-prop wrapper. Wasm-only (pulls leptos) — the
//! pure [`LoadState`] mapping and the canonical copy live in
//! [`super::state`] so they are native-testable.

use leptos::prelude::*;

use super::state::{LoadState, error_copy, loading_copy, missing_copy};

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
/// `couldn't load nets: …` / `nets not found`); omit it for the bare
/// forms. `error` overrides the error arm for surfaces that
/// deliberately suppress error copy (trawl's facet sidebar and
/// histogram render a quiet `—` because the results table already
/// shows the failure). `missing` likewise overrides the
/// [`LoadState::Missing`] arm; the default renders the canonical "not
/// found" copy on the neutral `.load-hint` tone (a missing resource is
/// not a failure), with `missing_subtitle` as an optional explanatory
/// second line. `missing_subtitle` decorates only that default arm:
/// pass a custom `missing` closure and the subtitle is ignored (render
/// it yourself inside the closure).
#[component]
pub fn Loaded<T>(
    #[prop(into)] state: Signal<LoadState<T>>,
    #[prop(optional)] label: Option<&'static str>,
    render: Box<dyn Fn(T) -> AnyView + Send + Sync>,
    #[prop(optional)] error: Option<Box<dyn Fn(String) -> AnyView + Send + Sync>>,
    #[prop(optional)] missing: Option<Box<dyn Fn() -> AnyView + Send + Sync>>,
    /// Optional second line under the default "not found" copy;
    /// ignored when a custom `missing` closure is supplied.
    #[prop(optional, into)]
    missing_subtitle: Option<String>,
) -> impl IntoView
where
    T: Clone + Send + Sync + 'static,
{
    view! {
        {move || match state.get() {
            LoadState::Loading => view! {
                <div class="load-hint">{loading_copy(label)}</div>
            }.into_any(),
            LoadState::Missing => match &missing {
                Some(render_missing) => render_missing(),
                None => view! {
                    <div class="load-hint">
                        {missing_copy(label)}
                        {missing_subtitle.clone().map(|sub| view! {
                            <div class="load-sub">{sub}</div>
                        })}
                    </div>
                }.into_any(),
            },
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
