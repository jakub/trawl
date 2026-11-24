// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<LoadMore/>` — cursor-driven list footer.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMorePhase {
    Empty,
    End,
    Idle,
    Busy,
}

#[must_use]
pub fn phase(empty: bool, has_more: bool, busy: bool) -> LoadMorePhase {
    if empty {
        LoadMorePhase::Empty
    } else if !has_more {
        LoadMorePhase::End
    } else if busy {
        LoadMorePhase::Busy
    } else {
        LoadMorePhase::Idle
    }
}

#[cfg(target_arch = "wasm32")]
mod component {
    use leptos::prelude::*;

    use super::{LoadMorePhase, phase};
    use crate::button::{Btn, Variant};
    use crate::loaded::state::loading_copy;

    #[component]
    pub fn LoadMore(
        #[prop(into)] has_more: Signal<bool>,
        #[prop(into)] busy: Signal<bool>,
        on_load: Callback<()>,
        #[prop(into, optional)] empty: Signal<bool>,
        #[prop(into, default = String::from("End of list"))] end_text: String,
        #[prop(into, default = String::from("Nothing here yet"))] empty_text: String,
        #[prop(into, default = String::from("Load more"))] label: String,
        #[prop(default = false)] full: bool,
    ) -> impl IntoView {
        view! {
            <div class="load-more">
                {move || match phase(empty.get(), has_more.get(), busy.get()) {
                    LoadMorePhase::Empty => view! {
                        <span class="load-more-end">{empty_text.clone()}</span>
                    }.into_any(),
                    LoadMorePhase::End => view! {
                        <span class="load-more-end">{end_text.clone()}</span>
                    }.into_any(),
                    LoadMorePhase::Idle | LoadMorePhase::Busy => {
                        let label = label.clone();
                        view! {
                            <Btn
                                variant=Variant::Secondary
                                disabled=busy
                                full=full
                                on_click=on_load
                            >
                                {move || if busy.get() { loading_copy(None) } else { label.clone() }}
                            </Btn>
                        }.into_any()
                    }
                }}
            </div>
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use component::LoadMore;

#[cfg(test)]
mod tests {
    use super::{LoadMorePhase, phase};

    #[test]
    fn empty_wins_over_everything() {
        assert_eq!(phase(true, true, true), LoadMorePhase::Empty);
        assert_eq!(phase(true, false, false), LoadMorePhase::Empty);
    }

    #[test]
    fn exhausted_cursor_is_the_end_state() {
        assert_eq!(phase(false, false, false), LoadMorePhase::End);
        assert_eq!(phase(false, false, true), LoadMorePhase::End);
    }

    #[test]
    fn live_cursor_renders_the_button_with_busy_disable() {
        assert_eq!(phase(false, true, false), LoadMorePhase::Idle);
        assert_eq!(phase(false, true, true), LoadMorePhase::Busy);
    }
}
