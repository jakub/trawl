// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ResourceNotice/>` — what the results region shows in place of the
//! table or chart when the server answered a snapshot with
//! `query_resource_exhausted` (ADR-0056).
//!
//! The query ran and used up the memory or scratch space trawld gives
//! queries. The text is fine, so nothing is underlined; the server ran
//! nothing to completion, so an empty table would claim there is nothing
//! to find; and "Couldn't load results" would read as a fault the server
//! does not have. The notice names the state and quotes the server's
//! sentence, which says what helps: a narrower range or a simpler query.
//! The sentence is fixed and carries no path, size or engine text; it
//! renders as a text node all the same.
//!
//! Like a query error, it offers no Retry: the same request spends the
//! same budget again. The reader changes the range or the text and
//! hauls.

use leptos::prelude::*;

/// The notice for one refused snapshot.
#[component]
pub fn ResourceNotice(
    /// The server's sentence, verbatim.
    message: String,
) -> impl IntoView {
    view! {
        <div class="resource-notice" role="alert">
            <p class="resource-notice-title">"Query ran out of resources"</p>
            <p class="resource-notice-text">{message}</p>
        </div>
    }
}
