// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<InvestigationsPage/>` and `<InvestigationPage/>` — the investigation
//! list and one investigation (ADR-0059).

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
use crate::auth_return::leaves_for_sign_in;
use crate::components::evidence_drawer::{EvidenceDrawer, KeepGate, Opened};
use crate::evidence_queue::{EvidenceQueue, Next, Paced, wait_ms};
use crate::investigation_live::{
    Connection, Liveness, Placed, ReadEnd, Reread, WriteFailure, announcement, new_entries_text,
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

const NOT_PERMITTED: &str = "You do not have permission to read investigations.";

const WAITING: &str = "The server asked the board to wait. It reads again when the wait ends.";

/// The shell mounts pages only once `/me` has answered.
fn permitted() -> Signal<bool> {
    let me = expect_context::<RwSignal<Option<api::MeResponse>>>();
    Signal::derive(move || {
        me.get()
            .is_some_and(|m| perms::can_read_investigations(&m.permissions))
    })
}

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

fn forbidden(error: &ApiError) -> bool {
    error.http_status() == Some(403)
}

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
    let refused = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let generation = RwSignal::new(0u64);

    let load = Callback::new(move |()| {
        let mine = generation.get_untracked().wrapping_add(1);
        generation.set(mine);
        busy.set(true);
        spawn_local(async move {
            let result = api::list_investigations().await;
            if generation.try_get_untracked() != Some(mine) || leaves_for_sign_in(&result) {
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Evidence {
    Loading,
    Row(EvidenceRow),
    Pruned,
    NoRow,
    Failed(String),
}

/// [`Self::pump`] is the one place a read starts, and it starts none while
/// the board's tab is hidden: the rows wait in the queue, and the queue
/// pumps again when the board's visibility listener says the tab shows.
#[derive(Clone, Copy)]
struct EvidenceReads {
    queue: StoredValue<EvidenceQueue<EvidenceRef>>,
    abort: StoredValue<Option<web_sys::AbortController>, LocalStorage>,
    evidence: RwSignal<HashMap<EvidenceRef, Evidence>>,
    stalled: RwSignal<Option<usize>>,
    page: NodeRef<html::Div>,
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
        Effect::new(move |_| {
            if visible.get() {
                untrack(|| reads.pump());
            }
        });
        reads
    }

    fn want(self, reference: EvidenceRef) {
        let _ = self.evidence.try_update(|map| {
            map.insert(reference.clone(), Evidence::Loading);
        });
        if self.queue.try_update_value(|q| q.push(reference)) == Some(true) {
            self.pump();
        }
    }

    fn resume(self) {
        self.queue
            .try_update_value(|q| q.resume(js_sys::Date::now()));
        let _ = self.stalled.try_set(None);
        self.pump();
    }

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
            if leaves_for_sign_in(&result) {
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum FirstRead {
    Pending,
    Missing,
    Forbidden,
    Failed(String),
}

const THROTTLED: &str = "The server is limiting reads for your API key.";

/// Whether the board's tab is visible. The one place the board asks:
/// it reads nothing while this is false ([`BoardReads::defer`]).
fn tab_visible() -> bool {
    !document().hidden()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// A person's read, `pages` reads into its run: none for mount,
    /// Refresh, Load more and Retry, more for the page that follows a
    /// short read.
    Person {
        pages: u32,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reader {
    /// Mount, Refresh, Load more or Retry: the page shows that it reads,
    /// and follows a short read at once.
    Person,
    /// The board's own read while its tab is visible: one read, quiet.
    Poll,
}

/// Every read a person or a write asks for passes one gate, [`Self::defer`],
/// which holds it while the tab is hidden or a 429's pause runs. The gate is
/// one place rather than a check in each caller, so a caller added later
/// cannot skip it. A deferred read claims no generation until it starts, so
/// it still counts as later than a lost write's mark ([`Self::mark`]).
///
/// The board's listener is the page's only one. It keeps `visible`, which
/// the board lends to its evidence reads ([`EvidenceReads`],
/// [`EvidenceDrawer`]).
#[derive(Clone, Copy)]
struct BoardReads {
    id: StoredValue<InvestigationId>,
    page: NodeRef<html::Div>,
    board: RwSignal<Option<Board>>,
    first: RwSignal<FirstRead>,
    stale: RwSignal<Option<String>>,
    busy: RwSignal<bool>,
    /// Reads merged since the person last asked for one.
    pages: RwSignal<u32>,
    live: RwSignal<Liveness>,
    visible: RwSignal<bool>,
    said: RwSignal<String>,
    generation: StoredValue<u64>,
    accepted: RwSignal<u64>,
    in_flight: StoredValue<Option<Reader>>,
    /// The read asked for while the tab was hidden or a 429's pause ran,
    /// which starts when the tab shows and the pause has ended. The page
    /// says a read waits while this is set in a visible tab.
    owed: RwSignal<Option<Ask>>,
    /// A read was answered 401: the page is on its way to sign-in.
    signed_out: StoredValue<bool>,
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
            owed: RwSignal::new(None),
            signed_out: StoredValue::new(false),
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
    /// gone, or on its way to sign-in.
    fn begin(self, reader: Reader) -> Option<(u64, Option<web_sys::AbortSignal>)> {
        if self.signed_out.try_get_value() != Some(false) {
            return None;
        }
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

    fn current(self, mine: u64) -> bool {
        self.generation.try_get_value() == Some(mine)
    }

    fn since(self) -> u64 {
        self.board
            .with_untracked(|b| b.as_ref().map_or(0, |b| b.through_seq))
    }

    fn adopt(self, investigation: Investigation) {
        let place = Place::of(self.page);
        let _ = self
            .board
            .try_maybe_update(|board| (board.as_mut().is_some_and(|b| b.adopt(investigation)), ()));
        if let Some(place) = place {
            place.keep();
        }
    }

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

    /// A read that starts reading at least as far as the one owed does its
    /// work, so that one is owed no more.
    fn read(self, ask: Ask) {
        if self.live.try_with_untracked(Liveness::stopped) != Some(false) {
            return;
        }
        if ask == Ask::Write && !self.readable() {
            return;
        }
        if self.defer(ask) {
            return;
        }
        let _ = self.owed.try_maybe_update(|owed| {
            let done = owed.is_some_and(|o| o.and(ask) == ask);
            if done {
                *owed = None;
            }
            (done, ())
        });
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

    /// The gate every read but the poll passes; the poll checks the 429
    /// pause itself ([`Self::poll`]).
    fn defer(self, ask: Ask) -> bool {
        let visible = tab_visible();
        let held = self.held();
        if visible && !held {
            return false;
        }
        let _ = self
            .owed
            .try_update(|owed| *owed = Some(owed.map_or(ask, |o| o.and(ask))));
        if visible {
            self.schedule();
        } else {
            let _ = self.live.try_update(Liveness::hidden);
        }
        true
    }

    fn held(self) -> bool {
        let now = js_sys::Date::now();
        self.live
            .try_with_untracked(|live| live.hold_ms(now) > 0)
            .unwrap_or(false)
    }

    fn readable(self) -> bool {
        self.board.try_with_untracked(Option::is_some) == Some(true)
            && self.live.try_with_untracked(Liveness::stopped) == Some(false)
    }

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
                if leaves_for_sign_in(&result) {
                    self.unauthorized();
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

    fn poll(self) {
        // Gone with the page, or another read runs.
        if self.in_flight.try_get_value() != Some(None) {
            return;
        }
        if !self.readable() || !tab_visible() {
            return;
        }
        if self.held() {
            self.schedule();
            return;
        }
        self.quiet();
    }

    /// The read after a write of the person's: at once, whatever read was
    /// running, since that one started before the write, or as soon as a
    /// hidden tab shows and any 429's pause has ended.
    ///
    /// A write's answer can arrive after the person left the page, so
    /// this reads nothing of a board that is gone: every access on the
    /// way is the disposal-safe one.
    fn after_write(self) {
        self.read(Ask::Write);
    }

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
                    if leaves_for_sign_in(&result) {
                        self.unauthorized();
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

    fn say_failure(self, message: String) {
        if self.board.with_untracked(Option::is_some) {
            self.stale.set(Some(message));
        } else {
            self.first.set(FirstRead::Failed(message));
        }
    }

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

    fn schedule(self) {
        let _ = self.timer.try_set_value(None);
        let Some(stopped) = self.live.try_with_untracked(Liveness::stopped) else {
            return;
        };
        if stopped {
            self.drop_owed();
            return;
        }
        let owes = self.owed.try_with_untracked(Option::is_some) == Some(true);
        if !owes && self.board.try_with_untracked(Option::is_none) != Some(false) {
            return;
        }
        if !tab_visible() {
            let _ = self.live.try_update(Liveness::hidden);
            return;
        }
        let now = js_sys::Date::now();
        let delay = self
            .live
            .try_with_untracked(|live| {
                if owes {
                    Some(live.hold_ms(now))
                } else {
                    live.next_delay(now)
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
                self.due();
            })));
    }

    fn due(self) {
        match self.owed.try_update(Option::take).flatten() {
            Some(ask) => self.read(ask),
            None => self.poll(),
        }
    }

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
        match wait {
            None => self.drop_owed(),
            Some(0) => self.due(),
            Some(ms) => self.arm(ms),
        }
    }

    fn unauthorized(self) {
        let _ = self.timer.try_set_value(None);
        self.drop_owed();
        let _ = self.signed_out.try_set_value(true);
    }

    fn drop_owed(self) {
        let _ = self
            .owed
            .try_maybe_update(|owed| (owed.take().is_some(), ()));
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
    let viewing = RwSignal::new(None::<Opened>);
    let page_size = RwSignal::new(None::<NonZeroUsize>);
    let open = Callback::new(move |opened: Opened| viewing.set(Some(opened)));
    let close = Callback::new(move |()| viewing.set(None));

    let me = expect_context::<RwSignal<Option<api::MeResponse>>>();
    let writer = Signal::derive(move || {
        me.with(|m| {
            m.as_ref()
                .is_some_and(|m| perms::can_write_investigations(&m.permissions))
        }) && !live.with(Liveness::stopped)
    });
    let accepted: Signal<u64> = reads.accepted.into();
    let can_read = Memo::new(move |_| !live.with(Liveness::stopped));
    let owed = reads.owed;
    let waiting = Signal::derive(move || visible.get() && owed.with(Option::is_some));
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
        reading: can_read.into(),
    };
    read.run(());

    let rows = EvidenceReads::new(evidence, page, visible);
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
    // reads its own current copy, so an open step stays open and a focused
    // row keeps its focus.
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
            FirstRead::Pending => view! {
                <div class="page inv-page">
                    <Show when=move || connection.get() == Connection::Paused
                        fallback=move || view! {
                            <Show when=move || waiting.get()
                                fallback=|| view! { <p role="status">"Loading investigation…"</p> }>
                                <p class="inv-wait" role="status">{WAITING}</p>
                            </Show>
                        }>
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
                        " Opened by " {field(|i| i.created_by.name.clone())}
                        " · updated " <When ts=Signal::derive(field(|i| i.updated_at.clone())) mode=WhenMode::Absolute/>
                        " by " {field(|i| i.updated_by.name.clone())}
                        <Show when=move || writer.get()>
                            <StatusControl reads=reads note=status_note/>
                        </Show>
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
                    <StatusNoteView note=status_note accepted=accepted connection=Signal::from(connection) refresh=refresh/>
                </div>
                <div class="actions">
                    <Show when=move || can_read.get()>
                        <Btn variant=Variant::Secondary size=Size::Sm on_click=refresh>"Refresh"</Btn>
                    </Show>
                </div>
            </div>
            <Show when=move || waiting.get()>
                <p class="inv-wait" role="status">{WAITING}</p>
            </Show>
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
                // A stopped board offers no Load more, and claims nothing
                // about what it did not read.
                <Show when=move || can_read.get() || !more.get()>
                    <LoadMore has_more=more busy=busy on_load=read
                        empty=Signal::derive(move || activity_seqs.with(Vec::is_empty))
                        label="Load more entries" end_text="Every entry is loaded." empty_text="Nothing recorded yet."/>
                </Show>
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
#[derive(Debug, Clone, PartialEq)]
enum StatusNote {
    Sending(InvestigationStatus),
    Done(InvestigationStatus),
    /// Refused as stale or closed. The mark is the board's newest read
    /// generation when the refusal arrived ([`BoardReads::mark`]): only a
    /// later read that lands has read the investigation again ([`Reread`]).
    Conflict(InvestigationStatus, WriteFailure, u64),
    /// No answer arrived. The mark is the board's newest read
    /// generation when it failed ([`BoardReads::mark`]): the control
    /// comes back once a later read lands.
    Unknown(InvestigationStatus, u64),
    Refused(InvestigationStatus, String),
}

fn status_verb(target: InvestigationStatus) -> &'static str {
    match target {
        InvestigationStatus::Closed => "close",
        InvestigationStatus::Open => "reopen",
    }
}

impl StatusNote {
    fn text(&self, accepted: u64, connection: Connection) -> Option<String> {
        Some(match self {
            Self::Sending(_) => return None,
            Self::Done(InvestigationStatus::Closed) => "You closed the investigation.".to_owned(),
            Self::Done(InvestigationStatus::Open) => "You reopened the investigation.".to_owned(),
            Self::Conflict(target, failure, mark) => {
                let reread = Reread::of(*mark, accepted, connection);
                let verb = status_verb(*target);
                let what = if *failure == WriteFailure::Closed {
                    format!("The investigation was already closed when your {verb} reached it")
                } else {
                    format!("The investigation changed before your {verb} reached it")
                };
                // Acting again needs the control a stopped board drops.
                let again = if *failure == WriteFailure::Closed || reread == Reread::Stopped {
                    ""
                } else {
                    ": check its status and act again if you still mean to"
                };
                format!("{what}, so nothing changed. {}{again}.", reread.sentence())
            }
            Self::Unknown(target, _) => {
                // A stopped board has no Refresh to point at.
                let next = if connection == Connection::Stopped {
                    "The board stopped reading, so reload the page to see the investigation's status."
                } else {
                    "Refresh to see the investigation's status before you act again."
                };
                format!(
                    "The answer to your {} was lost, so it may or may not have taken effect. {next}",
                    status_verb(*target)
                )
            }
            Self::Refused(target, message) => {
                format!(
                    "Could not {} the investigation. {message}",
                    status_verb(*target)
                )
            }
        })
    }
}

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
                    // Taken before the read after it claims a generation.
                    let mark = reads.mark();
                    reads.after_write();
                    StatusNote::Conflict(target, failure, mark)
                }
                Err(WriteFailure::Unknown) => StatusNote::Unknown(target, reads.mark()),
                Err(WriteFailure::Refused(message)) => StatusNote::Refused(target, message),
            };
            let _ = note.try_set(Some(outcome));
        });
    });
    view! {
        {move || status.get().map(|current| {
            let text = match current {
                InvestigationStatus::Open => "Close investigation",
                InvestigationStatus::Closed => "Reopen investigation",
            };
            view! {
                <span class="inv-status-act">
                    <Btn variant=Variant::Secondary size=Size::Xs
                        disabled=held on_click=act>{text}</Btn>
                </span>
            }
        })}
    }
}

#[component]
fn StatusNoteView(
    note: RwSignal<Option<StatusNote>>,
    accepted: Signal<u64>,
    connection: Signal<Connection>,
    refresh: Callback<()>,
) -> impl IntoView {
    let text = Memo::new(move |_| {
        note.with(|n| {
            n.as_ref()
                .and_then(|n| n.text(accepted.get(), connection.get()))
        })
    });
    let warn = move || {
        note.with(|n| {
            matches!(
                n,
                Some(StatusNote::Conflict(..) | StatusNote::Refused(..) | StatusNote::Unknown(..))
            )
        })
    };
    let unknown = move || {
        note.with(|n| match n {
            Some(StatusNote::Unknown(_, mark)) => accepted.get() <= *mark,
            _ => false,
        }) && connection.get() != Connection::Stopped
    };
    view! {
        <Show when=move || text.with(Option::is_some)>
            <div class="inv-write-note" class:inv-write-warn=warn>
                <p role="status">{move || text.get().unwrap_or_default()}</p>
                <Show when=unknown>
                    <Btn variant=Variant::Secondary size=Size::Xs on_click=refresh>"Refresh the board"</Btn>
                </Show>
            </div>
        </Show>
    }
}

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
                    <Btn variant=Variant::Secondary size=Size::Xs class:inv-open-rows=true
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

fn bounds_text(bounds: &StepBounds) -> String {
    format!("{} → {}", bounds.earliest, bounds.latest)
}

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

/// The disclosure is made once per step, so the step finishing leaves it
/// open or closed as the person left it.
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
                                    <Btn variant=Variant::Secondary size=Size::Xs class:inv-open-rows=true attr:aria-label=label
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
