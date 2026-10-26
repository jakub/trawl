// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<EvidenceDrawer/>` — the investigation board's viewer of one captured
//! result (ADR-0057, ADR-0059).
//!
//! It opens from a step that captured a result, at the first page, and
//! from a row a finding cites, at the page that holds that row, with the
//! row marked. It reads the stored rows through the captured-result page
//! route and nothing else: it never runs the query again, so a result
//! whose events have left the corpus still shows every row it captured.
//! Each read omits `limit`, so the server's page size decides a page, and
//! the viewer pages by the size the last answer reported. A cited row
//! past the first page costs one read to learn that size, which the page
//! then keeps for every later open.
//!
//! Cells read as the results table's do ([`evidence_rows`], #214 types).
//! A result whose evidence file was pruned answers 410 and reads Pruned;
//! one whose file is missing answers 409 and reads unavailable; any other
//! failure keeps a Retry. None of them reads as an empty result. A 429
//! is the reader's rate limit, which the viewer shares with the board's
//! polls and its cited-row reads: the viewer says it is waiting, keeps
//! the page it shows, and reads again once the `Retry-After` has passed.
//!
//! Below the rows, Open in Search reopens the result's executed text over
//! its bounds through the shared codec ([`result_search_href`]); a result
//! without bounds offers no link.
//!
//! The drawer is fixed over the page, so nothing it reads moves the
//! board under the reader.

use std::num::NonZeroUsize;

use fleet_ui::{Badge, Btn, Drawer, OffsetPager, PageTotal, PageWindow, Size, Tone, Variant};
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::results::ResultPage;

use crate::api::{self, ApiError};
use crate::evidence_queue::{Paced, wait_ms};
use crate::investigation_log::{EvidenceRows, evidence_rows, result_search_href, short_result_id};

/// What the viewer was opened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The captured result.
    pub result_id: String,
    /// The row a finding cites, when a cited row opened it: the viewer
    /// opens at its page and marks it.
    pub row: Option<u64>,
}

/// What the viewer shows.
#[derive(Debug, Clone)]
enum Shown {
    Loading,
    Rows(Box<ResultPage>),
    /// 410 `result_gone`: the server's sentence for why.
    Pruned(String),
    /// 409 `result_unavailable`: the server's sentence.
    Unavailable(String),
    Failed(String),
}

/// What a read the server throttled says while the viewer waits.
const THROTTLED: &str = "The server is limiting reads for your API key. Reading again shortly.";

/// The viewer's reads: one in flight at a time, each taking the next
/// generation, so only the newest may land and nothing lands once the
/// drawer is gone.
#[derive(Clone, Copy)]
struct Reads {
    id: StoredValue<String>,
    /// The page size the server last reported, shared with the page so a
    /// later open of a cited row can go straight to its page.
    size: RwSignal<Option<NonZeroUsize>>,
    shown: RwSignal<Shown>,
    /// The page last asked for, which Retry asks for again.
    asked: StoredValue<usize>,
    busy: RwSignal<bool>,
    waiting: RwSignal<bool>,
    generation: StoredValue<u64>,
    abort: StoredValue<Option<web_sys::AbortController>, LocalStorage>,
    timer: StoredValue<Option<gloo_timers::callback::Timeout>, LocalStorage>,
}

impl Reads {
    fn new(id: String, size: RwSignal<Option<NonZeroUsize>>) -> Self {
        let reads = Self {
            id: StoredValue::new(id),
            size,
            shown: RwSignal::new(Shown::Loading),
            asked: StoredValue::new(0),
            busy: RwSignal::new(false),
            waiting: RwSignal::new(false),
            generation: StoredValue::new(0),
            abort: StoredValue::new_local(None),
            timer: StoredValue::new_local(None),
        };
        on_cleanup(move || {
            let _ = reads.timer.try_set_value(None);
            reads.abort.try_update_value(|abort| {
                if let Some(abort) = abort.take() {
                    abort.abort();
                }
            });
        });
        reads
    }

    /// Read page `page`, or, when `row` is set and the page size is not
    /// known yet, the first page to learn it and then the page that
    /// holds `row`.
    fn read(self, page: usize, row: Option<u64>) {
        let _ = self.timer.try_set_value(None);
        let Some(signal) = self.abort.try_update_value(|abort| {
            if let Some(old) = abort.take() {
                old.abort();
            }
            *abort = web_sys::AbortController::new().ok();
            abort.as_ref().map(web_sys::AbortController::signal)
        }) else {
            return;
        };
        let Some(mine) = self.generation.try_update_value(|g| {
            *g = g.wrapping_add(1);
            *g
        }) else {
            return;
        };
        let Some(id) = self.id.try_get_value() else {
            return;
        };
        self.asked.set_value(page);
        let size = self.size.get_untracked();
        let offset = size.map_or(0, |size| {
            PageWindow::checked_offset(page, size).unwrap_or(0)
        });
        self.busy.set(true);
        spawn_local(async move {
            let paced = api::result_page(&id, offset as u64, None, signal.as_ref()).await;
            if self.generation.try_get_value() != Some(mine) {
                return;
            }
            let result = match paced {
                Paced::Throttled { retry_after_ms } => {
                    self.waiting.set(true);
                    let at = js_sys::Date::now() + retry_after_ms;
                    let wait = wait_ms(js_sys::Date::now(), at);
                    let _ = self
                        .timer
                        .try_set_value(Some(gloo_timers::callback::Timeout::new(
                            wait,
                            move || self.read(page, row),
                        )));
                    return;
                }
                Paced::Answered(result) => result,
            };
            self.waiting.set(false);
            if matches!(result, Err(ApiError::Unauthorized)) {
                crate::auth_return::redirect_to_login();
                return;
            }
            match result {
                Ok(answer) => {
                    let learned = NonZeroUsize::new(answer.pagination.limit);
                    if learned.is_some() {
                        self.size.set(learned);
                    }
                    // Opened on a cited row before the page size was
                    // known: now it is, read the row's page.
                    if let (None, Some(row), Some(learned)) = (size, row, learned) {
                        let at = usize::try_from(row).unwrap_or(usize::MAX) / learned;
                        if at != 0 {
                            self.read(at, None);
                            return;
                        }
                    }
                    self.shown.set(Shown::Rows(Box::new(answer)));
                }
                Err(e) => self.shown.set(match e.http_status() {
                    Some(410) => Shown::Pruned(e.to_string()),
                    Some(409) => Shown::Unavailable(e.to_string()),
                    Some(403) => {
                        Shown::Failed("You do not have permission to read this result.".into())
                    }
                    _ => Shown::Failed(e.to_string()),
                }),
            }
            self.busy.set(false);
        });
    }
}

/// The page a cited row is on, once the page size is known.
fn page_of(row: Option<u64>, size: Option<NonZeroUsize>) -> usize {
    match (row, size) {
        (Some(row), Some(size)) => usize::try_from(row).unwrap_or(usize::MAX) / size,
        _ => 0,
    }
}

#[component]
pub fn EvidenceDrawer(
    opened: Opened,
    /// The server's page size as this page last learned it.
    page_size: RwSignal<Option<NonZeroUsize>>,
    on_close: Callback<()>,
) -> impl IntoView {
    let Opened {
        result_id,
        row: marked,
    } = opened;
    let short = short_result_id(&result_id).to_owned();
    let reads = Reads::new(result_id, page_size);
    reads.read(page_of(marked, page_size.get_untracked()), marked);

    let body = NodeRef::<html::Div>::new();
    // The marked row scrolls into the viewer's own body when its page
    // lands; the board behind the drawer does not move.
    Effect::new(move |_| {
        if !reads.shown.with(|s| matches!(s, Shown::Rows(_))) {
            return;
        }
        let Some(body) = body.get() else { return };
        request_animation_frame(move || {
            let Some(row) = body
                .query_selector("tr[aria-current=\"true\"]")
                .ok()
                .flatten()
            else {
                return;
            };
            let Some(scroller) = row.closest(".sd-body").ok().flatten() else {
                return;
            };
            let top =
                row.get_bounding_client_rect().top() - scroller.get_bounding_client_rect().top();
            let height = f64::from(scroller.client_height());
            if top < 0.0 || top > height - 48.0 {
                scroller.scroll_by_with_x_and_y(0.0, top - height / 3.0);
            }
        });
    });

    let label = format!("Result {short}");
    let title = label.clone();
    let rows_label = format!("Rows of result {short}");
    let retry = Callback::new(move |()| {
        reads.shown.set(Shown::Loading);
        reads.read(reads.asked.get_value(), marked);
    });

    view! {
        <Drawer
            // One surface: the strip renders as the meta bar.
            tabs=vec![]
            tabs_label="Captured result"
            label=Signal::stored(label)
            active_tab=Signal::derive(String::new)
            on_tab_change=Callback::new(|_: String| {})
            on_close=on_close
            close_size=12
            meta="Captured rows".to_string()
            title=Box::new(move || view! { <span class="name">{title.clone()}</span> }.into_any())
        >
            <div class="inv-viewer" node_ref=body aria-busy=move || reads.busy.get().to_string()>
                {move || reads.waiting.get().then(|| view! { <p class="inv-meta" role="status">{THROTTLED}</p> })}
                {move || match reads.shown.get() {
                    Shown::Loading => view! { <p class="inv-meta" role="status">"Reading the captured rows…"</p> }.into_any(),
                    Shown::Rows(answer) => view! {
                        <ViewerRows answer=*answer label=rows_label.clone() marked=marked reads=reads/>
                    }.into_any(),
                    Shown::Pruned(message) => view! {
                        <p class="inv-pruned" role="status">
                            <Badge tone=Tone::Neutral>"Pruned"</Badge>
                            " " {message}
                        </p>
                    }.into_any(),
                    Shown::Unavailable(message) => view! {
                        <p class="inv-meta" role="status">"These rows are unavailable. " {message}</p>
                        <Btn variant=Variant::Secondary size=Size::Xs on_click=retry>"Check again"</Btn>
                    }.into_any(),
                    Shown::Failed(message) => view! {
                        <p class="inv-meta" role="status">"Could not read the rows. " {message}</p>
                        <Btn variant=Variant::Secondary size=Size::Xs on_click=retry>"Retry"</Btn>
                    }.into_any(),
                }}
            </div>
        </Drawer>
    }
}

/// One page of rows, its pager and Open in Search.
#[component]
fn ViewerRows(
    answer: ResultPage,
    label: String,
    marked: Option<u64>,
    reads: Reads,
) -> impl IntoView {
    let EvidenceRows { columns, rows } = evidence_rows(&answer);
    let href = result_search_href(&answer.result);
    let size = NonZeroUsize::new(answer.pagination.limit).unwrap_or(NonZeroUsize::MIN);
    let returned = answer.pagination.returned;
    let total = answer.pagination.total;
    let page = answer.pagination.offset / size;
    let window = Signal::derive(move || {
        PageWindow::new(
            page,
            size,
            returned,
            PageTotal::Known(total),
            reads.busy.get(),
        )
        .unwrap_or_else(|_| {
            PageWindow::new(0, size, 0, PageTotal::Known(0), true).expect("an empty window fits")
        })
    });
    let empty = rows.is_empty();
    view! {
        <div class="results-table-wrap inv-viewer-rows" role="region" aria-label=label tabindex="0">
            <table class="results-table">
                <thead><tr>
                    <th scope="col" class="inv-row-n">"Row"</th>
                    {columns.into_iter().map(|name| view! { <th scope="col">{name}</th> }).collect_view()}
                </tr></thead>
                <tbody>
                    {rows.into_iter().map(|row| {
                        let cited = marked == Some(row.number);
                        view! {
                            <tr class:inv-row-cited=cited aria-current=cited.then_some("true")>
                                <th scope="row" class="inv-row-n">{row.number}</th>
                                {row.cells.into_iter().map(|cell| view! { <td>{cell}</td> }).collect_view()}
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
        {empty.then(|| view! { <p class="inv-empty">"This result captured no rows."</p> })}
        <OffsetPager window=window on_page=Callback::new(move |p| reads.read(p, None))/>
        {href.map(|href| view! {
            <a class="btn-sec btn-xs inv-open-search" href=href>"Open in Search"</a>
        })}
    }
}
