// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<LoadMore/>` — cursor-driven list footer (issue #33 D3).
//!
//! Canonizes the busy-flag load-more idiom hand-rolled by trawl's intel
//! pages (retired in #112) and six coastwatch pages: a "load more"
//! button while a cursor remains, the canonical `loading…` label
//! (disabled) while a fetch is in flight, and two terminal texts —
//! "end of list" when the cursor runs out, "nothing here yet" when the
//! list is empty. Domain-informative copy stays a prop (`end_text` / `empty_text`);
//! the defaults are the canonical generic strings.
//!
//! A standalone component, not a [`Pager`](crate::pager::Pager) mode —
//! it composes into `Pager`'s trailing-children slot for footer
//! placement, or renders bare (as trawl's story timeline did).
//!
//! The phase resolution is pure ([`phase`]) and native-tested; the
//! wasm-only [`LoadMore`] component renders it.

/// Which of the three terminal shapes (plus the busy button state)
/// [`LoadMore`] renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMorePhase {
    /// The list has no items at all → `empty_text`.
    Empty,
    /// No further pages → `end_text`.
    End,
    /// A cursor remains → the load button, enabled.
    Idle,
    /// A fetch is in flight → the load button, disabled, canonical
    /// `loading…` label.
    Busy,
}

/// Resolve the rendered phase. `empty` wins over everything (an empty
/// list has nothing to page), then exhaustion, then busy.
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

    /// Cursor-driven list footer. `has_more` is typically derived from
    /// `Option<Cursor>::is_some()`; `empty` (default `false`) from the
    /// list length. `busy` disables the button and swaps its label to
    /// the canonical `loading…`. `label` exists for sites whose idle
    /// copy isn't "load more" (trawl's timeline says "load older");
    /// `full` renders the button full-width (`btn-full`).
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
        // Even a lying cursor (empty list, has_more set) renders the
        // empty text — there is nothing beneath the footer to extend.
        assert_eq!(phase(true, true, true), LoadMorePhase::Empty);
        assert_eq!(phase(true, false, false), LoadMorePhase::Empty);
    }

    #[test]
    fn exhausted_cursor_is_the_end_state() {
        assert_eq!(phase(false, false, false), LoadMorePhase::End);
        // busy without a cursor can't render a button — still End.
        assert_eq!(phase(false, false, true), LoadMorePhase::End);
    }

    #[test]
    fn live_cursor_renders_the_button_with_busy_disable() {
        assert_eq!(phase(false, true, false), LoadMorePhase::Idle);
        assert_eq!(phase(false, true, true), LoadMorePhase::Busy);
    }
}
