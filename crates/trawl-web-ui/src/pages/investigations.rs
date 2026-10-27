// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<InvestigationsPage/>` and `<InvestigationPage/>` — the investigation
//! list and one investigation (ADR-0059).
//!
//! Both pages are for a session holding `investigation_read` and `query`
//! ([`perms::can_read_investigations`]). Without them the page says so
//! and asks the server nothing, and the sidebar has no entry for it. A
//! first read the server refuses with 403 (a role revoked after `/me`
//! answered) shows the same page, with nothing to retry; the sidebar
//! stays as `/me` said until it answers otherwise.
//! A session that also holds `investigation_write`
//! ([`perms::can_write_investigations`]) closes or reopens the
//! investigation beside its status and keeps rows from the evidence
//! viewer; without it, no control on these pages writes or stands in for
//! one.
//!
//! The list is ADR-0029 rows, one stretched link each. The investigation
//! reads top to bottom: the question and scope, the latest finding, the
//! activity (every entry, a line each, in the order they were written),
//! every finding with the rows it cites, then the steps, each a
//! disclosure over the text it executed ([`crate::investigation_log`]).
//!
//! The page reads on mount and on Refresh or Load more, and while its
//! tab is visible it reads again by itself every few seconds. While the
//! tab is hidden it reads nothing, whoever asks: not the investigation,
//! not a cited row, not a page of the evidence viewer. A read asked for
//! then waits until the tab shows, and a board opened in a hidden tab
//! says Paused until then ([`crate::investigation_live`], [`BoardReads`]).
//! Each of the page's three kinds of read has one gate that holds it,
//! and the board's one visibility listener opens all three when the tab
//! shows: [`BoardReads::defer`] for the investigation,
//! [`EvidenceReads::pump`] for cited rows, and the viewer's own
//! ([`EvidenceDrawer`]).
//! The header says whether the board is live and when
//! it last checked; a polite live region says only that a check failed,
//! that the board stopped or came back, and how many entries a read
//! brought. Each read asks for the entries changed since the last one,
//! and a response for a read that a later one superseded, or that lands
//! after the page is gone, is dropped. A step that finishes or a finding
//! whose status changes updates where it stands, so an open step stays
//! open. A change the reader did not ask for, a poll or a cited row
//! landing, leaves what they read where it was on screen, however much
//! grew above it. A read a person asked for that leaves entries unread is
//! followed at once, up to
//! [`AUTO_PAGES`](crate::investigation_log::AUTO_PAGES) reads. Until the
//! log is whole the latest finding and the findings say the page is
//! still reading; when it stops short they name the entry they are as
//! of, and Load more reads on.
//!
//! A cited row is read through the captured-result page route, once per
//! row per page visit, paced so a board that cites many rows stays
//! inside the reader's rate limit ([`crate::evidence_queue`]). When the
//! server keeps refusing those reads, the queue stops and the Findings
//! section says how many rows wait, with one control that reads them
//! again. A row
//! whose evidence file was pruned answers 410 and shows as pruned in its
//! place: the finding still cites it. A row whose read failed keeps a
//! Retry, and Refresh asks again for every failed row. A row asked for
//! while the tab is hidden shows as loading until the tab shows.
//!
//! A step that captured a result, and a cited row that reads, open the
//! evidence viewer over that result's stored rows
//! ([`EvidenceDrawer`]): a cited row at the page that holds it, marked.
//! The viewer is one drawer at a time, fixed over the page, so opening it
//! or paging it never moves the board.
//!
//! Close or Reopen is one PATCH carrying the sequence the board holds
//! when it is pressed, with no confirmation, sent once and never retried
//! ([`StatusControl`]). A 409, stale or closed, leaves a notice and reads
//! the board at once, or once its hidden tab shows, keeping the reader's
//! place, so the next press acts on the status that stands. A success stays acknowledged whatever the
//! read after it does, and the board's cursor moves only with a read it
//! accepted ([`BoardReads::after_write`]). A write whose answer was lost
//! says its outcome is unknown and holds its control until a read begun
//! after it lands ([`BoardReads::mark`]); the notice offers that read.

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use trawl_api::investigations::{
    EntryPayload, EvidenceRef, Investigation, InvestigationId, InvestigationList,
    InvestigationRead, InvestigationStatus, LogEntry, StepBounds,
};
use wasm_bindgen::{JsCast, closure::Closure};

use crate::api::{self, ApiError};
use crate::components::evidence_drawer::{EvidenceDrawer, KeepGate, Opened};
use crate::evidence_queue::{EvidenceQueue, Next, Paced, wait_ms};
use crate::investigation_live::{
    Connection, Liveness, Placed, ReadEnd, WriteFailure, announcement, new_entries_text,
    pick_anchor,
};
use crate::investigation_log::{
    Activity, Board, ENTRY_PAGE, EvidenceRow, Finding, NO_PURPOSE, Reach, evidence_label,
    evidence_rows, finding_status_label, finding_status_tone, investigation_status_label,
    investigation_status_tone, is_result_id, outcome_facts, outcome_label, outcome_tone,
    short_result_id,
};
use crate::perms;
use fleet_ui::{Badge, Btn, LoadMore, LoadState, Loaded, Pager, Size, Variant, When, WhenMode};

/// What a session without the grants reads in place of either page.
const NOT_PERMITTED: &str = "You do not have permission to read investigations.";

/// Whether the signed-in session may read investigations. The shell
/// mounts pages only once `/me` has answered.
fn permitted() -> Signal<bool> {
    let me = expect_context::<RwSignal<Option<api::MeResponse>>>();
    Signal::derive(move || {
        me.get()
            .is_some_and(|m| perms::can_read_investigations(&m.permissions))
    })
}

/// The page a session without the grants sees: the heading and the
/// reason, and nothing to press.
#[component]
fn NotPermitted() -> impl IntoView {
    view! {
        <div class="page inv-page">
            <div class="page-hd compact">
                <div><h1>"Investigations"</h1></div>
            </div>
            <p class="inv-denied" role="status">{NOT_PERMITTED}</p>
        </div>
    }
}

/// A read refused with 401 sends the page to sign-in, carrying its URL.
fn sign_in_if_unauthorized<T>(result: &Result<T, ApiError>) -> bool {
    if matches!(result, Err(ApiError::Unauthorized)) {
        crate::auth_return::redirect_to_login();
        true
    } else {
        false
    }
}

/// Whether the server refused a read for want of a grant.
fn forbidden(error: &ApiError) -> bool {
    error.http_status() == Some(403)
}

/// The sentence for a failed read: the server's own for a refusal it
/// explained, a fixed one for a permission refusal.
fn read_failure(error: &ApiError) -> String {
    if forbidden(error) {
        NOT_PERMITTED.to_owned()
    } else {
        error.to_string()
    }
}

#[component]
pub fn InvestigationsPage() -> impl IntoView {
    let permitted = permitted();
    view! {
        <Show when=move || permitted.get() fallback=|| view! { <NotPermitted/> }>
            <InvestigationList/>
        </Show>
    }
}

#[component]
fn InvestigationList() -> impl IntoView {
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    let list = RwSignal::new(None::<InvestigationList>);
    let error = RwSignal::new(None::<String>);
    // The first read was refused with 403: the page is the not-permitted
    // one, with nothing to retry.
    let refused = RwSignal::new(false);
    let busy = RwSignal::new(false);
    // Each read takes the next generation; only the newest may land.
    let generation = RwSignal::new(0u64);

    let load = Callback::new(move |()| {
        let mine = generation.get_untracked().wrapping_add(1);
        generation.set(mine);
        busy.set(true);
        spawn_local(async move {
            let result = api::list_investigations().await;
            if generation.try_get_untracked() != Some(mine) || sign_in_if_unauthorized(&result) {
                return;
            }
            match result {
                Ok(read) => {
                    list.set(Some(read));
                    error.set(None);
                }
                Err(e) if forbidden(&e) && list.with_untracked(Option::is_none) => {
                    refused.set(true);
                }
                Err(e) => error.set(Some(read_failure(&e))),
            }
            busy.set(false);
        });
    });
    load.run(());

    let state = Signal::derive(move || match (list.get(), error.get()) {
        (Some(_), _) => LoadState::Ready(()),
        (None, Some(message)) if !busy.get() => LoadState::Error(message),
        (None, _) => LoadState::Loading,
    });
    let rows = Signal::derive(move || list.get().map(|l| l.investigations).unwrap_or_default());

    view! {
        <Show when=move || !refused.get() fallback=|| view! { <NotPermitted/> }>
        <div class="page inv-page">
            <div class="page-hd compact">
                <div>
                    <h1>"Investigations"</h1>
                    <p class="sub">"What agents and people asked of the logs, what ran, and what they found."</p>
                </div>
                <div class="actions">
                    <Btn variant=Variant::Secondary size=Size::Sm on_click=load>"Refresh"</Btn>
                </div>
            </div>
            {move || (list.get().is_some()).then(|| error.get()).flatten().map(|message| view! {
                <p class="inv-stale" role="status">"Refresh failed. Showing the last list read. " {message}</p>
            })}
            <section class="list-sheet" aria-labelledby="inv-sheet-title">
                <div class="list-sheet-hd">
                    <h2 id="inv-sheet-title" class="list-sheet-ttl">"All investigations"</h2>
                </div>
                <fleet_ui::OverflowHint viewport=table_viewport/>
                <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region"
                    aria-label="Investigations" aria-busy=move || busy.get().to_string() tabindex="0"
                    style="--list-min-width:320px">
                <div class="tbl-body">
                    <Loaded state=state label="investigations" retry=load render=Box::new(|()| ().into_any())/>
                    <table class="fleet-table investigations-table" aria-label="Investigations">
                        <thead><tr>
                            <th scope="col">"Title"</th>
                            <th scope="col" class="inv-col-status">"Status"</th>
                            <th scope="col" class="inv-col-when">"Updated"</th>
                            <th scope="col" class="inv-col-who">"By"</th>
                        </tr></thead>
                        <tbody>
                            <For each=move || rows.get() key=|row| (row.id.clone(), row.seq) children=move |row| {
                                let href = format!("/investigations/{}", row.id);
                                let status = row.status;
                                view! {
                                    <tr class="tbl-row">
                                        <td class="inv-title"><a class="row-stretch" href=href>{row.title}</a></td>
                                        <td><Badge tone=investigation_status_tone(status)>{investigation_status_label(status)}</Badge></td>
                                        <td><When ts=row.updated_at/></td>
                                        <td class="inv-who">{row.updated_by.name}</td>
                                    </tr>
                                }
                            }/>
                        </tbody>
                    </table>
                    {move || list.get().is_some_and(|l| l.investigations.is_empty()).then(|| view! {
                        <div class="tbl-empty">"No investigations yet. An agent opens one through trawl mcp."</div>
                    })}
                    <Pager summary=Signal::derive(move || {
                        let count = rows.get().len();
                        format!("{count} investigation{}", if count == 1 { "" } else { "s" })
                    })/>
                </div>
                </div>
            </section>
        </div>
        </Show>
    }
}

#[component]
pub fn InvestigationPage() -> impl IntoView {
    let permitted = permitted();
    let params = use_params_map();
    let id = Memo::new(move |_| params.get().get("id").unwrap_or_default());
    view! {
        <Show when=move || permitted.get() fallback=|| view! { <NotPermitted/> }>
            // A fresh owner per id: nothing read for one investigation can
            // land on the page of the next.
            <For each=move || vec![id.get()] key=String::clone children=move |raw| {
                match InvestigationId::parse(&raw) {
                    Ok(id) => view! { <InvestigationDetail id=id/> }.into_any(),
                    Err(_) => view! { <Missing/> }.into_any(),
                }
            }/>
        </Show>
    }
}

/// The page for an id the installation does not hold, or one that is not
/// an id at all.
#[component]
fn Missing() -> impl IntoView {
    view! {
        <div class="page inv-page">
            <div class="page-hd compact">
                <div><h1>"Investigation not found"</h1></div>
            </div>
            <p role="status">"No investigation has that id. " <a href="/investigations">"Back to Investigations"</a></p>
        </div>
    }
}

/// One cited row's state on this page.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Evidence {
    Loading,
    Row(EvidenceRow),
    /// The result's evidence file is gone (410 `result_gone`).
    Pruned,
    /// The result answered, but holds no row at that number.
    NoRow,
    Failed(String),
}

/// The page's reads of the rows its findings cite, through one
/// [`EvidenceQueue`]: at most four at a time, a row once however many
/// findings cite it, and a 429 pauses every read until its `Retry-After`
/// and asks for the row again instead of failing it. When the queue
/// stalls, `stalled` holds how many rows wait until [`Self::resume`]. A
/// row shows [`Evidence::Loading`] from the moment it is asked for until
/// it is answered. Leaving the page cancels the queue and aborts the reads in
/// flight; an answer that still lands finds the page's state gone and is
/// dropped. A row that lands, and the notice of a stalled queue, change
/// the page out of the reader's hands, so both keep their place
/// ([`Place`]).
///
/// While the board's tab is hidden no read starts. [`Self::pump`] is the
/// one place a read starts, and it starts none then: whatever asked, a
/// finding a read brought, Refresh, Retry or a 429's pause ending, the
/// rows wait in the queue, and the queue pumps again when the board's
/// visibility listener says the tab shows. A 429's pause runs on the
/// clock meanwhile, so a row waits for whichever ends last.
#[derive(Clone, Copy)]
struct EvidenceReads {
    queue: StoredValue<EvidenceQueue<EvidenceRef>>,
    abort: StoredValue<Option<web_sys::AbortController>, LocalStorage>,
    evidence: RwSignal<HashMap<EvidenceRef, Evidence>>,
    stalled: RwSignal<Option<usize>>,
    page: NodeRef<html::Div>,
    /// Whether the board's tab shows ([`BoardReads`]'s `visible`).
    visible: Signal<bool>,
}

impl EvidenceReads {
    fn new(
        evidence: RwSignal<HashMap<EvidenceRef, Evidence>>,
        page: NodeRef<html::Div>,
        visible: Signal<bool>,
    ) -> Self {
        let reads = Self {
            queue: StoredValue::new(EvidenceQueue::new()),
            abort: StoredValue::new_local(web_sys::AbortController::new().ok()),
            evidence,
            stalled: RwSignal::new(None),
            page,
            visible,
        };
        on_cleanup(move || {
            reads.queue.try_update_value(EvidenceQueue::cancel);
            reads.abort.try_with_value(|abort| {
                if let Some(abort) = abort {
                    abort.abort();
                }
            });
        });
        // The tab shows: start what waited for it.
        Effect::new(move |_| {
            if visible.get() {
                untrack(|| reads.pump());
            }
        });
        reads
    }

    /// Ask for a row: it shows as loading until it is answered.
    fn want(self, reference: EvidenceRef) {
        let _ = self.evidence.try_update(|map| {
            map.insert(reference.clone(), Evidence::Loading);
        });
        if self.queue.try_update_value(|q| q.push(reference)) == Some(true) {
            self.pump();
        }
    }

    /// Read the waiting rows again after the queue stalled.
    fn resume(self) {
        self.queue
            .try_update_value(|q| q.resume(js_sys::Date::now()));
        let _ = self.stalled.try_set(None);
        self.pump();
    }

    /// Start every read the queue allows now, arm its timer when a 429
    /// paused it, and say how many rows wait when it stalled. While the
    /// tab is hidden, nothing: the rows wait for it to show.
    fn pump(self) {
        if self.visible.try_get_untracked() != Some(true) {
            return;
        }
        loop {
            let Some(next) = self.queue.try_update_value(|q| q.next(js_sys::Date::now())) else {
                return;
            };
            match next {
                Next::Start(reference) => self.start(reference),
                Next::WakeAt(at) => {
                    let wait = wait_ms(js_sys::Date::now(), at);
                    spawn_local(async move {
                        gloo_timers::future::TimeoutFuture::new(wait).await;
                        if self.queue.try_update_value(EvidenceQueue::woke) == Some(true) {
                            self.pump();
                        }
                    });
                    return;
                }
                Next::Stalled { waiting } => {
                    let place = Place::of(self.page);
                    let _ = self.stalled.try_set(Some(waiting));
                    if let Some(place) = place {
                        place.keep();
                    }
                    return;
                }
                Next::Idle => return,
            }
        }
    }

    fn start(self, reference: EvidenceRef) {
        let signal = self
            .abort
            .try_with_value(|abort| abort.as_ref().map(web_sys::AbortController::signal))
            .flatten();
        spawn_local(async move {
            let paced = api::result_page(
                &reference.result_id,
                reference.row,
                Some(1),
                signal.as_ref(),
            )
            .await;
            let result = match paced {
                Paced::Throttled { retry_after_ms } => {
                    let now = js_sys::Date::now();
                    if self
                        .queue
                        .try_update_value(|q| q.throttled(reference, now, retry_after_ms))
                        == Some(true)
                    {
                        self.pump();
                    }
                    return;
                }
                Paced::Answered(result) => result,
            };
            // Gone with the page, or cancelled: drop the answer.
            if self.queue.try_update_value(|q| q.settled(&reference)) != Some(true) {
                return;
            }
            if sign_in_if_unauthorized(&result) {
                return;
            }
            let state = match result {
                Ok(page) => evidence_rows(&page)
                    .row(reference.row)
                    .map_or(Evidence::NoRow, Evidence::Row),
                Err(e) if e.http_status() == Some(410) => Evidence::Pruned,
                Err(e) => Evidence::Failed(e.to_string()),
            };
            let place = Place::of(self.page);
            let _ = self.evidence.try_update(|map| map.insert(reference, state));
            if let Some(place) = place {
                place.keep();
            }
            self.pump();
        });
    }
}

/// The notice a stalled evidence queue shows.
fn stalled_text(waiting: usize) -> String {
    let rows = if waiting == 1 {
        "1 cited row is".to_owned()
    } else {
        format!("{waiting} cited rows are")
    };
    format!("The server is limiting reads for your API key, so {rows} waiting.")
}

/// Where the reader is on the board: the `data-anchor` key of the element
/// that holds their place ([`pick_anchor`]) and how far below the
/// scroller's top edge it stands.
struct Place {
    scroller: web_sys::Element,
    key: String,
    offset: f64,
}

impl Place {
    /// The reader's place, measured before a change they did not ask for.
    /// `None` before the board is on the page, and at the top, which
    /// stays the top.
    fn of(page: NodeRef<html::Div>) -> Option<Self> {
        let scroller: web_sys::Element = page.try_get_untracked().flatten()?.into();
        if scroller.scroll_top() <= 0 {
            return None;
        }
        let line = scroller.get_bounding_client_rect().top();
        let nodes = scroller.query_selector_all("[data-anchor]").ok()?;
        let keyed: Vec<web_sys::Element> = (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into().ok())
            .collect();
        let placed: Vec<Placed> = keyed
            .iter()
            .enumerate()
            .map(|(i, element)| {
                let rect = element.get_bounding_client_rect();
                Placed {
                    top: rect.top(),
                    bottom: rect.bottom(),
                    holds_next: keyed
                        .get(i + 1)
                        .is_some_and(|next| element.contains(Some(next))),
                }
            })
            .collect();
        let at = pick_anchor(&placed, line)?;
        Some(Self {
            key: keyed[at].get_attribute("data-anchor")?,
            offset: placed[at].top - line,
            scroller,
        })
    }

    /// Once the change is on the page, scroll by however far it moved the
    /// element that holds the reader's place, so what they read stays
    /// where it was. The change's own DOM update is queued before this,
    /// and no scroll or tap of the person's can run between the two.
    fn keep(self) {
        queue_microtask(move || {
            if !self.scroller.is_connected() {
                return;
            }
            let selector = format!("[data-anchor=\"{}\"]", self.key);
            let Ok(Some(anchor)) = self.scroller.query_selector(&selector) else {
                return;
            };
            let line = self.scroller.get_bounding_client_rect().top();
            let moved = anchor.get_bounding_client_rect().top() - line - self.offset;
            if moved.abs() > f64::EPSILON {
                self.scroller.scroll_by_with_x_and_y(0.0, moved);
            }
        });
    }
}

/// What the first read of an investigation said, before there is a board
/// to show.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FirstRead {
    Pending,
    Missing,
    /// Refused with 403: the not-permitted page, with nothing to retry.
    Forbidden,
    Failed(String),
}

/// What a read the server throttled says when a person asked for it.
const THROTTLED: &str = "The server is limiting reads for your API key.";

/// Whether the board's tab is visible. The one place the board asks:
/// it reads nothing while this is false ([`BoardReads::defer`]).
fn tab_visible() -> bool {
    !document().hidden()
}

/// A read of the board someone asked for, which waits while the tab is
/// hidden ([`BoardReads::defer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// A person's read, `pages` reads into its run: none for mount,
    /// Refresh, Load more and Retry, more for the page that follows a
    /// short read.
    Person { pages: u32 },
    /// The read after a write of the person's ([`BoardReads::after_write`]).
    Write,
}

impl Ask {
    /// The one read that does the work of both `self` and `other`. Each
    /// replaces any read in flight and reads from the board's cursor, so
    /// a person's read does a write's work too; of two person's reads,
    /// the one earlier in its run reads at least as far.
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Person { pages: a }, Self::Person { pages: b }) => {
                Self::Person { pages: a.min(b) }
            }
            (person @ Self::Person { .. }, Self::Write) | (Self::Write, person) => person,
        }
    }
}

/// Who started a read of the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reader {
    /// Mount, Refresh, Load more or Retry: the page shows that it reads,
    /// and follows a short read at once.
    Person,
    /// The board's own read while its tab is visible: one read, quiet.
    Poll,
}

/// The board's reads of its investigation.
///
/// One read runs at a time. A person's read replaces a poll in flight,
/// aborting it; a poll that comes due while a person's read runs is
/// skipped, and the next one is scheduled when that read ends. Every read
/// asks for the entries changed since the board's `through_seq` and
/// merges what it brings ([`Board::merge`]); an answer for a read that a
/// later one replaced, or that lands after the page is gone, is dropped.
/// A person's read sets `busy` and counts `pages`, so the page says it is
/// reading and Load more shows its progress; a poll touches neither.
///
/// The schedule is [`Liveness`]: a poll [`POLL_MS`](crate::investigation_live::POLL_MS)
/// after a read ends while the tab is visible, none while it is hidden,
/// and one at once when it shows again.
///
/// No other read starts in a hidden tab either. Every read a person or a
/// write asks for passes one gate, [`Self::defer`], which while the tab
/// is hidden starts nothing and records the read as `owed` instead, and
/// so does the page that would follow a short read. When the tab shows,
/// [`Self::visibility_changed`] starts the owed read at once in place of
/// the poll, replacing any read still in flight, which began before it
/// was asked for. That holds before there is a board too: a board opened
/// in a hidden tab says Paused, not busy, and reads when it shows. The
/// gate is one place rather than a check in each caller because the
/// rule has no exception: a caller added later cannot skip it. A
/// deferred read claims no generation until it starts, so a write whose
/// answer was lost stays held until that read lands, and a read that
/// began before the write never releases it ([`Self::mark`]).
///
/// The board's listener is the page's only one. It keeps `visible`,
/// which the board lends to its evidence reads, whose gates hold every
/// read while it is false and start what waited once it is true
/// ([`EvidenceReads`], [`EvidenceDrawer`]).
///
/// A read that failed keeps the
/// board, its cursor and the instant it was last checked. A 401 sends
/// the page to sign-in; a 403 stops the reads and leaves the board as
/// read; a 404 ends the visit as missing. Leaving the page, or opening
/// another investigation, drops the timer and the visibility listener
/// and aborts the read in flight.
///
/// What a poll changes, the reader did not ask for, so a poll keeps their
/// place on the page ([`Place`]). A person's own read does not: Load more
/// grows the log under the control they pressed.
#[derive(Clone, Copy)]
struct BoardReads {
    id: StoredValue<InvestigationId>,
    /// The board's scroller, once the board is on the page.
    page: NodeRef<html::Div>,
    board: RwSignal<Option<Board>>,
    first: RwSignal<FirstRead>,
    stale: RwSignal<Option<String>>,
    busy: RwSignal<bool>,
    /// Reads merged since the person last asked for one.
    pages: RwSignal<u32>,
    live: RwSignal<Liveness>,
    /// Whether the tab shows, as the visibility listener last saw it.
    visible: RwSignal<bool>,
    /// What the polite live region says.
    said: RwSignal<String>,
    /// Each read takes the next generation; only the newest may land.
    generation: StoredValue<u64>,
    /// The generation of the newest read the board accepted.
    accepted: RwSignal<u64>,
    in_flight: StoredValue<Option<Reader>>,
    /// The read asked for while the tab was hidden, which starts when it
    /// shows.
    owed: StoredValue<Option<Ask>>,
    abort: StoredValue<Option<web_sys::AbortController>, LocalStorage>,
    timer: StoredValue<Option<gloo_timers::callback::Timeout>, LocalStorage>,
}

impl BoardReads {
    fn new(id: InvestigationId) -> Self {
        let reads = Self {
            id: StoredValue::new(id),
            page: NodeRef::new(),
            board: RwSignal::new(None),
            first: RwSignal::new(FirstRead::Pending),
            stale: RwSignal::new(None),
            busy: RwSignal::new(false),
            pages: RwSignal::new(0),
            live: RwSignal::new(Liveness::new(tab_visible())),
            visible: RwSignal::new(tab_visible()),
            said: RwSignal::new(String::new()),
            generation: StoredValue::new(0),
            accepted: RwSignal::new(0),
            in_flight: StoredValue::new(None),
            owed: StoredValue::new(None),
            abort: StoredValue::new_local(None),
            timer: StoredValue::new_local(None),
        };
        let document = document();
        let listener = Closure::<dyn FnMut()>::new(move || reads.visibility_changed());
        let _ = document.add_event_listener_with_callback(
            "visibilitychange",
            listener.as_ref().unchecked_ref(),
        );
        let listening = StoredValue::new_local((document, listener));
        on_cleanup(move || {
            let _ = reads.timer.try_set_value(None);
            reads.abort.try_update_value(|abort| {
                if let Some(abort) = abort.take() {
                    abort.abort();
                }
            });
            listening.try_with_value(|(document, listener)| {
                let _ = document.remove_event_listener_with_callback(
                    "visibilitychange",
                    listener.as_ref().unchecked_ref(),
                );
            });
        });
        reads
    }

    /// Claim the next generation for a read by `reader`: drop the
    /// pending poll and abort the read in flight. `None` once the page is
    /// gone.
    fn begin(self, reader: Reader) -> Option<(u64, Option<web_sys::AbortSignal>)> {
        let _ = self.timer.try_set_value(None);
        let signal = self.abort.try_update_value(|abort| {
            if let Some(old) = abort.take() {
                old.abort();
            }
            *abort = web_sys::AbortController::new().ok();
            abort.as_ref().map(web_sys::AbortController::signal)
        })?;
        let mine = self.generation.try_update_value(|g| {
            *g = g.wrapping_add(1);
            *g
        })?;
        let _ = self.in_flight.try_set_value(Some(reader));
        Some((mine, signal))
    }

    /// The newest generation begun so far. A write whose answer was lost
    /// takes it as its mark: only a read begun after the answer failed
    /// can show what the write did, so only an accepted generation past
    /// the mark releases it.
    fn mark(self) -> u64 {
        self.generation.try_get_value().unwrap_or(u64::MAX)
    }

    /// Whether the read with generation `mine` is still the board's.
    fn current(self, mine: u64) -> bool {
        self.generation.try_get_value() == Some(mine)
    }

    fn since(self) -> u64 {
        self.board
            .with_untracked(|b| b.as_ref().map_or(0, |b| b.through_seq))
    }

    /// Show the investigation's fields from a write's answer at once,
    /// keeping the reader's place ([`Board::adopt`]); the cursor moves
    /// only with the read that follows.
    fn adopt(self, investigation: Investigation) {
        let place = Place::of(self.page);
        let _ = self
            .board
            .try_maybe_update(|board| (board.as_mut().is_some_and(|b| b.adopt(investigation)), ()));
        if let Some(place) = place {
            place.keep();
        }
    }

    /// Fold a read into the board; how many entries it added.
    fn merge(self, read: InvestigationRead) -> usize {
        let mut added = 0;
        self.board.maybe_update(|board| {
            if let Some(board) = board {
                let merged = board.merge(read);
                added = merged.added;
                merged.changed
            } else {
                *board = Some(Board::new(read));
                true
            }
        });
        self.stale.set(None);
        added
    }

    /// A read the board was asked for: at once while the tab is visible,
    /// else when it shows ([`Self::defer`]). A write's read is dropped
    /// when there is no board or it stopped reading.
    fn read(self, ask: Ask) {
        if ask == Ask::Write && !self.readable() {
            return;
        }
        if self.defer(ask) {
            return;
        }
        match ask {
            Ask::Person { pages } => self.person(pages),
            Ask::Write => {
                // A read the person started is replaced: it no longer
                // reads.
                if self.in_flight.try_get_value() == Some(Some(Reader::Person)) {
                    let _ = self.busy.try_set(false);
                }
                self.quiet();
            }
        }
    }

    /// The gate every read but the poll passes, the poll being armed only
    /// in a visible tab: while the tab is hidden it starts nothing, owes
    /// `ask` for when the tab shows, says the board is paused, and
    /// answers `true`.
    fn defer(self, ask: Ask) -> bool {
        if tab_visible() {
            return false;
        }
        self.owed
            .try_update_value(|owed| *owed = Some(owed.map_or(ask, |o| o.and(ask))));
        let _ = self.live.try_update(Liveness::hidden);
        true
    }

    /// Whether a quiet read has a board to read into.
    fn readable(self) -> bool {
        self.board.try_with_untracked(Option::is_some) == Some(true)
            && self.live.try_with_untracked(Liveness::stopped) == Some(false)
    }

    /// A read the person asked for, `pages` reads into its run: on mount,
    /// Refresh, Load more and Retry from the start. A read that leaves
    /// entries unread is followed at once, up to
    /// [`AUTO_PAGES`](crate::investigation_log::AUTO_PAGES) reads; a page
    /// that falls due in a hidden tab is owed instead.
    fn person(self, pages: u32) {
        let mut since = self.since();
        let Some(id) = self.id.try_get_value() else {
            return;
        };
        let Some((mine, signal)) = self.begin(Reader::Person) else {
            return;
        };
        let _ = self.busy.try_set(true);
        let _ = self.pages.try_set(pages);
        spawn_local(async move {
            let end = loop {
                let paced = api::read_investigation(&id, since, ENTRY_PAGE, signal.as_ref()).await;
                if !self.current(mine) {
                    return;
                }
                let result = match paced {
                    Paced::Throttled { retry_after_ms } => {
                        self.say_failure(THROTTLED.to_owned());
                        break ReadEnd::Throttled { retry_after_ms };
                    }
                    Paced::Answered(result) => result,
                };
                if sign_in_if_unauthorized(&result) {
                    let _ = self.timer.try_set_value(None);
                    return;
                }
                match result {
                    Ok(read) => {
                        self.merge(read);
                        self.pages.update(|p| *p += 1);
                        let next = self.board.with_untracked(|b| {
                            b.as_ref()
                                .filter(|b| b.follow_up_due(self.pages.get_untracked()))
                                .map(|b| b.through_seq)
                        });
                        if let Some(next) = next {
                            let pages = self.pages.get_untracked();
                            if self.defer(Ask::Person { pages }) {
                                break ReadEnd::Accepted;
                            }
                            since = next;
                            continue;
                        }
                        break ReadEnd::Accepted;
                    }
                    Err(e) => break self.failed(&e, Reader::Person),
                }
            };
            self.busy.set(false);
            self.finish(mine, end, 0);
        });
    }

    /// The board's own read, when one is due: skipped while another read
    /// runs, while the tab is hidden, before the board exists and once it
    /// stopped reading.
    fn poll(self) {
        // Gone with the page, or another read runs.
        if self.in_flight.try_get_value() != Some(None) {
            return;
        }
        if !self.readable() || !tab_visible() {
            return;
        }
        self.quiet();
    }

    /// The read after a write of the person's landed, or was refused
    /// because the board was behind: at once, whatever read was running,
    /// since that one started before the write, or as soon as a hidden
    /// tab shows. It is quiet like a poll and keeps the reader's place,
    /// because the person is acting in the viewer or the header, not
    /// reading the log.
    ///
    /// A write's answer can arrive after the person left the page, so
    /// this reads nothing of a board that is gone: every access on the
    /// way is the disposal-safe one, and a disposed board does nothing.
    fn after_write(self) {
        self.read(Ask::Write);
    }

    /// One quiet read since the board's `through_seq`, replacing any read
    /// in flight, that keeps the reader's place when it lands.
    fn quiet(self) {
        let since = self.since();
        let Some(id) = self.id.try_get_value() else {
            return;
        };
        let Some((mine, signal)) = self.begin(Reader::Poll) else {
            return;
        };
        spawn_local(async move {
            let paced = api::read_investigation(&id, since, ENTRY_PAGE, signal.as_ref()).await;
            if !self.current(mine) {
                return;
            }
            let place = Place::of(self.page);
            let (end, added) = match paced {
                Paced::Throttled { retry_after_ms } => (ReadEnd::Throttled { retry_after_ms }, 0),
                Paced::Answered(result) => {
                    if sign_in_if_unauthorized(&result) {
                        let _ = self.timer.try_set_value(None);
                        return;
                    }
                    match result {
                        Ok(read) => (ReadEnd::Accepted, self.merge(read)),
                        Err(e) => (self.failed(&e, Reader::Poll), 0),
                    }
                }
            };
            self.finish(mine, end, added);
            if let Some(place) = place {
                place.keep();
            }
        });
    }

    /// What a failed read does to the page, and how it ended. A person
    /// hears why their read failed; a failed poll shows only in the
    /// connection state.
    fn failed(self, error: &ApiError, reader: Reader) -> ReadEnd {
        let held = self.board.with_untracked(Option::is_some);
        match error.http_status() {
            Some(404) => {
                self.board.set(None);
                self.first.set(FirstRead::Missing);
                ReadEnd::Refused
            }
            Some(403) if held => {
                self.stale.set(Some(NOT_PERMITTED.to_owned()));
                ReadEnd::Refused
            }
            Some(403) => {
                self.first.set(FirstRead::Forbidden);
                ReadEnd::Refused
            }
            _ => {
                if reader == Reader::Person {
                    self.say_failure(read_failure(error));
                }
                ReadEnd::Failed
            }
        }
    }

    /// A person's read failed with `message`: over the board when there
    /// is one, else in its place.
    fn say_failure(self, message: String) {
        if self.board.with_untracked(Option::is_some) {
            self.stale.set(Some(message));
        } else {
            self.first.set(FirstRead::Failed(message));
        }
    }

    /// A read ended: nothing is in flight, the next poll is scheduled,
    /// and the live region says what changed, if anything did.
    fn finish(self, mine: u64, end: ReadEnd, added: usize) {
        let _ = self.in_flight.try_set_value(None);
        if end == ReadEnd::Accepted {
            let _ = self.accepted.try_set(mine);
        }
        let Some(before) = self.live.try_with_untracked(Liveness::connection) else {
            return;
        };
        let now = js_sys::Date::now();
        self.live.update(|live| live.ended(end, now));
        self.schedule();
        let after = self.live.with_untracked(Liveness::connection);
        let words: Vec<String> = announcement(before, after)
            .map(str::to_owned)
            .into_iter()
            .chain(new_entries_text(added))
            .collect();
        if !words.is_empty() {
            self.say(&words.join(" "));
        }
    }

    /// Put `text` in the live region. The same words twice in a row
    /// would leave its text as it was, which a screen reader does not
    /// announce, so the repeat carries a trailing no-break space.
    fn say(self, text: &str) {
        self.said.update(|said| {
            *said = if said == text {
                format!("{text}\u{a0}")
            } else {
                text.to_owned()
            };
        });
    }

    /// Arm the next poll: none while the tab is hidden, before there is
    /// a board, or once the board stopped reading.
    fn schedule(self) {
        let _ = self.timer.try_set_value(None);
        if self.board.try_with_untracked(Option::is_none) != Some(false) {
            return;
        }
        let now = js_sys::Date::now();
        let visible = tab_visible();
        let delay = self
            .live
            .try_update(|live| {
                if visible {
                    live.next_delay(now)
                } else {
                    live.hidden();
                    None
                }
            })
            .flatten();
        if let Some(ms) = delay {
            self.arm(ms);
        }
    }

    fn arm(self, ms: u32) {
        let _ = self
            .timer
            .try_set_value(Some(gloo_timers::callback::Timeout::new(ms, move || {
                self.poll();
            })));
    }

    /// The tab was hidden or shown. Hidden, the board drops its pending
    /// poll. Shown, it starts at once the read it owes, with or without a
    /// board and whatever runs. Else it polls at once, unless a read is
    /// still running, whose end schedules the next, or a 429's pause has
    /// time left. Either way `visible` follows, which holds or starts the
    /// evidence reads.
    fn visibility_changed(self) {
        let visible = tab_visible();
        let _ = self.visible.try_set(visible);
        if !visible {
            self.schedule();
            return;
        }
        let now = js_sys::Date::now();
        let Some(wait) = self.live.try_update(|live| live.shown(now)) else {
            return;
        };
        if let Some(ask) = self.owed.try_update_value(Option::take).flatten() {
            self.read(ask);
            return;
        }
        if self.board.try_with_untracked(Option::is_none) != Some(false) {
            return;
        }
        match wait {
            Some(0) => self.poll(),
            Some(ms) => self.arm(ms),
            None => {}
        }
    }
}

#[component]
#[allow(clippy::too_many_lines)] // one page's view tree
fn InvestigationDetail(id: InvestigationId) -> impl IntoView {
    let reads = BoardReads::new(id);
    let BoardReads {
        page,
        board,
        first,
        stale,
        busy,
        pages,
        live,
        said,
        ..
    } = reads;
    let visible: Signal<bool> = reads.visible.into();
    let evidence = RwSignal::new(HashMap::<EvidenceRef, Evidence>::new());
    let read = Callback::new(move |()| reads.read(Ask::Person { pages: 0 }));
    // The evidence viewer: the result it is open on, if any, and the
    // server's page size once a viewer read has reported it.
    let viewing = RwSignal::new(None::<Opened>);
    let page_size = RwSignal::new(None::<NonZeroUsize>);
    let open = Callback::new(move |opened: Opened| viewing.set(Some(opened)));
    let close = Callback::new(move |()| viewing.set(None));

    // Writes need `investigation_write` beside the read grants, and a
    // board that still reads: once a read was refused for want of a
    // grant, no write control stays.
    let me = expect_context::<RwSignal<Option<api::MeResponse>>>();
    let writer = Signal::derive(move || {
        me.with(|m| {
            m.as_ref()
                .is_some_and(|m| perms::can_write_investigations(&m.permissions))
        }) && !live.with(Liveness::stopped)
    });
    let accepted: Signal<u64> = reads.accepted.into();
    let keep = KeepGate {
        investigation: reads.id,
        open: Signal::derive(move || {
            writer.get()
                && board.with(|b| {
                    b.as_ref()
                        .is_some_and(|b| b.investigation.status == InvestigationStatus::Open)
                })
        }),
        accepted,
        mark: Callback::new(move |()| reads.mark()),
        landed: Callback::new(move |()| reads.after_write()),
        refresh: read,
    };
    read.run(());

    let rows = EvidenceReads::new(evidence, page, visible);
    // Every row the findings cite is asked for once, the first time a
    // read brings the finding that cites it.
    Effect::new(move |_| {
        let wanted: Vec<EvidenceRef> = board.with(|b| {
            b.as_ref()
                .map(|b| {
                    b.findings()
                        .into_iter()
                        .flat_map(|f| f.references.into_iter().chain(f.counterevidence))
                        .collect()
                })
                .unwrap_or_default()
        });
        let missing: Vec<EvidenceRef> = evidence.with_untracked(|map| {
            wanted
                .into_iter()
                .filter(|r| !map.contains_key(r))
                .collect()
        });
        for reference in missing {
            rows.want(reference);
        }
    });
    let retry_row = Callback::new(move |reference| rows.want(reference));
    let resume_reads = Callback::new(move |()| rows.resume());
    // Refresh reads the log again and asks again for every row that
    // failed; Load more only reads the log.
    let refresh = Callback::new(move |()| {
        let failed: Vec<EvidenceRef> = evidence.with_untracked(|map| {
            map.iter()
                .filter(|(_, state)| matches!(state, Evidence::Failed(_)))
                .map(|(reference, _)| reference.clone())
                .collect()
        });
        for reference in failed {
            rows.want(reference);
        }
        read.run(());
    });

    // Lists render by the `seq` that created each item, and each item
    // reads its own current copy, so a step that finishes or a finding
    // whose status changes updates where it stands: an open step stays
    // open and a focused row keeps its focus. The same `seq` keys each
    // item's `data-anchor`, beside one key per section, which is how a
    // change the reader did not ask for finds their place again
    // ([`Place`]).
    let findings = Memo::new(move |_| {
        board.with(|b| {
            b.as_ref()
                .map(|b| b.findings().into_iter().map(|f| (f.seq, f)).collect())
                .unwrap_or_default()
        })
    });
    let finding_seqs = Memo::new(move |_| {
        findings.with(|m: &BTreeMap<u64, Finding>| m.keys().copied().collect::<Vec<_>>())
    });
    let activity = Memo::new(move |_| {
        board.with(|b| {
            b.as_ref()
                .map(|b| b.activity().into_iter().map(|a| (a.seq, a)).collect())
                .unwrap_or_default()
        })
    });
    let activity_seqs = Memo::new(move |_| {
        activity.with(|m: &BTreeMap<u64, Activity>| m.keys().copied().collect::<Vec<_>>())
    });
    let steps = Memo::new(move |_| {
        board.with(|b| {
            b.as_ref()
                .map(|b| b.steps().into_iter().map(|e| (e.seq, e)).collect())
                .unwrap_or_default()
        })
    });
    let step_seqs = Memo::new(move |_| {
        steps.with(|m: &BTreeMap<u64, LogEntry>| m.keys().copied().collect::<Vec<_>>())
    });
    let latest = Memo::new(move |_| board.with(|b| b.as_ref().and_then(Board::latest_finding)));
    let more = Signal::derive(move || board.with(|b| b.as_ref().is_some_and(|b| b.more)));
    let reach = Signal::derive(move || {
        board.with(|b| {
            b.as_ref()
                .map_or(Reach::Reading, |b| b.reach(pages.get(), busy.get()))
        })
    });
    let as_of = move || match reach.get() {
        Reach::AsOf(seq) => format!(" as of entry #{seq}"),
        Reach::Whole | Reach::Reading => String::new(),
    };
    let still_reading = || view! { <p class="inv-empty" role="status">"Reading the log…"</p> };
    let field = move |get: fn(&trawl_api::investigations::Investigation) -> String| {
        move || {
            board.with(|b| {
                b.as_ref()
                    .map(|b| get(&b.investigation))
                    .unwrap_or_default()
            })
        }
    };
    let status = Signal::derive(move || {
        board.with(|b| {
            b.as_ref()
                .map_or(InvestigationStatus::Open, |b| b.investigation.status)
        })
    });
    let connection = Memo::new(move |_| live.with(Liveness::connection));
    let status_note = RwSignal::new(None::<StatusNote>);
    // When the last read was accepted, as an instant `When` reads.
    let checked = Memo::new(move |_| {
        live.with(Liveness::last_ok).map(|ms| {
            String::from(js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms)).to_iso_string())
        })
    });

    view! {
        <Show when=move || board.with(Option::is_some) fallback=move || match first.get() {
            FirstRead::Missing => view! { <Missing/> }.into_any(),
            FirstRead::Forbidden => view! { <NotPermitted/> }.into_any(),
            // A first read owed to a hidden tab: paused, not loading.
            FirstRead::Pending => view! {
                <div class="page inv-page">
                    <Show when=move || connection.get() == Connection::Paused
                        fallback=|| view! { <p role="status">"Loading investigation…"</p> }>
                        <p class="inv-live"><Badge tone=Connection::Paused.tone()>{Connection::Paused.label()}</Badge></p>
                        <p role="status">"The investigation loads when this tab is shown."</p>
                    </Show>
                </div>
            }.into_any(),
            FirstRead::Failed(message) => view! {
                <div class="page inv-page">
                    <div class="page-hd compact"><div><h1>"Investigation"</h1></div></div>
                    <Loaded state=Signal::stored(LoadState::<()>::Error(message)) label="investigation"
                        retry=Callback::new(move |()| { first.set(FirstRead::Pending); read.run(()); })
                        render=Box::new(|()| ().into_any())/>
                </div>
            }.into_any(),
        }>
        <div class="page inv-page" node_ref=page aria-busy=move || busy.get().to_string()>
            <div class="page-hd compact">
                <div class="inv-hd">
                    <p class="inv-crumb"><a href="/investigations">"Investigations"</a></p>
                    <h1>{field(|i| i.title.clone())}</h1>
                    <p class="sub">
                        {move || {
                            let s = status.get();
                            view! { <Badge tone=investigation_status_tone(s)>{investigation_status_label(s)}</Badge> }
                        }}
                        <Show when=move || writer.get()>
                            <StatusControl reads=reads note=status_note/>
                        </Show>
                        " Opened by " {field(|i| i.created_by.name.clone())}
                        " · updated " <When ts=Signal::derive(field(|i| i.updated_at.clone())) mode=WhenMode::Absolute/>
                        " by " {field(|i| i.updated_by.name.clone())}
                    </p>
                    <p class="inv-live">
                        {move || {
                            let c = connection.get();
                            view! { <Badge tone=c.tone()>{c.label()}</Badge> }
                        }}
                        " "
                        <Show when=move || checked.with(Option::is_some)>
                            <span class="inv-meta">
                                "checked " <When ts=Signal::derive(move || checked.get().unwrap_or_default())/>
                            </span>
                        </Show>
                    </p>
                    <p class="sr-only" role="status">{move || said.get()}</p>
                    <StatusNoteView note=status_note accepted=accepted refresh=refresh/>
                </div>
                <div class="actions">
                    <Btn variant=Variant::Secondary size=Size::Sm on_click=refresh>"Refresh"</Btn>
                </div>
            </div>
            {move || stale.get().map(|message| view! {
                <p class="inv-stale" role="status">"Refresh failed. Showing what was read before. " {message}</p>
            })}

            <section class="inv-sec" data-anchor="question" aria-labelledby="inv-question-h">
                <h2 id="inv-question-h" class="inv-sec-h">"Question"</h2>
                <p class="inv-question">{field(|i| i.question.clone())}</p>
                {move || {
                    let scope = field(|i| i.scope.clone())();
                    (!scope.is_empty()).then(|| view! {
                        <h3 class="inv-sub-h">"Scope"</h3>
                        <p class="inv-scope">{scope}</p>
                    })
                }}
            </section>

            <section class="inv-sec" data-anchor="latest" aria-labelledby="inv-latest-h">
                <h2 id="inv-latest-h" class="inv-sec-h">"Latest finding" {as_of}</h2>
                {move || match (reach.get(), latest.get()) {
                    (Reach::Reading, _) => still_reading().into_any(),
                    (_, Some(finding)) => view! { <FindingHead finding=finding/> }.into_any(),
                    (_, None) => view! { <p class="inv-empty">"No findings yet."</p> }.into_any(),
                }}
            </section>

            <section class="inv-sec" data-anchor="activity" aria-labelledby="inv-activity-h">
                <h2 id="inv-activity-h" class="inv-sec-h">"Activity"</h2>
                <ol class="inv-activity" aria-label="Activity">
                    <For each=move || activity_seqs.get() key=|seq| *seq children=move |seq| {
                        let item = Memo::new(move |_| activity.with(|m| m.get(&seq).cloned()));
                        view! { <ActivityLine seq=seq item=item/> }
                    }/>
                </ol>
                <LoadMore has_more=more busy=busy on_load=read
                    empty=Signal::derive(move || activity_seqs.with(Vec::is_empty))
                    label="Load more entries" end_text="Every entry is loaded." empty_text="Nothing recorded yet."/>
            </section>

            <section class="inv-sec" data-anchor="findings" aria-labelledby="inv-findings-h">
                <h2 id="inv-findings-h" class="inv-sec-h">"Findings" {as_of}</h2>
                {move || rows.stalled.get().map(|waiting| view! {
                    <div class="inv-throttled">
                        <p role="status">{stalled_text(waiting)}</p>
                        <Btn variant=Variant::Secondary size=Size::Xs on_click=resume_reads>"Resume reading"</Btn>
                    </div>
                })}
                <Show when=move || reach.get() != Reach::Reading fallback=still_reading>
                    <Show when=move || !finding_seqs.with(Vec::is_empty)
                        fallback=|| view! { <p class="inv-empty">"No findings yet."</p> }>
                        <ol class="inv-findings" aria-label="Findings">
                            <For each=move || finding_seqs.get() key=|seq| *seq children=move |seq| {
                                let finding = Memo::new(move |_| findings.with(|m| m.get(&seq).cloned()));
                                view! {
                                    <li data-anchor=format!("finding-{seq}")>
                                        <FindingCard finding=finding evidence=evidence retry=retry_row open=open/>
                                    </li>
                                }
                            }/>
                        </ol>
                    </Show>
                </Show>
            </section>

            <section class="inv-sec" data-anchor="steps" aria-labelledby="inv-steps-h">
                <h2 id="inv-steps-h" class="inv-sec-h">"Steps"</h2>
                <Show when=move || !step_seqs.with(Vec::is_empty)
                    fallback=|| view! { <p class="inv-empty">"No steps yet."</p> }>
                    <ol class="inv-steps" aria-label="Steps">
                        <For each=move || step_seqs.get() key=|seq| *seq children=move |seq| {
                            let entry = Memo::new(move |_| steps.with(|m| m.get(&seq).cloned()));
                            view! { <StepItem seq=seq entry=entry open=open/> }
                        }/>
                    </ol>
                </Show>
            </section>
        </div>
        // A fresh viewer for each result it opens on.
        {move || viewing.get().map(|opened| view! {
            <EvidenceDrawer opened=opened page_size=page_size visible=visible on_close=close keep=keep/>
        })}
        </Show>
    }
}
/// What became of the person's last close or reopen. It stays until they
/// act again.
#[derive(Debug, Clone, PartialEq)]
enum StatusNote {
    Sending(InvestigationStatus),
    /// The server took it.
    Done(InvestigationStatus),
    /// Refused as stale: the board has read the investigation again.
    Conflict(InvestigationStatus, WriteFailure),
    /// No answer arrived. The mark is the board's newest read
    /// generation when it failed ([`BoardReads::mark`]): the control
    /// comes back once a later read lands.
    Unknown(InvestigationStatus, u64),
    Refused(InvestigationStatus, String),
}

/// The verb for the change to `target`.
fn status_verb(target: InvestigationStatus) -> &'static str {
    match target {
        InvestigationStatus::Closed => "close",
        InvestigationStatus::Open => "reopen",
    }
}

impl StatusNote {
    fn text(&self) -> Option<String> {
        Some(match self {
            Self::Sending(_) => return None,
            Self::Done(InvestigationStatus::Closed) => "You closed the investigation.".to_owned(),
            Self::Done(InvestigationStatus::Open) => "You reopened the investigation.".to_owned(),
            Self::Conflict(target, WriteFailure::Closed) => format!(
                "The investigation was already closed when your {} reached it, so nothing \
                 changed. The board has read it again.",
                status_verb(*target)
            ),
            Self::Conflict(target, _) => format!(
                "The investigation changed before your {} reached it, so nothing changed. The \
                 board has read it again: check its status and act again if you still mean to.",
                status_verb(*target)
            ),
            Self::Unknown(target, _) => format!(
                "The answer to your {} was lost, so it may or may not have taken effect. \
                 Refresh to see the investigation's status before you act again.",
                status_verb(*target)
            ),
            Self::Refused(target, message) => {
                format!(
                    "Could not {} the investigation. {message}",
                    status_verb(*target)
                )
            }
        })
    }
}

/// Close or Reopen, beside the status: one PATCH with the sequence the
/// board holds when it is pressed, never sent twice and never retried.
/// A 409 of either kind leaves a notice and reads the board again at
/// once, so the control the person presses next is for the status that
/// stands. A lost answer disables the control until a read lands after
/// it.
#[component]
fn StatusControl(reads: BoardReads, note: RwSignal<Option<StatusNote>>) -> impl IntoView {
    let board = reads.board;
    let status = Memo::new(move |_| board.with(|b| b.as_ref().map(|b| b.investigation.status)));
    let held = Signal::derive(move || match note.get() {
        Some(StatusNote::Sending(_)) => true,
        Some(StatusNote::Unknown(_, mark)) => reads.accepted.get() <= mark,
        _ => false,
    });
    let act = Callback::new(move |()| {
        if held.get_untracked() {
            return;
        }
        let Some((seq, current)) = board.with_untracked(|b| {
            b.as_ref()
                .map(|b| (b.investigation.seq, b.investigation.status))
        }) else {
            return;
        };
        let Some(id) = reads.id.try_get_value() else {
            return;
        };
        let target = match current {
            InvestigationStatus::Open => InvestigationStatus::Closed,
            InvestigationStatus::Closed => InvestigationStatus::Open,
        };
        note.set(Some(StatusNote::Sending(target)));
        // The answer can land after the page is gone: everything it
        // touches below is reached through disposal-safe access.
        spawn_local(async move {
            let result = api::patch_status(&id, seq, target).await;
            let outcome = match result {
                Ok(investigation) => {
                    reads.adopt(investigation);
                    reads.after_write();
                    StatusNote::Done(target)
                }
                Err(WriteFailure::Unauthorized) => {
                    crate::auth_return::redirect_to_login();
                    return;
                }
                Err(failure @ (WriteFailure::Stale { .. } | WriteFailure::Closed)) => {
                    reads.after_write();
                    StatusNote::Conflict(target, failure)
                }
                Err(WriteFailure::Unknown) => StatusNote::Unknown(target, reads.mark()),
                Err(WriteFailure::Refused(message)) => StatusNote::Refused(target, message),
            };
            let _ = note.try_set(Some(outcome));
        });
    });
    view! {
        {move || status.get().map(|current| {
            let (text, label) = match current {
                InvestigationStatus::Open => ("Close", "Close the investigation"),
                InvestigationStatus::Closed => ("Reopen", "Reopen the investigation"),
            };
            view! {
                <Btn variant=Variant::Secondary size=Size::Xs attr:class="inv-status-act" attr:aria-label=label
                    disabled=held on_click=act>{text}</Btn>
            }
        })}
    }
}

/// The notice of the person's last close or reopen, with a Refresh when
/// its outcome is unknown.
#[component]
fn StatusNoteView(
    note: RwSignal<Option<StatusNote>>,
    accepted: Signal<u64>,
    refresh: Callback<()>,
) -> impl IntoView {
    view! {
        {move || note.get().and_then(|n| {
            let unknown = match &n {
                StatusNote::Unknown(_, mark) => Some(*mark),
                _ => None,
            };
            let conflict = matches!(n, StatusNote::Conflict(..) | StatusNote::Refused(..) | StatusNote::Unknown(..));
            n.text().map(|text| view! {
                <div class="inv-write-note" class:inv-write-warn=conflict>
                    <p role="status">{text}</p>
                    {unknown.filter(|mark| accepted.get() <= *mark).map(|_| view! {
                        <Btn variant=Variant::Secondary size=Size::Xs on_click=refresh>"Refresh the board"</Btn>
                    })}
                </div>
            })
        })}
    }
}

/// A finding's status, author and statement.
#[component]
fn FindingHead(finding: Finding) -> impl IntoView {
    let status_by = finding.status_by.map(|actor| {
        view! { <p class="inv-meta">"Status set by " {actor.name}</p> }
    });
    view! {
        <div class="inv-finding-hd">
            <Badge tone=finding_status_tone(finding.status)>{finding_status_label(finding.status)}</Badge>
            <span class="inv-meta">{finding.author.name} " · " <When ts=finding.at mode=WhenMode::Absolute/></span>
        </div>
        <p class="inv-statement">{finding.statement}</p>
        {status_by}
        {finding.remaining_question.map(|question| view! {
            <p class="inv-open-q"><span class="inv-label">"Still open: "</span>{question}</p>
        })}
    }
}

/// A finding with every row it cites. The rows stay as they are when
/// the finding's status changes; only its head reads again.
#[component]
fn FindingCard(
    finding: Memo<Option<Finding>>,
    evidence: RwSignal<HashMap<EvidenceRef, Evidence>>,
    retry: Callback<EvidenceRef>,
    open: Callback<Opened>,
) -> impl IntoView {
    // A finding's citations are written with it and never change.
    let (references, counter) = finding
        .get_untracked()
        .map(|f| (f.references, f.counterevidence))
        .unwrap_or_default();
    view! {
        <article class="inv-finding">
            {move || finding.get().map(|finding| view! { <FindingHead finding=finding/> })}
            <EvidenceList title="Evidence" refs=references evidence=evidence retry=retry open=open/>
            {(!counter.is_empty()).then(|| view! {
                <EvidenceList title="Counterevidence" refs=counter evidence=evidence retry=retry open=open/>
            })}
        </article>
    }
}

#[component]
fn EvidenceList(
    title: &'static str,
    refs: Vec<EvidenceRef>,
    evidence: RwSignal<HashMap<EvidenceRef, Evidence>>,
    retry: Callback<EvidenceRef>,
    open: Callback<Opened>,
) -> impl IntoView {
    view! {
        <div class="inv-evidence">
            <h3 class="inv-sub-h">{title}</h3>
            {if refs.is_empty() {
                view! { <p class="inv-empty">"No rows cited."</p> }.into_any()
            } else {
                view! {
                    <ul class="inv-refs">
                        {refs.into_iter().map(|reference| view! {
                            <li><EvidenceItem reference=reference evidence=evidence retry=retry open=open/></li>
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </div>
    }
}

/// One cited row: its name, then the row in the results table's body, or
/// what stands in its place. A row that reads opens the evidence viewer
/// at its page.
#[component]
fn EvidenceItem(
    reference: EvidenceRef,
    evidence: RwSignal<HashMap<EvidenceRef, Evidence>>,
    retry: Callback<EvidenceRef>,
    open: Callback<Opened>,
) -> impl IntoView {
    let label = evidence_label(&reference);
    let title = reference.result_id.clone();
    let key = reference.clone();
    let state = Memo::new(move |_| {
        evidence.with(|map| map.get(&key).cloned().unwrap_or(Evidence::Loading))
    });
    let row_label = label.clone();
    view! {
        <p class="inv-ref" title=title>{label.clone()}</p>
        {move || match state.get() {
            Evidence::Loading => view! { <p class="inv-meta">"Loading row…"</p> }.into_any(),
            Evidence::Row(row) => {
                let opened = Opened { result_id: reference.result_id.clone(), row: Some(reference.row) };
                view! {
                    <EvidenceTable row=row label=row_label.clone()/>
                    <Btn variant=Variant::Secondary size=Size::Xs attr:class="inv-open-rows"
                        attr:aria-label=format!("Open {row_label} among its rows")
                        on_click=Callback::new(move |()| open.run(opened.clone()))>"Open result"</Btn>
                }.into_any()
            }
            Evidence::Pruned => view! {
                <p class="inv-pruned">
                    <Badge tone=fleet_ui::Tone::Neutral>"Pruned"</Badge>
                    " This result's evidence file is gone. The finding still cites the row."
                </p>
            }.into_any(),
            Evidence::NoRow => view! {
                <p class="inv-meta">{format!("Result {} holds no row {}.", short_result_id(&reference.result_id), reference.row)}</p>
            }.into_any(),
            Evidence::Failed(message) => {
                let again = reference.clone();
                view! {
                    <p class="inv-meta" role="status">"Could not load this row. " {message}</p>
                    <Btn variant=Variant::Secondary size=Size::Xs attr:aria-label=format!("Retry {row_label}")
                        on_click=Callback::new(move |()| retry.run(again.clone()))>"Retry"</Btn>
                }.into_any()
            }
        }}
    }
}

/// One captured row in the results table's markup, scrolling sideways
/// inside its own named region.
#[component]
fn EvidenceTable(row: EvidenceRow, label: String) -> impl IntoView {
    view! {
        <div class="results-table-wrap inv-row-scroll" role="region" aria-label=label tabindex="0">
            <table class="results-table">
                <thead><tr>
                    {row.columns.into_iter().map(|name| view! { <th scope="col">{name}</th> }).collect_view()}
                </tr></thead>
                <tbody><tr>
                    {row.cells.into_iter().map(|cell| view! { <td>{cell}</td> }).collect_view()}
                </tr></tbody>
            </table>
        </div>
    }
}

/// The bounds a step ran inside, as sent.
fn bounds_text(bounds: &StepBounds) -> String {
    format!("{} → {}", bounds.earliest, bounds.latest)
}

/// One entry in the activity: its kind, its outcome or status, what it
/// says, and who wrote it when.
#[component]
fn ActivityLine(seq: u64, item: Memo<Option<Activity>>) -> impl IntoView {
    view! {
        <li class="inv-act" data-anchor=format!("activity-{seq}")>
            {move || item.get().map(|item| view! {
                <div class="inv-entry-hd">
                    <span class="inv-kind">{item.kind}</span>
                    {item.badge.map(|(label, tone)| view! { <Badge tone=tone>{label}</Badge> })}
                </div>
                <p class="inv-act-text">{item.text}</p>
                <p class="inv-meta">{item.actor.name} " · " <When ts=item.at mode=WhenMode::Absolute/></p>
            })}
        </li>
    }
}

/// One step: its outcome and purpose, and behind them the text it
/// executed, its bounds, and who ran it when. The disclosure is made once
/// per step, so the step finishing leaves it open or closed as the
/// person left it.
#[component]
fn StepItem(seq: u64, entry: Memo<Option<LogEntry>>, open: Callback<Opened>) -> impl IntoView {
    let step = move || {
        entry.get().and_then(|entry| match entry.payload {
            EntryPayload::Step {
                executed,
                bounds,
                purpose,
                outcome,
                ..
            } => Some((entry.actor, entry.at, executed, bounds, purpose, outcome)),
            _ => None,
        })
    };
    view! {
        <li class="inv-step" data-anchor=format!("step-{seq}")>
            <details>
                <summary>
                    {move || step().map(|(.., purpose, outcome)| {
                        let facts = outcome_facts(outcome.as_ref());
                        view! {
                            <span class="inv-kind">"Step"</span>
                            " "
                            <Badge tone=outcome_tone(outcome.as_ref())>{outcome_label(outcome.as_ref())}</Badge>
                            {(!facts.is_empty()).then(|| view! { " " <span class="inv-meta">{facts}</span> })}
                            " "
                            <span class="inv-purpose">{purpose.unwrap_or_else(|| NO_PURPOSE.to_owned())}</span>
                        }
                    })}
                </summary>
                {move || step().map(|(actor, at, executed, bounds, _, outcome)| {
                    // A step that captured a result opens it in the viewer.
                    let result = outcome
                        .and_then(|o| o.result_id)
                        .filter(|id| is_result_id(id));
                    view! {
                        <div class="inv-step-body">
                            <pre class="inv-executed"><code>{executed}</code></pre>
                            {bounds.map(|b| view! {
                                <p class="inv-bounds"><span class="inv-label">"Bounds "</span><span class="mono">{bounds_text(&b)}</span></p>
                            })}
                            <p class="inv-meta">{actor.name} " · " <When ts=at mode=WhenMode::Absolute/></p>
                            {result.map(|result_id| {
                                let label = format!("View the rows of result {}", short_result_id(&result_id));
                                let opened = Opened { result_id, row: None };
                                view! {
                                    <Btn variant=Variant::Secondary size=Size::Xs attr:class="inv-open-rows" attr:aria-label=label
                                        on_click=Callback::new(move |()| open.run(opened.clone()))>"View rows"</Btn>
                                }
                            })}
                        </div>
                    }
                })}
            </details>
        </li>
    }
}
