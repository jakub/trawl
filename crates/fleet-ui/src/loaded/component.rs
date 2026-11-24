// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::prelude::*;

use super::state::{LoadState, error_copy, loading_copy, missing_copy};

#[component]
pub fn Loaded<T>(
    #[prop(into)] state: Signal<LoadState<T>>,
    #[prop(optional)] label: Option<&'static str>,
    render: Box<dyn Fn(T) -> AnyView + Send + Sync>,
    #[prop(optional)] error: Option<Box<dyn Fn(String) -> AnyView + Send + Sync>>,
    #[prop(optional)] retry: Option<Callback<()>>,
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
                    <div class="load-hint error">
                        <div>{error_copy(label, &msg)}</div>
                        {retry.map(|on_retry| view! {
                            <div class="load-recovery">
                                <crate::Btn variant=crate::Variant::Secondary on_click=on_retry>"Retry"</crate::Btn>
                            </div>
                        })}
                    </div>
                }.into_any(),
            },
            LoadState::Ready(value) => render(value),
        }}
    }
}
