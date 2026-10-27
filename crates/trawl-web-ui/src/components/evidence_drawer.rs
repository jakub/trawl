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
//! A capture the cap cut says so above its rows, with how many it kept
//! of how many the query produced ([`cut_notice`]); the pager counts only
//! the kept rows.
//! A result whose evidence file was pruned answers 410 and reads Pruned;
//! one whose file is missing answers 409 and reads unavailable; any other
//! failure keeps a Retry. None of them reads as an empty result. A 429
//! is the reader's rate limit, which the viewer shares with the board's
//! polls and its cited-row reads: the viewer says it is waiting, keeps
//! the page it shows, and reads again once the `Retry-After` has passed.
//!
//! While the board's tab is hidden the viewer reads nothing (ADR-0059).
//! The gate is `Reads::read`, the one place a read starts: then it owes
//! the read it was asked for, whether the viewer opened, paged, learned
//! its page size or saw a 429's `Retry-After` pass, and starts it when
//! the board's visibility listener says the tab shows.
//!
//! Below the rows, Open in Search reopens the result's executed text over
//! its bounds through the shared codec ([`result_search_href`]); a result
//! without bounds offers no link.
//!
//! A session that may write keeps a row from here, one row per keep, with
//! a reason ([`KeepGate`]). The Keep column is absent without
//! `investigation_write`, while the investigation is closed, and on a
//! result that does not read. The reason is checked against the server's
//! bounds before it is sent ([`keep_reason`]); the form submits from the
//! keyboard, says what is wrong beside the field, and sends once however
//! often it is pressed. A keep the server took is acknowledged here and
//! the board reads what changed; its form closes only if it still holds
//! the row that was sent, so a reason begun for another row while the
//! keep was in flight stays as it is. A keep whose answer was lost says
//! its outcome is unknown, keeps the reason, and offers a refresh of the
//! board, which it waits for before it sends again.
//!
//! The drawer is fixed over the page, so nothing it reads moves the
//! board under the reader.

use std::num::NonZeroUsize;

use fleet_ui::{Badge, Btn, Drawer, OffsetPager, PageTotal, PageWindow, Size, Tone, Variant};
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::investigations::InvestigationId;
use trawl_api::results::{CapturedResult, ResultPage, ResultShape};
use wasm_bindgen::JsCast;

use crate::api::{self, ApiError};
use crate::evidence_queue::{Paced, wait_ms};
use crate::investigation_live::{WriteFailure, keep_reason};
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

/// What the board lends the viewer for a keep.
#[derive(Clone, Copy)]
pub struct KeepGate {
    /// The investigation a keep is written to.
    pub investigation: StoredValue<InvestigationId>,
    /// Whether a keep may be written now: the session may write, the
    /// investigation is open and the board still reads.
    pub open: Signal<bool>,
    /// The generation of the newest read the board accepted.
    pub accepted: Signal<u64>,
    /// The board's newest read generation, taken when a keep's answer is
    /// lost: the form sends again once a read past it is accepted.
    pub mark: Callback<(), u64>,
    /// The board reads what changed, after a keep landed or was refused
    /// because the investigation closed.
    pub landed: Callback<()>,
    /// The person's own refresh of the board.
    pub refresh: Callback<()>,
}

/// What became of the person's last keep.
#[derive(Debug, Clone, PartialEq)]
enum KeepNote {
    Kept(u64),
    /// No answer arrived. The mark is the board's newest read generation
    /// when it failed: the form sends again once a later read lands.
    Unknown(u64, u64),
    /// The investigation was closed when the keep reached it.
    Closed(u64),
    Refused(u64, String),
}

impl KeepNote {
    fn text(&self) -> String {
        match self {
            Self::Kept(row) => format!("Kept row {row}. The board shows it in the activity."),
            Self::Unknown(row, _) => format!(
                "The answer to your keep of row {row} was lost, so it may or may not have been \
                 kept. Refresh the board and look for it in the activity before you keep it again."
            ),
            Self::Closed(row) => format!(
                "The investigation was closed before your keep of row {row} reached it, so \
                 nothing was kept."
            ),
            Self::Refused(row, message) => format!("Could not keep row {row}. {message}"),
        }
    }
}

/// The viewer's keep: the row whose form is open, its reason, and what
/// became of the last one sent.
#[derive(Clone, Copy)]
struct Keeping {
    gate: KeepGate,
    result_id: StoredValue<String>,
    row: RwSignal<Option<u64>>,
    reason: RwSignal<String>,
    error: RwSignal<Option<String>>,
    note: RwSignal<Option<KeepNote>>,
    sending: RwSignal<bool>,
    input: NodeRef<html::Input>,
    /// The reason field takes focus when it mounts, once per Keep press.
    wants_focus: StoredValue<bool>,
    body: NodeRef<html::Div>,
}

impl Keeping {
    /// Whether the form must not send now: a keep is in flight, or the
    /// last one's outcome is unknown and no read has landed since.
    fn held(self) -> bool {
        self.sending.get()
            || matches!(self.note.get(), Some(KeepNote::Unknown(_, mark)) if self.gate.accepted.get() <= mark)
    }

    /// Open the form for `row`, its reason kept if it was already open
    /// for that row.
    fn start(self, row: u64) {
        if self.row.get_untracked() != Some(row) {
            self.reason.set(String::new());
            self.error.set(None);
        }
        let same = self.row.get_untracked() == Some(row);
        self.row.set(Some(row));
        match self.input.get_untracked() {
            Some(input) if same => {
                let _ = input.focus();
            }
            // The field for this row mounts after this update.
            _ => self.wants_focus.set_value(true),
        }
    }

    fn cancel(self) {
        let row = self.row.get_untracked();
        self.row.set(None);
        self.error.set(None);
        self.focus_control(row);
    }

    /// Give focus back to row `row`'s Keep control once its form is gone.
    /// A keep's answer can land after the drawer closed, so the body is
    /// read disposal-safe, and a drawer that is gone takes no focus.
    fn focus_control(self, row: Option<u64>) {
        let (Some(row), Some(body)) = (row, self.body.try_get_untracked().flatten()) else {
            return;
        };
        let selector = format!("button[aria-label=\"Keep row {row}\"]");
        if let Ok(Some(control)) = body.query_selector(&selector)
            && let Ok(control) = control.dyn_into::<web_sys::HtmlElement>()
        {
            let _ = control.focus();
        }
    }

    /// Send the keep the form holds, once.
    fn submit(self) {
        if self.held() {
            return;
        }
        let Some(row) = self.row.get_untracked() else {
            return;
        };
        let reason = match keep_reason(&self.reason.get_untracked()) {
            Ok(reason) => reason.to_owned(),
            Err(problem) => {
                self.error.set(Some(problem.message()));
                return;
            }
        };
        let (Some(id), Some(result_id)) = (
            self.gate.investigation.try_get_value(),
            self.result_id.try_get_value(),
        ) else {
            return;
        };
        self.error.set(None);
        self.note.set(None);
        self.sending.set(true);
        spawn_local(async move {
            let result = api::append_keep(&id, &result_id, row, &reason).await;
            let _ = self.sending.try_set(false);
            let note = match result {
                Ok(_) => {
                    // The form may have moved to another row while this
                    // keep was in flight: that draft is the person's, so
                    // only the form that sent is cleared.
                    if self.row.try_get_untracked() == Some(Some(row)) {
                        let _ = self.row.try_set(None);
                        let _ = self.reason.try_set(String::new());
                        self.focus_control(Some(row));
                    }
                    let _ = self.gate.landed.try_run(());
                    KeepNote::Kept(row)
                }
                Err(WriteFailure::Unauthorized) => {
                    crate::auth_return::redirect_to_login();
                    return;
                }
                Err(WriteFailure::Closed) => {
                    let _ = self.gate.landed.try_run(());
                    KeepNote::Closed(row)
                }
                Err(WriteFailure::Unknown) => {
                    KeepNote::Unknown(row, self.gate.mark.try_run(()).unwrap_or(u64::MAX))
                }
                Err(WriteFailure::Refused(message)) => KeepNote::Refused(row, message),
                // An append carries no sequence, so the server never
                // answers it stale.
                Err(WriteFailure::Stale { .. }) => {
                    KeepNote::Refused(row, "The investigation changed.".to_owned())
                }
            };
            let _ = self.note.try_set(Some(note));
        });
    }
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
/// drawer is gone. While the board's tab is hidden, the newest read asked
/// for is owed instead, and starts when the tab shows.
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
    /// Whether the board's tab shows.
    visible: Signal<bool>,
    /// The page, and the row it was opened on, asked for while the tab
    /// was hidden.
    owed: StoredValue<Option<(usize, Option<u64>)>>,
}

impl Reads {
    fn new(id: String, size: RwSignal<Option<NonZeroUsize>>, visible: Signal<bool>) -> Self {
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
            visible,
            owed: StoredValue::new(None),
        };
        on_cleanup(move || {
            let _ = reads.timer.try_set_value(None);
            reads.abort.try_update_value(|abort| {
                if let Some(abort) = abort.take() {
                    abort.abort();
                }
            });
        });
        // The tab shows: start the read it held.
        Effect::new(move |_| {
            if visible.get()
                && let Some((page, row)) = reads.owed.try_update_value(Option::take).flatten()
            {
                untrack(|| reads.read(page, row));
            }
        });
        reads
    }

    /// Read page `page`, or, when `row` is set and the page size is not
    /// known yet, the first page to learn it and then the page that
    /// holds `row`. It replaces any read asked for before it. While the
    /// tab is hidden it starts nothing and is owed instead.
    fn read(self, page: usize, row: Option<u64>) {
        let _ = self.timer.try_set_value(None);
        if self.visible.try_get_untracked() != Some(true) {
            let _ = self.owed.try_set_value(Some((page, row)));
            return;
        }
        let _ = self.owed.try_set_value(None);
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

/// What a result the capture cap cut says above its rows, or nothing for
/// a whole one. An event-shaped capture keeps its most recent rows; the
/// counts read as the pager's do.
fn cut_notice(result: &CapturedResult) -> Option<String> {
    if !result.truncated {
        return None;
    }
    let (kept, total) = (result.captured_rows, result.total_rows);
    Some(match result.shape {
        ResultShape::Events => format!("This result kept the {kept} most recent of {total} rows."),
        ResultShape::Whole => format!("This result kept {kept} of {total} rows."),
    })
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
    /// Whether the board's tab shows: while it does not, the viewer
    /// reads nothing.
    visible: Signal<bool>,
    on_close: Callback<()>,
    /// Present for a session that may write to the investigation.
    #[prop(optional)]
    keep: Option<KeepGate>,
) -> impl IntoView {
    let Opened {
        result_id,
        row: marked,
    } = opened;
    let short = short_result_id(&result_id).to_owned();
    let body = NodeRef::<html::Div>::new();
    let keeping = keep.map(|gate| Keeping {
        gate,
        result_id: StoredValue::new(result_id.clone()),
        row: RwSignal::new(None),
        reason: RwSignal::new(String::new()),
        error: RwSignal::new(None),
        note: RwSignal::new(None),
        sending: RwSignal::new(false),
        input: NodeRef::new(),
        wants_focus: StoredValue::new(false),
        body,
    });
    let reads = Reads::new(result_id, page_size, visible);
    reads.read(page_of(marked, page_size.get_untracked()), marked);

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
                        <ViewerRows answer=*answer label=rows_label.clone() marked=marked reads=reads keeping=keeping/>
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

/// One page of rows, what a cut capture says above them, its pager and
/// Open in Search.
#[component]
fn ViewerRows(
    answer: ResultPage,
    label: String,
    marked: Option<u64>,
    reads: Reads,
    keeping: Option<Keeping>,
) -> impl IntoView {
    let keepable = move || keeping.is_some_and(|k| k.gate.open.get());
    let EvidenceRows { columns, rows } = evidence_rows(&answer);
    let href = result_search_href(&answer.result);
    let cut = cut_notice(&answer.result);
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
        {cut.map(|cut| view! { <p class="inv-meta">{cut}</p> })}
        <div class="results-table-wrap inv-viewer-rows" role="region" aria-label=label tabindex="0">
            <table class="results-table">
                <thead><tr>
                    <th scope="col" class="inv-row-n">"Row"</th>
                    {columns.into_iter().map(|name| view! { <th scope="col">{name}</th> }).collect_view()}
                    <Show when=keepable><th scope="col" class="inv-keep-col">"Keep"</th></Show>
                </tr></thead>
                <tbody>
                    {rows.into_iter().map(|row| {
                        let cited = marked == Some(row.number);
                        view! {
                            <tr class:inv-row-cited=cited aria-current=cited.then_some("true")>
                                <th scope="row" class="inv-row-n">{row.number}</th>
                                {row.cells.into_iter().map(|cell| view! { <td>{cell}</td> }).collect_view()}
                                <Show when=keepable>
                                    {keeping.map(|k| {
                                        let number = row.number;
                                        view! {
                                            <td class="inv-keep-col">
                                                <button type="button" class="btn-sec btn-xs"
                                                    aria-label=format!("Keep row {number}")
                                                    aria-expanded=move || (k.row.get() == Some(number)).to_string()
                                                    on:click=move |_| k.start(number)>"Keep"</button>
                                            </td>
                                        }
                                    })}
                                </Show>
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
        {empty.then(|| view! { <p class="inv-empty">"This result captured no rows."</p> })}
        {keeping.map(|k| view! { <KeepForm keeping=k/> })}
        <OffsetPager window=window on_page=Callback::new(move |p| reads.read(p, None))/>
        {href.map(|href| view! {
            <a class="btn-sec btn-xs inv-open-search" href=href>"Open in Search"</a>
        })}
    }
}

/// The reason form for the row whose Keep was pressed, and the notice of
/// the last keep.
#[component]
fn KeepForm(keeping: Keeping) -> impl IntoView {
    let k = keeping;
    let open = move || k.gate.open.get();
    let unknown = move || match k.note.get() {
        Some(KeepNote::Unknown(_, mark)) => k.gate.accepted.get() <= mark,
        _ => false,
    };
    Effect::new(move || {
        if let Some(input) = k.input.get()
            && k.wants_focus.try_update_value(std::mem::take) == Some(true)
        {
            let _ = input.focus();
        }
    });
    view! {
        {move || k.note.get().map(|note| view! {
            <div class="inv-write-note" class:inv-write-warn=!matches!(note, KeepNote::Kept(_))>
                <p role="status">{note.text()}</p>
                {unknown().then(|| view! {
                    <Btn variant=Variant::Secondary size=Size::Xs on_click=k.gate.refresh>"Refresh the board"</Btn>
                })}
            </div>
        })}
        <Show when=move || open() && k.row.get().is_some()>
            {move || k.row.get().map(|row| view! {
                <form class="inv-keep" novalidate aria-label=format!("Keep row {row}")
                    on:submit=move |e: web_sys::SubmitEvent| { e.prevent_default(); k.submit(); }>
                    <label for="inv-keep-reason" class="inv-keep-l">{format!("Why keep row {row}?")}</label>
                    <input id="inv-keep-reason" class="inv-keep-in" type="text" autocomplete="off"
                        node_ref=k.input
                        prop:value=move || k.reason.get()
                        on:input=move |e| { k.reason.set(event_target_value(&e)); k.error.set(None); }
                        aria-invalid=move || k.error.with(Option::is_some).to_string()
                        aria-describedby=move || k.error.with(Option::is_some).then_some("inv-keep-error")/>
                    {move || k.error.get().map(|message| view! {
                        <p id="inv-keep-error" class="inv-keep-error">{message}</p>
                    })}
                    <div class="inv-keep-acts">
                        <button type="submit" class="btn-pri btn-xs" disabled=move || k.held()>{format!("Keep row {row}")}</button>
                        <button type="button" class="btn-sec btn-xs" on:click=move |_| k.cancel()>"Cancel"</button>
                    </div>
                </form>
            })}
        </Show>
    }
}
