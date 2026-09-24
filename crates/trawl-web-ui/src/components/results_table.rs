// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ResultsTable/>` — paginated snapshot results table.
//!
//! Sans-serif column headers with a sort affordance, expandable rows that
//! reveal a `_time` / field tag detail panel with Copy _raw / Show context /
//! Find similar action buttons. Detail-row tag clicks add filters through a
//! parent-supplied callback, paired with the executed query they belong to,
//! and every row action is gated on that query's field provenance
//! ([`Capabilities`]): a `let` or a rename can leave a field that no raw
//! event carries, and an aggregation has no raw row to copy.
//!
//! Two optional reading modes ride on top, both off by default so the
//! markup below is exactly the pre-ADR-0032 table until a reader turns
//! one on:
//!
//! - `details == Inspector` moves the expanded row into a panel docked
//!   beside the table ([`fleet_ui::Drawer`] in its docked presentation).
//!   The selection is a `(generation, original row index)` pair owned by
//!   the page, so a new response or a page turn closes the panel rather
//!   than repointing it at a different event, and `j`/`k` walk the
//!   SORTED order while never changing which event is selected.
//! - `rows == MessageFirst` reduces the columns to time, severity and
//!   message, with the rest of the row as a muted secondary line.
//!
//! Both detail presentations fold an event's null fields behind one
//! "Show N null fields" disclosure ([`partition_detail_fields`]). Its
//! open state is one signal owned by [`ResultsTable`], so it survives
//! selecting another event and re-mounting the inspector, and it is not
//! in the URL.

use crate::api::PAGE_SIZE;
use crate::context_query::SearchNavigation;
use crate::context_query::{build_context_query, escape_dq, find_col};
use crate::result_actions::{Capabilities, compare};
use crate::results_layout::{
    MessageFirst, inspector_selection, message_first, null_fields_label, partition_detail_fields,
};
use crate::state::query::{Filter, FilterOp};
use crate::state::search_session::{ExecutedFailure, ExecutedQuery, ExecutedResponse};
use fleet_ui::overlay::has_layers;
use fleet_ui::{
    Btn, CopyButton, Details, Drawer, LoadState, Loaded, OffsetPager, PageTotal, PageWindow, Rows,
    ToastBus, ToastKind, Variant,
};
use leptos::prelude::*;
use leptos::web_sys;
use trawl_api::QueryResponse;
use trawl_api::display::value_to_string;
use wasm_bindgen::JsCast as _;

use crate::severity_cell::{severity_class, severity_columns, severity_display};
use trawl_api::value::Value;

#[component]
pub fn ResultsTable(
    #[prop(into)] page: Signal<usize>,
    rows: LocalResource<Result<ExecutedResponse, ExecutedFailure>>,
    #[prop(into)] busy: Signal<bool>,
    /// Called with the new page index when prev/next is clicked. Parent
    /// captures a router navigator and translates to URL navigation.
    on_paginate: Callback<usize>,
    /// Called when a detail-row field tag is clicked — adds an include
    /// filter for that `field = value`, against the query that produced
    /// the row.
    on_add_filter: Callback<(ExecutedQuery, Filter)>,
    /// Navigate to a fresh snapshot search with its own query and range. Used by "Show context" and "Find similar".
    on_navigate: Callback<SearchNavigation>,
    /// Where a row's fields are read: in place, or in the docked
    /// inspector. `Inline` is the default and renders today's markup.
    #[prop(into)]
    details: Signal<Details>,
    /// Compact column table, or message-first rows. `Compact` is the
    /// default and renders today's markup.
    #[prop(into)]
    rows_mode: Signal<Rows>,
    /// The inspector's selection, owned by the page: only the page knows
    /// when a new response, a page turn or a new effective query has
    /// landed under it.
    selected: RwSignal<Option<(u64, usize)>>,
    /// The response generation that selection is keyed on.
    #[prop(into)]
    generation: Signal<u64>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    // The traversal order `j`/`k` walk: the ORIGINAL row indices in the
    // order the current sort renders them. Written by the body, read
    // here, and deliberately not reactive — a sort re-order must move
    // where the next keystroke goes, never move the selection itself.
    let order = StoredValue::new(Vec::<usize>::new());

    let inspector_row = Memo::new(move |_| {
        if details.get() == Details::Inspector {
            inspector_selection(generation.get(), selected.get())
        } else {
            None
        }
    });

    let on_keydown = inspector_keys(details, selected, generation, order);
    // The null-field disclosure, shared by the inline expansion and the
    // inspector for the life of this results view.
    let show_nulls = RwSignal::new(false);

    view! {
        <div class="results-split" class:has-inspector=move || inspector_row.get().is_some()>
            <fleet_ui::OverflowHint viewport=table_viewport/>
            <div
                node_ref=table_viewport
                id="search-results"
                class="results"
                role="region"
                aria-label="Search results"
                tabindex="0"
                on:keydown=on_keydown
            >
                <Loaded
                    state=Signal::derive(move || LoadState::from_resource(rows.get()))
                    label="results"
                    retry=Callback::new(move |()| { rows.set(None); rows.refetch(); })
                    render=Box::new(move |resp: ExecutedResponse| view! {
                        <ResultsTableBody
                            resp=resp.response
                            executed_query=resp.query
                            page=page
                            busy=busy
                            on_paginate=on_paginate
                            on_add_filter=on_add_filter
                            on_navigate=on_navigate
                            bus=bus
                            details=details
                            rows_mode=rows_mode
                            selected=selected
                            generation=generation
                            order=order
                            show_nulls=show_nulls
                        />
                    }.into_any())
                />
            </div>
            {move || {
                let idx = inspector_row.get()?;
                let resp = rows.get()?.ok()?;
                let columns: Vec<String> =
                    resp.response.result.columns.iter().map(|c| c.name.clone()).collect();
                let row = resp.response.result.rows.get(idx)?.clone();
                let capabilities = Capabilities::for_query(&resp.query.effective);
                Some(view! {
                    <InspectorPanel
                        idx=idx
                        row=row
                        columns=columns
                        executed_query=resp.query
                        capabilities=capabilities
                        on_add_filter=on_add_filter
                        on_navigate=on_navigate
                        on_close=Callback::new(move |()| selected.set(None))
                        bus=bus
                        show_nulls=show_nulls
                    />
                })
            }}
        </div>
    }
}

/// Escape and `j`/`k` for the results region while the inspector is the
/// reading mode.
///
/// `j`/`k` walk the SORTED order (`order`, republished on every re-sort)
/// but address rows by their ORIGINAL index, so a re-sort moves where
/// the next keystroke goes without moving the selection itself. A
/// keystroke from anywhere but the region or one of its row controls is
/// ignored: a letter typed into a field is not a navigation.
fn inspector_keys(
    details: Signal<Details>,
    selected: RwSignal<Option<(u64, usize)>>,
    generation: Signal<u64>,
    order: StoredValue<Vec<usize>>,
) -> impl Fn(web_sys::KeyboardEvent) + 'static {
    move |ev: web_sys::KeyboardEvent| {
        if details.get_untracked() != Details::Inspector
            || ev.ctrl_key()
            || ev.alt_key()
            || ev.meta_key()
            || ev.shift_key()
        {
            return;
        }
        let key = ev.key();
        if key == "Escape" {
            // A menu, modal or popover stacked over the page owns Escape
            // first; the selection is page furniture underneath it.
            if !has_layers() && selected.get_untracked().is_some() {
                ev.prevent_default();
                selected.set(None);
            }
            return;
        }
        if key != "j" && key != "k" {
            return;
        }
        // The region itself or one of its row controls — never a field
        // that swallows letters, and never a control outside the table.
        let from_region = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| {
                el.class_list().contains("results")
                    || el.closest(".row-stretch").ok().flatten().is_some()
            });
        if !from_region {
            return;
        }
        let ord = order.get_value();
        let Some(last) = ord.len().checked_sub(1) else {
            return;
        };
        ev.prevent_default();
        let cur_gen = generation.get_untracked();
        let at = inspector_selection(cur_gen, selected.get_untracked())
            .and_then(|idx| ord.iter().position(|&o| o == idx));
        let next = match (at, key.as_str()) {
            (None, "j") => 0,
            (None, _) => last,
            (Some(pos), "j") => (pos + 1).min(last),
            (Some(pos), _) => pos.saturating_sub(1),
        };
        selected.set(Some((cur_gen, ord[next])));
    }
}

/// The docked inspector: the expanded row's key/value grid and its three
/// actions, rendered beside the table instead of inside it.
///
/// A [`Drawer`] in its docked presentation (ADR-0032), so the header,
/// close affordance and body scroll are the fleet's, not a second
/// hand-rolled panel. It registers no overlay layer, which is what keeps
/// the command palette's chord live while a row is open. The same
/// provenance gates apply as inline: a field the executed query changed,
/// or an instant or the raw event ([`Capabilities::detail_filter`]), gets
/// no filter buttons, and a row with no raw source gets no raw actions.
#[component]
fn InspectorPanel(
    idx: usize,
    row: Vec<Value>,
    columns: Vec<String>,
    executed_query: ExecutedQuery,
    capabilities: Capabilities,
    on_add_filter: Callback<(ExecutedQuery, Filter)>,
    on_navigate: Callback<SearchNavigation>,
    on_close: Callback<()>,
    bus: ToastBus,
    show_nulls: RwSignal<bool>,
) -> impl IntoView {
    let time_text = find_col(&columns, &["_time"])
        .and_then(|i| row.get(i))
        .map(value_to_string);
    let raw_actions = capabilities.raw_actions();
    let row_for_actions = row.clone();
    let columns_for_actions = columns.clone();
    let heading = format!("Event {}", idx + 1);
    // `zip` semantics: a short row renders the cells it has, no more.
    let fields = partition_detail_fields(&row[..row.len().min(columns.len())]);
    let null_count = fields.null.len();

    view! {
        <Drawer
            docked=true
            tabs=vec![]
            tabs_label="Event details"
            label=heading.clone()
            active_tab=Signal::derive(String::new)
            on_tab_change=Callback::new(|_: String| ())
            on_close=on_close
            close_size=12
            panel_id="search-inspector"
            panel_class="inspector"
            title=Box::new(move || view! {
                <span class="name">{heading}</span>
                {time_text.map(|t| view! { <span class="sub">{t}</span> })}
            }.into_any())
        >
            <div class="dg" id=INSPECTOR_FIELDS_ID>
                {move || fields.visible(show_nulls.get()).into_iter().map(|i| {
                    let (name, v) = (&columns[i], &row[i]);
                    let key = name.clone();
                    let value_text = value_to_string(v);
                    let copy_text = value_text.clone();
                    let allowed = capabilities.detail_filter(name, v);
                    let inc = Filter {
                        field: name.clone(),
                        value: value_text.clone(),
                        op: FilterOp::Include,
                    };
                    let exc = Filter {
                        field: name.clone(),
                        value: value_text.clone(),
                        op: FilterOp::Exclude,
                    };
                    let inc_label = format!("Include {name} = {value_text}");
                    let exc_label = format!("Exclude {name} = {value_text}");
                    let query_for_inc = executed_query.clone();
                    let query_for_exc = executed_query.clone();
                    view! {
                        <span class="k">{key}</span>
                        <span class="v">{value_text}</span>
                        <span class="kv-act">
                            // The visible word says what the press does;
                            // the field and value it acts on live in the
                            // accessible name, because three identical
                            // "Include" buttons down a column are only
                            // told apart by the row they sit in.
                            {allowed.then(|| view! {
                                <button
                                    type="button"
                                    class="tag"
                                    aria-label=inc_label
                                    on:click=move |_| on_add_filter.run((query_for_inc.clone(), inc.clone()))
                                >"Include"</button>
                                <button
                                    type="button"
                                    class="tag"
                                    aria-label=exc_label
                                    on:click=move |_| on_add_filter.run((query_for_exc.clone(), exc.clone()))
                                >"Exclude"</button>
                            })}
                            <CopyButton
                                class="tag"
                                text=Signal::derive(move || copy_text.clone())
                                success_detail="Value copied to clipboard."
                            >"Copy"</CopyButton>
                        </span>
                    }
                }).collect::<Vec<_>>()}
            </div>
            <NullFieldsToggle count=null_count open=show_nulls controls=INSPECTOR_FIELDS_ID.to_owned()/>
            {raw_actions.then(|| view! {
                <div class="actions">
                    <CopyRawButton
                        row=row_for_actions.clone()
                        columns=columns_for_actions.clone()
                    />
                    <ShowContextButton
                        row=row_for_actions.clone()
                        columns=columns_for_actions.clone()
                        on_navigate=on_navigate
                        bus=bus
                    />
                    <FindSimilarButton
                        row=row_for_actions
                        columns=columns_for_actions
                        on_navigate=on_navigate
                        bus=bus
                    />
                </div>
            })}
        </Drawer>
    }
}

/// The inspector's field grid, which its null-field disclosure controls.
const INSPECTOR_FIELDS_ID: &str = "search-inspector-fields";

/// The "Show N null fields" disclosure under a detail grid: a real button
/// with `aria-expanded`, so Enter and Space work natively, and nothing at
/// all when the event has no null field. Hidden null rows are not
/// rendered, so they are not keyboard stops either.
#[component]
fn NullFieldsToggle(count: usize, open: RwSignal<bool>, controls: String) -> impl IntoView {
    (count > 0).then(|| {
        view! {
            <button
                type="button"
                class="null-toggle"
                aria-expanded=move || open.get().to_string()
                aria-controls=controls
                on:click=move |_| open.update(|o| *o = !*o)
            >
                {move || null_fields_label(open.get(), count)}
            </button>
        }
    })
}

/// Move focus into the docked inspector, which the "Jump to details"
/// link points at. The panel is `tabindex="-1"`, so the anchor's own
/// navigation scrolls to it but leaves focus behind.
fn focus_inspector() {
    if let Some(el) = document()
        .get_element_by_id("search-inspector")
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SortState {
    col: usize,
    asc: bool,
}

#[component]
fn ResultsTableBody(
    resp: QueryResponse,
    executed_query: ExecutedQuery,
    page: Signal<usize>,
    busy: Signal<bool>,
    on_paginate: Callback<usize>,
    on_add_filter: Callback<(ExecutedQuery, Filter)>,
    on_navigate: Callback<SearchNavigation>,
    bus: ToastBus,
    details: Signal<Details>,
    rows_mode: Signal<Rows>,
    selected: RwSignal<Option<(u64, usize)>>,
    generation: Signal<u64>,
    order: StoredValue<Vec<usize>>,
    show_nulls: RwSignal<bool>,
) -> impl IntoView {
    let capabilities = Capabilities::for_query(&executed_query.effective);
    let columns: Vec<String> = resp.result.columns.iter().map(|c| c.name.clone()).collect();
    let rows_data = resp.result.rows.clone();
    let returned = resp.pagination.returned;
    let empty_message = if resp.pagination.offset == 0 {
        "No events match this query. Check the time range and filters."
    } else {
        "No events on this page. Try the previous page or check the time range and filters."
    };

    // Cells render as severity tokens for `_severity` plus the columns the
    // response declares (`sev()` output). A bare `severity` column is ordinary
    // sender data, so it renders like any other field.
    let severity_cols =
        severity_columns(columns.iter().map(String::as_str), &resp.severity_columns);
    let expanded = RwSignal::new(None::<usize>);
    let sort = RwSignal::new(None::<SortState>);

    // The message-first plan for THIS response, if it has a message to
    // lead with. Computed once; the memo below decides whether the mode
    // in force actually uses it.
    let plan = message_first(&columns, &severity_cols);
    let layout = Memo::new(move |_| {
        if rows_mode.get() == Rows::MessageFirst {
            plan.clone()
        } else {
            None
        }
    });

    let cols_for_header = columns.clone();
    let cols_for_view = columns.clone();
    // One header cell, by original column index. A closure rather than a
    // pre-built Vec because the message-first layout renders three of
    // these and the compact layout renders all of them, and both need
    // the same `th-sort` markup and the same sort state.
    let sort_header = move |i: usize, name: String| {
        // The visible text is the column, so the name says what the
        // press DOES and keeps that word inside it (WCAG 2.5.3). The
        // direction stays on the cell's `aria-sort` rather than joining
        // the name: on a real table it is announced once already, and
        // repeating it here would read twice.
        let sort_label = format!("Sort by {name}");
        view! {
            // A real `<table>`, so direction is announced by `aria-sort`
            // on the sorted `<th>` alone (ADR-0029) and the glyph is
            // decoration. The control is the button inside, never the cell.
            <th
                class="sortable"
                class:sorted=move || sort.get().is_some_and(|s| s.col == i)
                aria-sort=move || {
                    sort.get()
                        .filter(|s| s.col == i)
                        .map(|s| if s.asc { "ascending" } else { "descending" })
                }
            >
                <button
                    type="button"
                    class="th-sort"
                    aria-label=sort_label
                    on:click=move |_| sort.update(|cur| {
                        *cur = match *cur {
                            Some(s) if s.col == i => Some(SortState { col: i, asc: !s.asc }),
                            _ => Some(SortState { col: i, asc: false }),
                        };
                    })
                >
                    <span>{name}</span>
                    <span class="sort" aria-hidden="true">{move || {
                        sort.get().filter(|s| s.col == i).map_or("·", |s| if s.asc { "▲" } else { "▼" })
                    }}</span>
                </button>
            </th>
        }
    };
    let header_cells = move || match layout.get() {
        Some(mf) => mf
            .header_indices()
            .into_iter()
            .map(|i| sort_header(i, cols_for_header[i].clone()))
            .collect::<Vec<_>>(),
        None => cols_for_header
            .iter()
            .enumerate()
            .map(|(i, name)| sort_header(i, name.clone()))
            .collect::<Vec<_>>(),
    };

    let has_rows = !rows_data.is_empty();
    let sorted_indices = SortedIndices::new(&rows_data, sort);
    // Publish the render order for the page's `j`/`k` traversal. An
    // Effect, not a read at keystroke time: the memo is the only thing
    // that knows the current sort, and the keystroke handler must not
    // subscribe to it or every re-sort would re-run the handler.
    Effect::new(move |_| order.set_value(sorted_indices.indices.get()));
    // Cells per row, including the expander column: the colspans of the
    // empty-result cell and the inline detail must follow the layout.
    let visible_cols = columns.len();
    let span = Memo::new(move |_| {
        layout
            .get()
            .map_or(visible_cols, |mf| mf.header_indices().len())
            + 1
    });
    let fetched_page = resp.pagination.offset / PAGE_SIZE;
    let window = Signal::derive(move || {
        PageWindow::new(
            fetched_page,
            std::num::NonZeroUsize::new(PAGE_SIZE).expect("query page size is nonzero"),
            returned,
            PageTotal::Probe,
            busy.get() || page.get() != fetched_page,
        )
    });
    let wiring = RowWiring {
        on_add_filter,
        on_navigate,
        bus,
        details,
        layout,
        span,
        selected,
        generation,
        executed_query,
        capabilities,
        show_nulls,
    };

    view! {
        <>
            <div class="results-table-wrap">
                <table class="results-table" class:msg-first=move || layout.get().is_some()>
                    <thead>
                        <tr>
                            <th class="exp-col"><span class="sr-only">"Details"</span></th>
                            {header_cells}
                        </tr>
                    </thead>
                    <tbody>
                        {if has_rows {
                            // Closure, not a bare block: row rendering must
                            // re-run when the sort memo changes.
                            (move || sorted_indices.render(
                                rows_data.clone(),
                                cols_for_view.clone(),
                                severity_cols.clone(),
                                expanded,
                                wiring.clone(),
                            )).into_any()
                        } else {
                            view! {
                                <tr>
                                    <td class="results-empty-cell" colspan=move || span.get()>
                                        {empty_message}
                                    </td>
                                </tr>
                            }.into_any()
                        }}
                    </tbody>
                </table>
            </div>
            <Loaded
                state=Signal::derive(move || match window.get() {
                    Ok(window) => LoadState::Ready(window),
                    Err(_) => LoadState::Error("This result page extends past the supported row range.".to_string()),
                })
                label="pagination"
                render=Box::new(move |window: PageWindow| view! {
                    <OffsetPager
                        window=Signal::from(window)
                        on_page=on_paginate
                    />
                }.into_any())
            />
        </>
    }
    .into_any()
}

/// Sort indirection: the table renders rows by index, applying the
/// current sort lazily on each render. Avoids cloning rows.
#[derive(Clone, Copy)]
struct SortedIndices {
    indices: Memo<Vec<usize>>,
}

impl SortedIndices {
    fn new(rows: &[Vec<Value>], sort: RwSignal<Option<SortState>>) -> Self {
        let snapshot = rows.to_vec();
        let indices = Memo::new(move |_| {
            let mut idx: Vec<usize> = (0..snapshot.len()).collect();
            if let Some(s) = sort.get() {
                idx.sort_by(|&a, &b| {
                    let ord = compare(snapshot[a].get(s.col), snapshot[b].get(s.col));
                    if s.asc { ord } else { ord.reverse() }
                });
            }
            idx
        });
        Self { indices }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn render(
        &self,
        rows: Vec<Vec<Value>>,
        columns: Vec<String>,
        severity_cols: Vec<usize>,
        expanded: RwSignal<Option<usize>>,
        wiring: RowWiring,
    ) -> Vec<leptos::prelude::AnyView> {
        let indices = self.indices.get();
        indices
            .into_iter()
            .map(|i| {
                let row = rows[i].clone();
                let cols = columns.clone();
                let sev_cols = severity_cols.clone();
                view! {
                    <RowFragment
                        idx=i
                        row=row
                        columns=cols
                        severity_cols=sev_cols
                        expanded=expanded
                        wiring=wiring.clone()
                    />
                }
                .into_any()
            })
            .collect()
    }
}

/// Everything a row needs that is the same for every row: the two
/// callbacks, the toast bus, the reading modes, the inspector's
/// selection, and the executed query with its field provenance. One
/// struct rather than nine parameters, because the row fragment already
/// carries its own data and passing the page's wiring through by name
/// made both call sites unreadable.
#[derive(Clone)]
struct RowWiring {
    on_add_filter: Callback<(ExecutedQuery, Filter)>,
    on_navigate: Callback<SearchNavigation>,
    bus: ToastBus,
    details: Signal<Details>,
    /// The message-first plan in force, or `None` for the compact table.
    layout: Memo<Option<MessageFirst>>,
    /// Cells per row including the expander column, for the detail
    /// row's `colspan`.
    span: Memo<usize>,
    selected: RwSignal<Option<(u64, usize)>>,
    generation: Signal<u64>,
    executed_query: ExecutedQuery,
    capabilities: Capabilities,
    /// The null-field disclosure's open state, shared with the inspector.
    show_nulls: RwSignal<bool>,
}

#[component]
fn RowFragment(
    idx: usize,
    row: Vec<Value>,
    columns: Vec<String>,
    severity_cols: Vec<usize>,
    expanded: RwSignal<Option<usize>>,
    wiring: RowWiring,
) -> impl IntoView {
    let RowWiring {
        on_add_filter,
        on_navigate,
        bus,
        details,
        layout,
        span,
        selected,
        generation,
        executed_query,
        capabilities,
        show_nulls,
    } = wiring;
    let raw_actions = capabilities.raw_actions();

    // Which mode owns this row's disclosure. Inline is the default and
    // the only one that renders a sibling detail `<tr>`.
    let inspecting = move || details.get() == Details::Inspector;
    let is_selected = move || inspector_selection(generation.get(), selected.get()) == Some(idx);
    let is_open = move || {
        if inspecting() {
            is_selected()
        } else {
            expanded.get() == Some(idx)
        }
    };

    let cells_row = row.clone();
    let cells_cols = columns.clone();
    let cells = move || match layout.get() {
        Some(mf) => message_first_cells(&cells_row, &cells_cols, &mf),
        None => compact_cells(&cells_row, &severity_cols),
    };

    let columns_for_detail = columns.clone();
    let row_for_detail = row.clone();
    // `zip` semantics: a short row renders the cells it has, no more.
    let detail_fields = partition_detail_fields(&row[..row.len().min(columns.len())]);
    let null_count = detail_fields.null.len();
    let detail_grid_id = format!("result-{idx}-fields");
    let columns_for_actions = columns.clone();
    let row_for_actions = row.clone();

    view! {
        <>
            <tr class:expanded=move || !inspecting() && expanded.get() == Some(idx)
                class:selected=move || inspecting() && is_selected()>
                <td class="exp-col">
                    // The row's one control (ADR-0029): the caret button
                    // stretches over the row, so a pointer anywhere on it
                    // toggles the detail exactly once.
                    <button
                        type="button"
                        class="row-stretch"
                        aria-expanded=move || is_open().to_string()
                        // Only in inspector mode is there a panel to
                        // point at; inline, the detail is the next row
                        // and `aria-expanded` already says so.
                        aria-controls=move || inspecting().then_some("search-inspector")
                        aria-label=format!("Show details for result {}", idx + 1)
                        on:click=move |_| {
                            if inspecting() {
                                let cur_gen = generation.get_untracked();
                                selected.update(|cur| {
                                    *cur = if inspector_selection(cur_gen, *cur) == Some(idx) {
                                        None
                                    } else {
                                        Some((cur_gen, idx))
                                    };
                                });
                            } else {
                                expanded.update(|cur| {
                                    *cur = if *cur == Some(idx) { None } else { Some(idx) };
                                });
                            }
                        }
                    >
                        <span aria-hidden="true">
                            {move || if is_open() { "▾" } else { "▸" }}
                        </span>
                    </button>
                    // Under 900px the inspector stacks below the table, so
                    // the selected row needs a way down to it. Hidden by
                    // CSS above that width, where the panel is already
                    // beside the row.
                    <Show when=move || inspecting() && is_selected()>
                        <a
                            class="jump-details"
                            href="#search-inspector"
                            on:click=move |ev| {
                                ev.prevent_default();
                                focus_inspector();
                            }
                        >"Jump to details"</a>
                    </Show>
                </td>
                {cells}
            </tr>
            <Show when=move || !inspecting() && expanded.get() == Some(idx)>
                <tr>
                    <td class="detail" colspan=move || span.get()>
                        <div class="dg" id=detail_grid_id.clone()>
                            {
                            let columns_for_detail = columns_for_detail.clone();
                            let row_for_detail = row_for_detail.clone();
                            let detail_fields = detail_fields.clone();
                            let executed_query = executed_query.clone();
                            let capabilities = capabilities.clone();
                            move || detail_fields.visible(show_nulls.get()).into_iter().map(|i| {
                                let (name, v) = (&columns_for_detail[i], &row_for_detail[i]);
                                let key = name.clone();
                                let value_text = value_to_string(v);
                                let field_for_click = name.clone();
                                let value_for_click = value_text.clone();
                                let field_for_label = name.clone();
                                let value_for_label = value_text.clone();
                                let allowed = capabilities.detail_filter(name, v);
                                let query_for_click = executed_query.clone();
                                view! {
                                    <span class="k">{key}</span>
                                    <span class="v">
                                        {if allowed { view! { <button
                                            type="button"
                                            class="tag"
                                            aria-label=format!(
                                                "Include {field_for_label} = {value_for_label}",
                                            )
                                            on:click=move |_| {
                                                on_add_filter.run((query_for_click.clone(), Filter {
                                                    field: field_for_click.clone(),
                                                    value: value_for_click.clone(),
                                                    op: FilterOp::Include,
                                                }));
                                            }
                                        >{value_text}</button> }.into_any() } else { view! { <span>{value_text}</span> }.into_any() }}
                                    </span>
                                }
                            }).collect::<Vec<_>>()
                            }
                        </div>
                        <NullFieldsToggle
                            count=null_count
                            open=show_nulls
                            controls=detail_grid_id.clone()
                        />
                        {if raw_actions { view! {
                        <div class="actions">
                            <CopyRawButton
                                row=row_for_actions.clone()
                                columns=columns_for_actions.clone()
                            />
                            <ShowContextButton
                                row=row_for_actions.clone()
                                columns=columns_for_actions.clone()
                                on_navigate=on_navigate
                                bus=bus
                            />
                            <FindSimilarButton
                                row=row_for_actions.clone()
                                columns=columns_for_actions.clone()
                                on_navigate=on_navigate
                                bus=bus
                            />
                        </div>
                        }.into_any() } else { ().into_any() }}
                    </td>
                </tr>
            </Show>
        </>
    }
}

#[component]
fn CopyRawButton(row: Vec<Value>, columns: Vec<String>) -> impl IntoView {
    // The derived signal keeps `raw_or_synthesized` lazy: it runs at click
    // time, not on every render.
    let text = Signal::derive(move || raw_or_synthesized(&row, &columns));
    view! {
        <CopyButton text=text success_detail="Raw event copied to clipboard.">
            "Copy _raw"
        </CopyButton>
    }
}

#[component]
fn ShowContextButton(
    row: Vec<Value>,
    columns: Vec<String>,
    on_navigate: Callback<SearchNavigation>,
    bus: ToastBus,
) -> impl IntoView {
    let on_click = Callback::new(move |()| match build_context_query(&row, &columns) {
        Some(q) => on_navigate.run(q),
        None => bus.push(
            ToastKind::Info,
            "Show context",
            Some("Need a timestamp column to build a context window.".into()),
        ),
    });
    view! {
        <Btn variant=Variant::Secondary on_click=on_click>"Show context"</Btn>
    }
}

#[component]
fn FindSimilarButton(
    row: Vec<Value>,
    columns: Vec<String>,
    on_navigate: Callback<SearchNavigation>,
    bus: ToastBus,
) -> impl IntoView {
    let on_click = Callback::new(move |()| match build_similar_query(&row, &columns) {
        Some(q) => on_navigate.run(SearchNavigation {
            query: q,
            range: crate::query_merge::RangeSpec::default(),
        }),
        None => bus.push(
            ToastKind::Info,
            "Find similar",
            Some("Need a message column to find similar events.".into()),
        ),
    });
    view! {
        <Btn variant=Variant::Secondary on_click=on_click>"Find similar"</Btn>
    }
}

/// The compact layout's cells: one per column, in wire order.
fn compact_cells(row: &[Value], severity_cols: &[usize]) -> Vec<AnyView> {
    row.iter()
        .enumerate()
        .map(|(ci, v)| {
            if severity_cols.contains(&ci) {
                // Display shows the token; the wire (json/csv/SSE) keeps
                // the number for arithmetic consumers.
                let s = severity_display(v);
                let cls = severity_class(v);
                view! { <td><span class=cls>{s}</span></td> }.into_any()
            } else {
                view! { <td>{value_to_string(v)}</td> }.into_any()
            }
        })
        .collect()
}

/// The message-first layout's cells: time, severity pill, then the
/// message at full width with service / host / latency under it.
///
/// Every column this drops is still reachable in the inline detail or
/// the docked inspector — the mode trades the columns for one readable
/// message, it does not hide data.
fn message_first_cells(row: &[Value], columns: &[String], mf: &MessageFirst) -> Vec<AnyView> {
    let mut out: Vec<AnyView> = Vec::with_capacity(3);
    if let Some(i) = mf.time {
        let text = row.get(i).map(value_to_string).unwrap_or_default();
        out.push(view! { <td class="mono mf-time">{text}</td> }.into_any());
    }
    if let Some(i) = mf.severity
        && let Some(v) = row.get(i)
    {
        let s = severity_display(v);
        let cls = severity_class(v);
        out.push(view! { <td><span class=cls>{s}</span></td> }.into_any());
    }
    let message = row.get(mf.message).map(value_to_string).unwrap_or_default();
    let meta = mf
        .meta
        .iter()
        .filter_map(|&i| {
            let name = columns.get(i)?;
            let value = value_to_string(row.get(i)?);
            Some(view! { <span>{format!("{name} {value}")}</span> })
        })
        .collect::<Vec<_>>();
    let has_meta = !meta.is_empty();
    out.push(
        view! {
            <td class="mf-msg">
                <div class="msg">{message}</div>
                {has_meta.then_some(view! { <div class="mf-meta">{meta}</div> })}
            </td>
        }
        .into_any(),
    );
    out
}

fn raw_or_synthesized(row: &[Value], columns: &[String]) -> String {
    if let Some(idx) = columns.iter().position(|c| c == "_raw" || c == "raw")
        && let Some(v) = row.get(idx)
    {
        return value_to_string(v);
    }
    // Fallback: join "key=value" pairs so the user still gets something
    // copy-pastable.
    columns
        .iter()
        .zip(row.iter())
        .map(|(k, v)| format!("{k}={}", value_to_string(v)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build a phrase-match query on the first ~60 chars of the row's message.
fn build_similar_query(row: &[Value], columns: &[String]) -> Option<String> {
    let mi = find_col(columns, &["message", "msg"])?;
    let msg = value_to_string(row.get(mi)?);
    let trimmed = msg.trim();
    if trimmed.is_empty() {
        return None;
    }
    let take: String = trimmed.chars().take(60).collect();
    Some(format!("\"{}\"", escape_dq(&take)))
}
