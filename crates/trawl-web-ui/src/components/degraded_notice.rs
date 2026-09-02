// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<DegradedNotice/>` — the incomplete-results notice, the SPA's
//! rendering of `QueryResponse.degraded_fields` (ADR-0011).
//!
//! Everything here is a fact about one execution. The fields are the
//! ones the server said that query bound; nothing is re-derived from the
//! DSL, and no catalog state is consulted afterwards — a repin that
//! retires a badge does not edit a notice already on screen, because the
//! results under it were still produced by the old pin.
//!
//! Nothing renders when the list is empty, which is the ordinary case,
//! and nothing renders for the live tail: SSE carries no notice, so the
//! search page hands this component an empty list in live mode rather
//! than the last snapshot's.
//!
//! Field names are client-chosen text: they render in leptos text
//! positions through `sanitize_display_text`, while the exact original
//! spelling is what gets percent-encoded into the case-file link.

use leptos::prelude::*;
use trawl_core::sanitize::sanitize_display_text;

use crate::api;
use crate::components::enc_uri;
use crate::notice_key::degraded_notice_key;
use crate::perms::can_schema_read;

/// One line over the results: which fields are shelving values, and
/// where to read the case file.
#[component]
pub fn DegradedNotice(
    /// The effective query the fields belong to — half of the dismissal
    /// identity, never displayed.
    #[prop(into)]
    query: Signal<String>,
    /// `QueryResponse.degraded_fields`, verbatim.
    #[prop(into)]
    fields: Signal<Vec<String>>,
) -> impl IntoView {
    // The case file is `schema_read`-gated, so without it the link would
    // be a 403 dressed as a remedy. The names still show: knowing which
    // field is incomplete is the useful half, and it needs no permission
    // the query itself did not.
    let me = use_context::<RwSignal<Option<api::MeResponse>>>();
    let can_link = Signal::derive(move || {
        me.and_then(|me| me.get())
            .is_some_and(|me| can_schema_read(&me.permissions))
    });

    // Dismissal is keyed, not boolean: the same notice paged through
    // stays dismissed, a different query or a changed field set is a
    // different notice and comes back.
    let dismissed = RwSignal::new(None::<String>);
    let key = Memo::new(move |_| {
        fields.with(|f| (!f.is_empty()).then(|| degraded_notice_key(&query.get(), f)))
    });
    let showing = Signal::derive(move || key.with(|k| k.is_some() && *k != dismissed.get()));

    view! {
        // The live region outlives the notice inside it: a region that
        // mounts with its content is not reliably announced, so this
        // wrapper stays in the tree and collapses to nothing when empty.
        <div class="deg-live" aria-live="polite">
            <Show when=move || showing.get()>
                <div class="deg-notice">
                    <span class="deg-text">
                        "Results may be incomplete \u{2014} a type conflict has been shelving \
                         values for "
                        {move || field_names(&fields.get(), can_link.get())}
                        ", so rows carrying a value the pin could not keep read as empty here."
                        <Show when=move || can_link.get()>
                            " Open a name for its case file."
                        </Show>
                    </span>
                    <button
                        type="button"
                        class="deg-x"
                        title="Dismiss this notice"
                        aria-label="Dismiss this notice"
                        on:click=move |_| dismissed.set(key.get_untracked())
                    >
                        "\u{00d7}"
                    </button>
                </div>
            </Show>
        </div>
    }
}

/// The field names as a comma-separated run — each one a case-file link
/// when the session may read the catalog, plain text when it may not.
fn field_names(fields: &[String], link: bool) -> Vec<AnyView> {
    let last = fields.len().saturating_sub(1);
    fields
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let shown = sanitize_display_text(name);
            let sep = (i < last).then_some(", ");
            if link {
                let href = format!("/search/schema?field={}", enc_uri(name));
                view! { <a class="link mono" href=href>{shown}</a>{sep} }.into_any()
            } else {
                view! { <span class="mono">{shown}</span>{sep} }.into_any()
            }
        })
        .collect()
}
