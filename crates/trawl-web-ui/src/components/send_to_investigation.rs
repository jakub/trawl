// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<SendToInvestigation/>` — send the search Search ran into an open
//! investigation (ADR-0057, ADR-0059).

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::investigations::{InvestigationId, PURPOSE_MAX_BYTES};

use crate::api;
use crate::components::read_turn::ReadTurn;
use crate::query_merge::{CaptureParts, RangeSpec};
use crate::send_investigation::{
    Choice, SendFailure, SendPlan, SendSnapshot, busy_copy, open_choices, plan, purpose,
};
use fleet_ui::{Btn, Field, Icon, Modal, ToastBus, ToastKind, ToastLink, Variant};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    Idle,
    Pinning(RangeSpec),
    Sending,
    Unknown(InvestigationId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Choices {
    Loading,
    Ready(Vec<Choice>),
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanLimit {
    Loading,
    /// `bounds.max_span_secs`.
    Known(u64),
    Unknown,
}

impl SpanLimit {
    const fn secs(self) -> Option<u64> {
        match self {
            Self::Known(secs) => Some(secs),
            Self::Loading | Self::Unknown => None,
        }
    }
}

fn board_href(id: &InvestigationId) -> String {
    format!("/investigations/{id}")
}

fn window_text(parts: &CaptureParts) -> String {
    parts.window.as_ref().map_or_else(
        || "From the time terms in the query text.".to_owned(),
        |window| {
            format!(
                "From {} up to, not including, {}",
                window.earliest, window.latest
            )
        },
    )
}

#[component]
#[allow(clippy::too_many_lines)]
pub fn SendToInvestigation(
    snapshot: SendSnapshot,
    #[prop(into)] accepted: Signal<Option<SendSnapshot>>,
    #[prop(into)] failed: Signal<bool>,
    /// Run Search again on a pinned range, through the shared codec. The
    /// error is the sentence to show when the link is refused.
    on_pin: Callback<RangeSpec, Result<(), String>>,
    on_close: Callback<()>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    // This latch belongs to this one dialog, outside the reactive arena:
    // a capture that answers after the dialog closed still toasts, but
    // never writes into a dialog that is gone.
    let alive = Arc::new(AtomicBool::new(true));
    let cleanup_alive = Arc::clone(&alive);
    on_cleanup(move || cleanup_alive.store(false, Ordering::Release));

    let snap = RwSignal::new(snapshot);
    let stage = RwSignal::new(Stage::Idle);
    let choices = RwSignal::new(Choices::Loading);
    let choice = RwSignal::new(None::<Choice>);
    let purpose_text = RwSignal::new(String::new());
    let notice = RwSignal::new(None::<String>);
    let span_limit = RwSignal::new(SpanLimit::Loading);
    let current_plan = Memo::new(move |_| {
        let limit = span_limit.get().secs();
        snap.with(|snap| plan(snap, limit))
    });
    {
        let alive = Arc::clone(&alive);
        spawn_local(async move {
            let read = api::capabilities().await;
            if alive.load(Ordering::Acquire) && !crate::auth_return::leaves_for_sign_in(&read) {
                let _ = span_limit.try_set(match read {
                    Ok(capabilities) => SpanLimit::Known(capabilities.bounds.max_span_secs),
                    Err(_) => SpanLimit::Unknown,
                });
            }
        });
    }

    let choices_turn = ReadTurn::new();
    let load_choices = {
        let alive = Arc::clone(&alive);
        move || {
            choices.set(Choices::Loading);
            let alive = Arc::clone(&alive);
            let turn = choices_turn.begin();
            spawn_local(async move {
                let listed = api::list_investigations().await;
                if !alive.load(Ordering::Acquire)
                    || !choices_turn.is_current(turn)
                    || crate::auth_return::leaves_for_sign_in(&listed)
                {
                    return;
                }
                let open = match listed {
                    Ok(list) => Choices::Ready(open_choices(&list)),
                    Err(_) => Choices::Failed,
                };
                if let Choices::Ready(open) = &open
                    && choice
                        .get_untracked()
                        .is_some_and(|picked| !open.iter().any(|c| c.id == picked.id))
                {
                    choice.set(None);
                }
                choices.try_set(open);
            });
        }
    };
    load_choices();

    let purpose_problem = Memo::new(move |_| purpose_text.with(|text| purpose(text).err()));
    let purpose_bytes = Memo::new(move |_| purpose_text.with(|text| text.trim().len()));

    let send_parts = {
        let alive = Arc::clone(&alive);
        let load_choices = load_choices.clone();
        move |parts: CaptureParts| {
            let Some(target) = choice.get_untracked() else {
                return;
            };
            let Ok(stated) =
                purpose_text.with_untracked(|text| purpose(text).map(|p| p.map(str::to_owned)))
            else {
                return;
            };
            stage.set(Stage::Sending);
            notice.set(None);
            let alive = Arc::clone(&alive);
            let load_choices = load_choices.clone();
            spawn_local(async move {
                let answer = api::capture_attached(&parts, &target.id, stated.as_deref()).await;
                let live = alive.load(Ordering::Acquire);
                match answer {
                    Ok(response) => {
                        let step = response
                            .step_seq
                            .map_or_else(String::new, |seq| format!("Step {seq} in "));
                        bus.push_with_link(
                            ToastKind::Success,
                            "Sent to investigation",
                            Some(format!("{step}{}", target.title)),
                            ToastLink::new(board_href(&target.id), "Open the board"),
                        );
                        if live {
                            on_close.run(());
                        }
                    }
                    Err(_) if !live => {}
                    Err(SendFailure::Closed) => {
                        stage.set(Stage::Idle);
                        notice.set(Some(format!(
                            "\u{201c}{}\u{201d} was closed before the search reached it. \
                             Nothing was sent. Choose an open investigation.",
                            target.title
                        )));
                        load_choices();
                    }
                    Err(SendFailure::Busy {
                        message,
                        retry_after_ms,
                    }) => {
                        stage.set(Stage::Idle);
                        notice.set(Some(busy_copy(&message, retry_after_ms)));
                    }
                    Err(SendFailure::Unauthorized) => {
                        stage.set(Stage::Idle);
                        notice.set(Some(
                            "Your session has ended. Sign in again, then send.".to_owned(),
                        ));
                    }
                    Err(SendFailure::Refused(message)) => {
                        stage.set(Stage::Idle);
                        notice.set(Some(format!("Nothing was sent: {message}")));
                    }
                    Err(SendFailure::Unknown) => {
                        stage.set(Stage::Unknown(target.id.clone()));
                        notice.set(Some(
                            "The answer was lost on the way back, so it is not known whether \
                             the search reached the investigation. Open its board to check \
                             before you send it again."
                                .to_owned(),
                        ));
                    }
                }
            });
        }
    };

    {
        let send_parts = send_parts.clone();
        Effect::new(move |_| {
            let Stage::Pinning(pinned) = stage.get() else {
                return;
            };
            if let Some(ran) = accepted.get() {
                let same = snap.with_untracked(|s| s.base == ran.base && s.filters == ran.filters);
                if same && ran.range == pinned {
                    snap.set(ran);
                    match current_plan.get_untracked() {
                        SendPlan::Ready(parts) => send_parts(parts),
                        // An absolute range with both ends is ready, so
                        // this is the codec refusing the pinned range.
                        SendPlan::Pin(_) | SendPlan::Refused(_) => {
                            stage.set(Stage::Idle);
                            notice.set(Some(
                                "The pinned range could not be sent. Nothing was sent.".to_owned(),
                            ));
                        }
                    }
                    return;
                }
            }
            if failed.get() {
                stage.set(Stage::Idle);
                notice.set(Some(
                    "The search did not run on the pinned range, so nothing was sent.".to_owned(),
                ));
            }
        });
    }

    let ready = Signal::derive(move || {
        choice.with(Option::is_some)
            && span_limit.get() != SpanLimit::Loading
            && purpose_problem.get().is_none()
            && stage.get() == Stage::Idle
            && !matches!(current_plan.get(), SendPlan::Refused(_))
    });
    let act = {
        let send_parts = send_parts.clone();
        Callback::new(move |()| {
            if !ready.get_untracked() {
                return;
            }
            match current_plan.get_untracked() {
                SendPlan::Ready(parts) => send_parts(parts),
                SendPlan::Pin(pinned) => match on_pin.run(pinned.clone()) {
                    Ok(()) => {
                        notice.set(None);
                        stage.set(Stage::Pinning(pinned));
                    }
                    Err(reason) => notice.set(Some(reason)),
                },
                SendPlan::Refused(_) => {}
            }
        })
    };
    let cancel = Callback::new(move |()| on_close.run(()));

    let primary_label = move || match (stage.get(), current_plan.get()) {
        (Stage::Sending, _) => "Sending…",
        (Stage::Pinning(_), _) => "Running the pinned search…",
        (_, SendPlan::Pin(_)) => "Pin and re-run",
        _ => "Send",
    };

    view! {
        <Modal
            title="Send to investigation"
            icon=Icon::Document
            on_cancel=cancel
            on_submit=act
            footer=Box::new(move || view! {
                <Btn variant=Variant::Secondary on_click=cancel>"Close"</Btn>
                {move || match stage.get() {
                    Stage::Unknown(id) => view! {
                        <a class="btn-pri" href=board_href(&id)>"Open the board"</a>
                    }.into_any(),
                    _ if matches!(current_plan.get(), SendPlan::Refused(_)) => ().into_any(),
                    _ => view! {
                        <Btn variant=Variant::Primary disabled=Signal::derive(move || !ready.get()) on_click=act>
                            {primary_label}
                        </Btn>
                    }.into_any(),
                }}
            }.into_any())
        >
            <fieldset class="m-field send-choices">
                <legend>"Investigation"</legend>
                {move || match choices.get() {
                    Choices::Loading => view! {
                        <p class="send-note" role="status">"Listing the open investigations…"</p>
                    }.into_any(),
                    Choices::Failed => {
                        let load_choices = load_choices.clone();
                        view! {
                            <p class="m-refusal" role="alert">"The open investigations could not be listed."</p>
                            <Btn variant=Variant::Secondary on_click=Callback::new(move |()| load_choices())>"List again"</Btn>
                        }.into_any()
                    }
                    Choices::Ready(open) if open.is_empty() => view! {
                        <p class="send-note" role="status">
                            "No investigation is open. An agent opens one through "
                            <code>"trawl mcp"</code>
                            ", with the "<code>"investigation"</code>" tool's "
                            <code>"create"</code>" action."
                        </p>
                    }.into_any(),
                    Choices::Ready(open) => open.into_iter().map(|option| {
                        let picked = option.id.clone();
                        let short = option.short_id().to_owned();
                        let title = option.title.clone();
                        view! {
                            <label class="send-choice">
                                <input
                                    type="radio"
                                    name="send-investigation"
                                    prop:checked=move || choice.with(|c| c.as_ref().is_some_and(|c| c.id == picked))
                                    disabled=move || stage.get() != Stage::Idle
                                    on:change=move |_| choice.set(Some(option.clone()))
                                />
                                <span class="send-title">{title}</span>
                                " "
                                <span class="mono send-id">{short}</span>
                            </label>
                        }
                    }).collect_view().into_any(),
                }}
            </fieldset>

            {move || match current_plan.get() {
                SendPlan::Ready(parts) => {
                    let window = window_text(&parts);
                    view! {
                        <div class="m-field">
                            <label>"Query"</label>
                            <div class="preview send-query" title=parts.query.clone()>{parts.query.clone()}</div>
                        </div>
                        <div class="m-field">
                            <label>"Window"</label>
                            <div class="preview send-window">{window}</div>
                        </div>
                    }.into_any()
                }
                SendPlan::Pin(pinned) => {
                    let (from, to) = match &pinned {
                        RangeSpec::Absolute { from, to } => (from.clone(), to.clone()),
                        RangeSpec::Quick(label) => ((*label).to_owned(), String::new()),
                    };
                    view! {
                        <p class="send-note send-moving">
                            "This search's range moves: it ends when the search runs. A capture is "
                            "fixed, so the range is pinned to when this search ran, "
                            <span class="mono">{from}</span>" to "<span class="mono">{to}</span>
                            ". Search runs again on that range, and then that search is sent."
                        </p>
                    }.into_any()
                }
                SendPlan::Refused(reason) => view! {
                    <p class="m-refusal" role="alert">{reason}</p>
                }.into_any(),
            }}

            <Field id="sendPurpose" label="Purpose (optional)" class="m-field">
                <textarea
                    id="sendPurpose"
                    class="reason-input"
                    prop:value=move || purpose_text.get()
                    on:input=move |e| purpose_text.set(event_target_value(&e))
                    disabled=move || stage.get() != Stage::Idle
                    aria-invalid=move || purpose_problem.get().is_some().to_string()
                    aria-describedby="sendPurposeHint"
                ></textarea>
                <span id="sendPurposeHint" class="send-note" role="status">
                    {move || purpose_problem.get().map_or_else(
                        || format!("{} of {PURPOSE_MAX_BYTES} bytes.", purpose_bytes.get()),
                        crate::send_investigation::PurposeProblem::copy,
                    )}
                </span>
            </Field>

            {move || notice.get().map(|text| view! { <p class="m-refusal" role="alert">{text}</p> })}
        </Modal>
    }
}
