// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<InvestigationsPage/>` and `<InvestigationPage/>` — the read-only
//! investigation list and one investigation (ADR-0059).
//!
//! Both pages are for a session holding `investigation_read` and `query`
//! ([`perms::can_read_investigations`]). Without them the page says so
//! and asks the server nothing, and the sidebar has no entry for it. A
//! first read the server refuses with 403 (a role revoked after `/me`
//! answered) shows the same page, with nothing to retry; the sidebar
//! stays as `/me` said until it answers otherwise.
//! Nothing here writes: Keep, Send to investigation and close or reopen
//! need `investigation_write` and are not part of these pages, so no
//! control on them stands in for one.
//!
//! The list is ADR-0029 rows, one stretched link each. The investigation
//! reads top to bottom: the question and scope, the latest finding,
//! every finding with the rows it cites, then the log of steps and notes
//! in the order they were written ([`crate::investigation_log`]). The
//! page reads on mount and on Refresh or Load more, never on a timer.
//! Each read asks for the entries changed since the last one, and a
//! response for a read that a later one superseded, or that lands after
//! the page is gone, is dropped.
//!
//! A cited row is read through the captured-result page route, once per
//! row per page visit, paced so a board that cites many rows stays
//! inside the reader's rate limit ([`crate::evidence_queue`]). A row
//! whose evidence file was pruned answers 410 and shows as pruned in its
//! place: the finding still cites it. A row whose read failed keeps a
//! Retry, and Refresh asks again for every failed row.

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use trawl_api::investigations::{
    EntryPayload, EvidenceRef, InvestigationId, InvestigationList, InvestigationStatus, LogEntry,
    StepBounds,
};

use crate::api::{self, ApiError};
use crate::evidence_queue::{EvidenceQueue, Next, Paced, wait_ms};
use crate::investigation_log::{
    Board, ENTRY_PAGE, EvidenceRow, Finding, evidence_label, evidence_row, finding_status_label,
    finding_status_tone, investigation_status_label, investigation_status_tone, outcome_facts,
    outcome_label, outcome_tone, short_result_id,
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
/// and asks for the row again instead of failing it. A row shows
/// [`Evidence::Loading`] from the moment it is asked for until it is
/// answered. Leaving the page cancels the queue and aborts the reads in
/// flight; an answer that still lands finds the page's state gone and is
/// dropped.
#[derive(Clone, Copy)]
struct EvidenceReads {
    queue: StoredValue<EvidenceQueue<EvidenceRef>>,
    abort: StoredValue<Option<web_sys::AbortController>, LocalStorage>,
    evidence: RwSignal<HashMap<EvidenceRef, Evidence>>,
}

impl EvidenceReads {
    fn new(evidence: RwSignal<HashMap<EvidenceRef, Evidence>>) -> Self {
        let reads = Self {
            queue: StoredValue::new(EvidenceQueue::new()),
            abort: StoredValue::new_local(web_sys::AbortController::new().ok()),
            evidence,
        };
        on_cleanup(move || {
            reads.queue.try_update_value(EvidenceQueue::cancel);
            reads.abort.try_with_value(|abort| {
                if let Some(abort) = abort {
                    abort.abort();
                }
            });
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

    /// Start every read the queue allows now, and arm its timer when a
    /// 429 paused it.
    fn pump(self) {
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
            let paced =
                api::result_page(&reference.result_id, reference.row, 1, signal.as_ref()).await;
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
                Ok(page) => {
                    evidence_row(&page, reference.row).map_or(Evidence::NoRow, Evidence::Row)
                }
                Err(e) if e.http_status() == Some(410) => Evidence::Pruned,
                Err(e) => Evidence::Failed(e.to_string()),
            };
            let _ = self.evidence.try_update(|map| map.insert(reference, state));
            self.pump();
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

#[component]
#[allow(clippy::too_many_lines)] // one page's view tree
fn InvestigationDetail(id: InvestigationId) -> impl IntoView {
    let board = RwSignal::new(None::<Board>);
    let first = RwSignal::new(FirstRead::Pending);
    let stale = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let generation = RwSignal::new(0u64);
    let evidence = RwSignal::new(HashMap::<EvidenceRef, Evidence>::new());

    let read = {
        let id = id.clone();
        Callback::new(move |()| {
            let since = board.with_untracked(|b| b.as_ref().map_or(0, |b| b.through_seq));
            let mine = generation.get_untracked().wrapping_add(1);
            generation.set(mine);
            busy.set(true);
            let id = id.clone();
            spawn_local(async move {
                let result = api::read_investigation(&id, since, ENTRY_PAGE).await;
                if generation.try_get_untracked() != Some(mine) || sign_in_if_unauthorized(&result)
                {
                    return;
                }
                match result {
                    Ok(read) => {
                        board.update(|b| match b {
                            Some(b) => b.merge(read),
                            None => *b = Some(Board::new(read)),
                        });
                        stale.set(None);
                    }
                    Err(e) if board.with_untracked(Option::is_some) => {
                        stale.set(Some(read_failure(&e)));
                    }
                    Err(e) if e.http_status() == Some(404) => first.set(FirstRead::Missing),
                    Err(e) if forbidden(&e) => first.set(FirstRead::Forbidden),
                    Err(e) => first.set(FirstRead::Failed(read_failure(&e))),
                }
                busy.set(false);
            });
        })
    };
    read.run(());

    let reads = EvidenceReads::new(evidence);
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
            reads.want(reference);
        }
    });
    let retry_row = Callback::new(move |reference| reads.want(reference));
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
            reads.want(reference);
        }
        read.run(());
    });

    let findings =
        Signal::derive(move || board.with(|b| b.as_ref().map(Board::findings).unwrap_or_default()));
    let latest = Signal::derive(move || board.with(|b| b.as_ref().and_then(Board::latest_finding)));
    let log =
        Signal::derive(move || board.with(|b| b.as_ref().map(Board::log).unwrap_or_default()));
    let more = Signal::derive(move || board.with(|b| b.as_ref().is_some_and(|b| b.more)));
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

    view! {
        <Show when=move || board.with(Option::is_some) fallback=move || match first.get() {
            FirstRead::Missing => view! { <Missing/> }.into_any(),
            FirstRead::Forbidden => view! { <NotPermitted/> }.into_any(),
            FirstRead::Pending => view! {
                <div class="page inv-page"><p role="status">"Loading investigation…"</p></div>
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
        <div class="page inv-page" aria-busy=move || busy.get().to_string()>
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
                    </p>
                </div>
                <div class="actions">
                    <Btn variant=Variant::Secondary size=Size::Sm on_click=refresh>"Refresh"</Btn>
                </div>
            </div>
            {move || stale.get().map(|message| view! {
                <p class="inv-stale" role="status">"Refresh failed. Showing what was read before. " {message}</p>
            })}

            <section class="inv-sec" aria-labelledby="inv-question-h">
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

            <section class="inv-sec" aria-labelledby="inv-latest-h">
                <h2 id="inv-latest-h" class="inv-sec-h">"Latest finding"</h2>
                {move || match latest.get() {
                    Some(finding) => view! { <FindingHead finding=finding/> }.into_any(),
                    None => view! { <p class="inv-empty">"No findings yet."</p> }.into_any(),
                }}
            </section>

            <section class="inv-sec" aria-labelledby="inv-findings-h">
                <h2 id="inv-findings-h" class="inv-sec-h">"Findings"</h2>
                <Show when=move || !findings.get().is_empty()
                    fallback=|| view! { <p class="inv-empty">"No findings yet."</p> }>
                    <ol class="inv-findings" aria-label="Findings">
                        <For each=move || findings.get()
                            key=|f| (f.seq, f.status, f.status_by.as_ref().map(|a| a.prefix.clone()))
                            children=move |finding| view! {
                                <li><FindingCard finding=finding evidence=evidence retry=retry_row/></li>
                            }/>
                    </ol>
                </Show>
            </section>

            <section class="inv-sec" aria-labelledby="inv-log-h">
                <h2 id="inv-log-h" class="inv-sec-h">"Steps and notes"</h2>
                <ol class="inv-log" aria-label="Steps and notes">
                    <For each=move || log.get() key=|e| (e.seq, e.updated_seq) children=|entry| view! {
                        <LogItem entry=entry/>
                    }/>
                </ol>
                <LoadMore has_more=more busy=busy on_load=read
                    empty=Signal::derive(move || log.get().is_empty() && findings.get().is_empty())
                    label="Load more entries" end_text="Every entry is loaded." empty_text="Nothing recorded yet."/>
            </section>
        </div>
        </Show>
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

/// A finding with every row it cites.
#[component]
fn FindingCard(
    finding: Finding,
    evidence: RwSignal<HashMap<EvidenceRef, Evidence>>,
    retry: Callback<EvidenceRef>,
) -> impl IntoView {
    let references = finding.references.clone();
    let counter = finding.counterevidence.clone();
    view! {
        <article class="inv-finding">
            <FindingHead finding=finding/>
            <EvidenceList title="Evidence" refs=references evidence=evidence retry=retry/>
            {(!counter.is_empty()).then(|| view! {
                <EvidenceList title="Counterevidence" refs=counter evidence=evidence retry=retry/>
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
                            <li><EvidenceItem reference=reference evidence=evidence retry=retry/></li>
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </div>
    }
}

/// One cited row: its name, then the row in the results table's body, or
/// what stands in its place.
#[component]
fn EvidenceItem(
    reference: EvidenceRef,
    evidence: RwSignal<HashMap<EvidenceRef, Evidence>>,
    retry: Callback<EvidenceRef>,
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
            Evidence::Row(row) => view! { <EvidenceTable row=row label=row_label.clone()/> }.into_any(),
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

/// One log entry.
#[component]
fn LogItem(entry: LogEntry) -> impl IntoView {
    let meta = view! {
        <p class="inv-meta">{entry.actor.name.clone()} " · " <When ts=entry.at.clone() mode=WhenMode::Absolute/></p>
    };
    let body = match entry.payload {
        EntryPayload::Step {
            executed,
            bounds,
            purpose,
            outcome,
            ..
        } => {
            let facts = outcome_facts(outcome.as_ref());
            view! {
                <div class="inv-entry-hd">
                    <span class="inv-kind">"Step"</span>
                    <Badge tone=outcome_tone(outcome.as_ref())>{outcome_label(outcome.as_ref())}</Badge>
                    {(!facts.is_empty()).then(|| view! { <span class="inv-meta">{facts}</span> })}
                </div>
                <p class="inv-purpose">{purpose.unwrap_or_else(|| "No purpose given.".to_owned())}</p>
                <pre class="inv-executed"><code>{executed}</code></pre>
                {bounds.map(|b| view! {
                    <p class="inv-bounds"><span class="inv-label">"Bounds "</span><span class="mono">{bounds_text(&b)}</span></p>
                })}
            }
            .into_any()
        }
        EntryPayload::Note { text } => view! {
            <div class="inv-entry-hd"><span class="inv-kind">"Note"</span></div>
            <p class="inv-note">{text}</p>
        }
        .into_any(),
        EntryPayload::Keep {
            result_id,
            rows,
            reason,
        } => {
            let rows = rows
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            view! {
                <div class="inv-entry-hd"><span class="inv-kind">"Keep"</span></div>
                <p class="inv-note" title=result_id.clone()>
                    {format!("Kept rows {rows} of result {}", short_result_id(&result_id))}
                </p>
                <p class="inv-note">{reason}</p>
            }
            .into_any()
        }
        // The log leaves findings to their own section; one passed here
        // still reads as what it is.
        EntryPayload::Finding { statement, .. } => view! {
            <div class="inv-entry-hd"><span class="inv-kind">"Finding"</span></div>
            <p class="inv-note">{statement}</p>
        }
        .into_any(),
        EntryPayload::FindingStatus { status, .. } => view! {
            <div class="inv-entry-hd"><span class="inv-kind">"Finding status"</span></div>
            <p class="inv-note">{format!("Set a finding to {}.", finding_status_label(status))}</p>
        }
        .into_any(),
        EntryPayload::Status { status } => view! {
            <div class="inv-entry-hd"><span class="inv-kind">"Status"</span></div>
            <p class="inv-note">{match status {
                InvestigationStatus::Open => "Reopened the investigation.",
                InvestigationStatus::Closed => "Closed the investigation.",
            }}</p>
        }
        .into_any(),
        EntryPayload::Title { title } => view! {
            <div class="inv-entry-hd"><span class="inv-kind">"Title"</span></div>
            <p class="inv-note">{title}</p>
        }
        .into_any(),
        EntryPayload::Question { question } => view! {
            <div class="inv-entry-hd"><span class="inv-kind">"Question"</span></div>
            <p class="inv-note">{question}</p>
        }
        .into_any(),
    };
    view! {
        <li class="inv-entry">
            {body}
            {meta}
        </li>
    }
}
