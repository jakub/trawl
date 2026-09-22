// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<QueryErrorNotice/>` — what the results region shows in place of the
//! table or chart when the query itself is wrong (ADR-0039).
//!
//! It is the one renderer for a snapshot refusal on Events and on
//! Visualization and for a live stream whose sent text does not parse.
//! It offers no Retry and no other control: sending the same text again
//! earns the same answer, and the editor holding the text is right there.
//!
//! Every string it shows is server-written or quotes the query text, so
//! all of it renders as text nodes, never as markup. The excerpt quotes
//! the text that was sent, which [`NoticeModel`] was built against; the
//! caret line under it is decoration for sighted readers and hidden from
//! assistive technology, which reads the message instead.

use leptos::prelude::*;

use crate::query_error::{Block, Excerpt, NoticeModel};

/// The notice for one refused query.
#[component]
pub fn QueryErrorNotice(
    /// The headline and the per-detail blocks, already resolved against
    /// the sent text.
    model: NoticeModel,
) -> impl IntoView {
    let NoticeModel { headline, blocks } = model;
    view! {
        <div class="query-error" role="alert">
            <p class="query-error-lead">{headline}</p>
            {blocks.into_iter().map(block).collect_view()}
        </div>
    }
}

/// One detail: its message when the headline is a count, and the sent
/// line under its span when the span indexes that text.
fn block(Block { message, excerpt }: Block) -> impl IntoView {
    view! {
        <div class="query-error-block">
            {message.map(|m| view! { <p class="query-error-message">{m}</p> })}
            {excerpt.map(excerpt_view)}
        </div>
    }
}

fn excerpt_view(
    Excerpt {
        prefix,
        text,
        caret,
    }: Excerpt,
) -> impl IntoView {
    view! {
        <figure class="query-error-excerpt">
            <figcaption class="query-error-caption">"Query sent to server"</figcaption>
            <pre class="query-error-text">
                {(!prefix.is_empty())
                    .then(|| view! { <span class="query-error-line">{prefix}</span> })}
                {text}
                "\n"
                <span class="query-error-caret" aria-hidden="true">{caret}</span>
            </pre>
        </figure>
    }
}
