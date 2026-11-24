// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<FacetSidebar/>` — the filter rail (the glossary's term; the code
//! keeps its old name).

use std::collections::HashMap;

use fleet_ui::{Icon, IconView, LoadState, Loaded, SearchInput};
use leptos::prelude::*;
use leptos::{ev, web_sys};
use leptos_use::{
    UseEventListenerOptions, use_event_listener_with_options, use_media_query, use_window,
};
use trawl_api::value::QueryResult;
use wasm_bindgen::JsCast;

use crate::facets::{FACET_TOP_N, RailPage};
use crate::severity_cell::{order_severity_facet, severity_facet_ink, severity_facet_name};
use crate::state::filter_rail::FilterRailChoice;
use crate::state::query::{Filter, FilterOp};

#[component]
#[allow(clippy::too_many_lines)]
pub fn FacetSidebar(
    /// The rows on screen, from whichever source the page's mode makes
    /// active: the snapshot page, or the live ring. Read here for its
    /// loading and error states and its row count, the bars' scale; the
    /// groups come from `page`.
    #[prop(into)]
    state: Signal<LoadState<QueryResult>>,
    #[prop(into)] filters: Signal<Vec<Filter>>,
    #[prop(into)] suppressed: Signal<bool>,
    /// Hide row-derived controls for a pending response, an aggregation,
    /// or an unknown source. URL filter count and Clear all remain usable.
    #[prop(into)]
    rows_suppressed: Signal<bool>,
    #[prop(into)] page: Signal<Option<RailPage>>,
    /// Whether that page is countable, which opens the wide rail in
    /// automatic mode. Separate from `page` so the open state is not
    /// recomputed on every live frame.
    #[prop(into)]
    countable: Signal<Option<bool>>,
    on_add: Callback<Filter>,
    on_clear: Callback<()>,
) -> impl IntoView {
    let collapsed: RwSignal<HashMap<String, bool>> = RwSignal::new(HashMap::new());
    let needle = RwSignal::new(String::new());
    let expanded: RwSignal<HashMap<String, bool>> = RwSignal::new(HashMap::new());

    // 899.98px is the codebase's one spelling of "narrow" (`Shell` and
    // both stylesheets query it), so the disclosure and the layout it
    // presents can never disagree by a pixel.
    let compact = use_media_query("(max-width: 899.98px)");
    let open = RwSignal::new(false);
    let panel = NodeRef::<leptos::html::Details>::new();
    let summary = NodeRef::<leptos::html::Summary>::new();
    // Narrowing with focus in the rail's content opens the disclosure,
    // so the focused control is not hidden inside a closed one. The
    // `<summary>` is the part a closed disclosure still shows, so focus
    // on it leaves the disclosure as it is.
    Effect::new(move |_| {
        if compact.get()
            && let Some(panel) = panel.get()
            && let Some(active) = document().active_element()
            && panel.contains(Some(&active))
            && !summary
                .get()
                .is_some_and(|summary| summary.is_same_node(Some(&active)))
        {
            open.set(true);
        }
    });

    let choice = expect_context::<FilterRailChoice>();
    let wide_open = Memo::new(move |_| {
        choice
            .held()
            .unwrap_or_else(|| countable.get() == Some(true))
    });
    let rail_open = Memo::new(move |_| {
        if compact.get() {
            open.get()
        } else {
            wide_open.get()
        }
    });
    let hint = Memo::new(move |_| page.with(|page| page.as_ref().and_then(RailPage::hint)));
    let body_shown =
        move || !suppressed.get() && !rows_suppressed.get() && countable.get() == Some(true);

    // Whether keyboard focus is somewhere in the rail. Set on the way in,
    // cleared when focus moves to an element outside it or a pointer
    // presses outside it. Focus that drops to `body` with no destination
    // clears nothing: a query unmounts the groups the moment it is sent,
    // which drops a focused group header to `body`, and the rail is still
    // the reader's place until the answer settles.
    let focus_inside = StoredValue::new(false);
    let outside_rail = move |target: Option<web_sys::EventTarget>| {
        target
            .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
            .is_some_and(|target| {
                panel
                    .get_untracked()
                    .is_some_and(|panel| !panel.contains(Some(&target)))
            })
    };
    // A press on content that takes no focus, such as the top bar's
    // title, also leaves `body` active, so the focus events alone cannot
    // tell it from the unmount above. The press itself says the reader
    // went elsewhere. Captured at the window, so no handler that stops
    // the press can hide it; the hook removes the listener with the rail.
    let _ = use_event_listener_with_options(
        use_window(),
        ev::pointerdown,
        move |event: web_sys::PointerEvent| {
            if outside_rail(event.target()) {
                focus_inside.set_value(false);
            }
        },
        UseEventListenerOptions::default().capture(true),
    );
    Effect::new(move |was_open: Option<bool>| {
        let is_open = rail_open.get();
        if was_open == Some(true)
            && !is_open
            && !compact.get_untracked()
            && let Some(panel) = panel.get_untracked()
            && let Some(summary) = summary.get_untracked()
        {
            let document = document();
            // Focus still in the rail is stranded whatever the flag says.
            // The flag only decides whether an idle `body` holds focus
            // that dropped there from the rail.
            let dropped = focus_inside.get_value();
            let stranded = document.active_element().map_or(dropped, |active| {
                panel.contains(Some(&active))
                    || (dropped
                        && document
                            .body()
                            .is_some_and(|body| body.is_same_node(Some(&active))))
            });
            if stranded {
                let _ = summary.focus();
            }
        }
        is_open
    });

    view! {
        <details
            class="facet-panel"
            node_ref=panel
            open=move || rail_open.get()
            // Narrow only: when the DOM disagrees with what the binding last
            // rendered, the browser opened or closed the disclosure by itself
            // (find-in-page opens one to show a match), and the narrow state
            // takes that up, or the next press would look dead. Never write the
            // wide choice here: an automatic open would become a hand choice.
            on:toggle=move |_| {
                if compact.get_untracked()
                    && let Some(panel) = panel.get_untracked()
                {
                    let shown = panel.has_attribute("open");
                    if shown != rail_open.get_untracked() {
                        open.set(shown);
                    }
                }
            }
            on:focusin=move |_| focus_inside.set_value(true)
            on:focusout=move |event: web_sys::FocusEvent| {
                if outside_rail(event.related_target()) {
                    focus_inside.set_value(false);
                }
            }
        >
        <summary
            node_ref=summary
            on:click=move |event: web_sys::MouseEvent| {
                event.prevent_default();
                if compact.get_untracked() {
                    open.update(|open| *open = !*open);
                } else {
                    choice.choose(!wide_open.get_untracked());
                }
            }
        >
            <span class="facet-chev" aria-hidden="true">
                <IconView icon=Icon::Chevron size=12 stroke_width=1.5/>
            </span>
            "Filters"
            <span class="facet-count">{move || {
                let count = filters.get().len();
                if count == 0 || suppressed.get() { String::new() }
                else { format!("{count} active") }
            }}</span>
        </summary>
        // A closed wide rail's content is inert, so the browser cannot
        // open the rail by itself: find-in-page and a link's text fragment
        // show a match inside a closed `<details>` by opening it, which
        // would bypass the hand choice. Inert content is not searched.
        <aside
            class="facets"
            aria-label="Search filters"
            inert=move || !compact.get() && !rail_open.get()
        >
            <div class="phead">
                <Show when=move || !filters.get().is_empty() && !suppressed.get()>
                    <button
                        type="button"
                        class="clear"
                        on:click=move |_| on_clear.run(())
                    >"Clear all"</button>
                </Show>
            </div>
            {move || {
                (!suppressed.get())
                    .then(|| hint.get())
                    .flatten()
                    .map(|hint| view! { <p class="facets-hint">{hint}</p> })
            }}
            <Show when=body_shown>
                <SearchInput value=needle placeholder="Filter field values"/>
            </Show>
            <Show when=body_shown>
            <Loaded
                state=state
                // Deliberate quiet-error override: the results table
                // already reports the query failure, and repeating it in
                // the facet rail is noise.
                error=Box::new(|_| view! { <p class="facets-hint">"—"</p> }.into_any())
                render=Box::new(move |result: QueryResult| {
                    let Some(RailPage::Countable(facets)) = page.get() else {
                        return ().into_any();
                    };
                    // One scale for every bar: the rows the counts come
                    // from, never the capped value list, and taken before
                    // the value search so typing does not rescale them.
                    let rows = result.row_count();
                    let scale = f64::from(u32::try_from(rows).unwrap_or(u32::MAX).max(1));
                    let q = needle.get().to_lowercase();
                    let active = filters.get();
                    let scope_text = if rows == 1 {
                        "Bar = share of 1 shown row".to_owned()
                    } else {
                        format!("Bar = share of {rows} shown rows")
                    };
                    // Taken before the value search: a search narrows
                    // what a group shows, not what the page lists.
                    let capped = facets
                        .iter()
                        .any(|(_, values, distinct)| *distinct > values.len());
                    let scope = view! {
                        <p class="facets-scope">{scope_text}</p>
                        {capped.then(|| view! {
                            <p class="facets-scope">
                                {format!("Lists show up to {FACET_TOP_N} values per field")}
                            </p>
                        })}
                    };
                    let groups = facets.into_iter().map(|(field, values, distinct)| {
                        // Severity values show as names. Only the label
                        // changes: the title, the selection match, the
                        // filter a click builds and the count keep the raw
                        // value. The rail counts input fields only, so no
                        // `sev()` column reaches it.
                        let severity = trawl_core::severity::renders_as_severity(&field, &[]);
                        let label = move |v: &str| {
                            severity
                                .then(|| severity_facet_name(v))
                                .flatten()
                                .map_or_else(|| v.to_owned(), str::to_owned)
                        };
                        // The value search matches what the row shows as
                        // well as the raw value, so `error` finds `17`.
                        let mut filtered: Vec<_> = if q.is_empty() {
                            values
                        } else {
                            values.into_iter()
                                .filter(|(v, _)| {
                                    v.to_lowercase().contains(&q)
                                        || label(v).to_lowercase().contains(&q)
                                })
                                .collect()
                        };
                        // The values were already counted and capped by count.
                        if severity {
                            order_severity_facet(&mut filtered);
                        }
                        let show_all = expanded.get().get(&field).copied().unwrap_or(false);
                        let visible: Vec<_> = if show_all {
                            filtered.clone()
                        } else {
                            filtered.iter().take(5).cloned().collect()
                        };
                        let extra = filtered.len().saturating_sub(visible.len());
                        let is_collapsed = collapsed.get().get(&field).copied().unwrap_or(false);
                        let field_for_toggle = field.clone();
                        let field_for_more = field.clone();
                        let noun = if extra == 1 { "value" } else { "values" };
                        let more_text = format!("Show {extra} more listed {noun}");
                        let more_label = format!("Show {extra} more listed {noun} for {field}");
                        // The heading counts the field's distinct values
                        // on the page, past the listed top values; a value
                        // row's count is rows. The words keep them apart.
                        let count = if distinct == 1 {
                            "1 value".to_owned()
                        } else {
                            format!("{distinct} values")
                        };
                        // The count is a span INSIDE the button, and a
                        // name is the concatenation of what the button
                        // contains: `_time` beside `8 values` read as
                        // `_time8 values`. Naming the button explicitly
                        // puts a comma between the two.
                        let group_label = format!("{field}, {count}");
                        let active = active.clone();
                        view! {
                            <div class="g" class:collapsed=move || is_collapsed>
                                <button
                                    type="button"
                                    class="g-hd"
                                    aria-label=group_label
                                    aria-expanded=move || (!is_collapsed).to_string()
                                    on:click=move |_| collapsed.update(|c| {
                                        let cur = c.get(&field_for_toggle).copied().unwrap_or(false);
                                        c.insert(field_for_toggle.clone(), !cur);
                                    })
                                >
                                    <span class="chev" aria-hidden="true"><IconView icon=Icon::Chevron size=12 stroke_width=1.75/></span>
                                    <span class="name">{field.clone()}</span>
                                    <span class="cnt">{count}</span>
                                </button>
                                <div class="vals">
                                    {visible.into_iter().map(|(v, c)| {
                                        let share = (f64::from(c) / scale).min(1.0);
                                        let title = v.clone();
                                        let shown = label(&v);
                                        let ink = severity.then(|| severity_facet_ink(&v)).flatten();
                                        let state = match_filter_state(&active, &field, &v);
                                        let field_for_inc = field.clone();
                                        let field_for_exc = field.clone();
                                        let value_for_inc = v.clone();
                                        let value_for_exc = v.clone();
                                        let inc_label = format!("Include {field} = {v}");
                                        let exc_label = format!("Exclude {field} = {v}");
                                        view! {
                                            <div
                                                class="v"
                                                class:selected=move || state == FilterState::Included
                                                class:excluded=move || state == FilterState::Excluded
                                            >
                                                <div
                                                    class="bar"
                                                    style=format!("--share: {share:.3}")
                                                ></div>
                                                <span
                                                    class="n"
                                                    class:lvl-ink-error={ink == Some("error")}
                                                    class:lvl-ink-warn={ink == Some("warn")}
                                                    title=title
                                                >{shown}</span>
                                                <span class="c">{c}</span>
                                                <span class="act">
                                                    <button
                                                        type="button"
                                                        class="op"
                                                        aria-label=inc_label
                                                        // The glyph says
                                                        // nothing on sight, so
                                                        // the hover tooltip is
                                                        // the pointer user's
                                                        // only reading of it.
                                                        // The verb alone: the
                                                        // field and value are
                                                        // on the row already.
                                                        title="Include"
                                                        on:click=move |_| {
                                                            on_add.run(Filter {
                                                                field: field_for_inc.clone(),
                                                                value: value_for_inc.clone(),
                                                                op: FilterOp::Include,
                                                            });
                                                        }
                                                    ><span aria-hidden="true">"+"</span></button>
                                                    <button
                                                        type="button"
                                                        class="op"
                                                        aria-label=exc_label
                                                        title="Exclude"
                                                        on:click=move |_| {
                                                            on_add.run(Filter {
                                                                field: field_for_exc.clone(),
                                                                value: value_for_exc.clone(),
                                                                op: FilterOp::Exclude,
                                                            });
                                                        }
                                                    ><span aria-hidden="true">"⊘"</span></button>
                                                </span>
                                            </div>
                                        }
                                    }).collect::<Vec<_>>()}
                                    {(extra > 0).then(|| view! {
                                        <button
                                            type="button"
                                            class="more"
                                            aria-label=more_label
                                            on:click=move |_| expanded.update(|e| {
                                                e.insert(field_for_more.clone(), true);
                                            })
                                        >
                                            {more_text}
                                        </button>
                                    })}
                                </div>
                            </div>
                        }
                    }).collect::<Vec<_>>();
                    (scope, groups).into_any()
                })
            />
            </Show>
        </aside>
        </details>
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterState {
    None,
    Included,
    Excluded,
}

fn match_filter_state(active: &[Filter], field: &str, value: &str) -> FilterState {
    for f in active {
        if f.field == field && f.value == value {
            return match f.op {
                FilterOp::Include => FilterState::Included,
                FilterOp::Exclude => FilterState::Excluded,
            };
        }
    }
    FilterState::None
}
