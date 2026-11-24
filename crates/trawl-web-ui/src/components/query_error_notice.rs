// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<QueryErrorNotice/>` — what the results region shows in place of the
//! table or chart when the query itself is wrong (ADR-0039).

use leptos::prelude::*;

use crate::query_error::{Block, Excerpt, NoticeModel};

#[component]
pub fn QueryErrorNotice(model: NoticeModel) -> impl IntoView {
    let NoticeModel {
        verdict,
        detail,
        blocks,
    } = model;
    let excerpt_first = detail.is_some()
        && matches!(
            blocks.as_slice(),
            [Block {
                message: None,
                excerpt: Some(_),
            }]
        );
    let (lead_detail, diagnostic) = if excerpt_first {
        (None, detail)
    } else {
        (detail, None)
    };
    view! {
        <div class="query-error" role="alert">
            <p class="query-error-lead">
                <span class="query-error-verdict">{verdict}</span>
                {lead_detail
                    .map(|d| {
                        view! {
                            ": "
                            <span class="query-error-detail">{d}</span>
                        }
                    })}
            </p>
            {blocks.into_iter().map(block).collect_view()}
            {diagnostic.map(|d| view! { <p class="query-error-diagnostic">{d}</p> })}
            <p class="query-error-next">"Edit the query above, then Haul again."</p>
        </div>
    }
}

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
        pad,
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
                <span class="query-error-caret" aria-hidden="true">
                    <span class="query-error-caret-pad">{pad}</span>
                    {caret}
                </span>
            </pre>
        </figure>
    }
}
