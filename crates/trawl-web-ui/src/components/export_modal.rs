// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ExportModal/>` — download query results as CSV, JSON, or Parquet.
//!
//! Built on `fleet_ui::Modal`: the shell owns the scrim, Escape,
//! Cmd/Ctrl+Enter submit, and the header (Download icon chip + close);
//! this component owns the format picker, the download flow, and the
//! footer buttons.

use leptos::prelude::*;
use leptos::task::spawn_local;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use trawl_api::ExportFormat;

use crate::api;
use crate::download;
use crate::search_url::{EMPTY_QUERY_REFUSAL, is_executable};
use fleet_ui::{Btn, Icon, Modal, Segmented, SegmentedOption, ToastBus, ToastKind, Variant};

#[component]
#[allow(clippy::needless_pass_by_value)]
pub fn ExportModal(
    /// The DSL query to export. Shown read-only in the preview strip.
    query: String,
    /// Called on close — `true` if a download completed, `false` on cancel.
    on_close: Callback<bool>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let format = RwSignal::new(ExportFormat::Csv);
    let downloading = RwSignal::new(false);
    // This latch belongs to this one dialog instance, outside the reactive
    // arena. A late task may still report its outcome after dismissal, but
    // must never invoke the parent's callback for a replacement dialog.
    let alive = Arc::new(AtomicBool::new(true));
    let cleanup_alive = Arc::clone(&alive);
    on_cleanup(move || cleanup_alive.store(false, Ordering::Release));
    // What this modal refused, shown in its own body rather than as a
    // toast: the reader is looking at the dialog they just submitted.
    let refusal = RwSignal::new(None::<&'static str>);

    let q_for_submit = query.clone();
    let do_download = Callback::new(move |()| {
        if downloading.get_untracked() {
            return;
        }
        // An empty query is the whole corpus to the server's emitter, so
        // the download button is not a way around the malformed gate
        // that blanked it (ADR-0027). `api::export` refuses it as well;
        // this arm is what the reader sees.
        if !is_executable(&q_for_submit) {
            refusal.set(Some(EMPTY_QUERY_REFUSAL));
            return;
        }
        refusal.set(None);
        downloading.set(true);
        let q = q_for_submit.clone();
        let fmt = format.get_untracked();
        let alive = Arc::clone(&alive);
        spawn_local(async move {
            match api::export(&q, &fmt, None).await {
                Ok((bytes, filename)) => {
                    let mime = mime_for_format(&fmt);
                    if let Err(e) = download::trigger_download(&bytes, &filename, mime) {
                        bus.push(ToastKind::Error, "Download failed", Some(e));
                    } else {
                        bus.push(
                            ToastKind::Success,
                            "Exported",
                            Some(format!("{filename} ({} bytes)", bytes.len())),
                        );
                        if alive.load(Ordering::Acquire) {
                            on_close.run(true);
                        }
                    }
                    // The successful close above may dispose this owner.
                    downloading.try_set(false);
                }
                Err(e) => {
                    downloading.try_set(false);
                    bus.push(ToastKind::Error, "Export failed", Some(e.to_string()));
                }
            }
        });
    });

    let cancel = Callback::new(move |()| on_close.run(false));

    view! {
        <Modal
            title="Export results"
            icon=Icon::Download
            on_cancel=cancel
            on_submit=do_download
            footer=Box::new(move || view! {
                <Btn variant=Variant::Secondary on_click=cancel>{move || if downloading.get() { "Close" } else { "Cancel" }}</Btn>
                <Btn variant=Variant::Primary disabled=downloading on_click=do_download>
                    {move || if downloading.get() { "Downloading…" } else { "Download" }}
                </Btn>
            }.into_any())
        >
            <div class="m-field">
                <label>"Query"</label>
                <div class="preview" title=query.clone()>{query.clone()}</div>
            </div>

            <Show when=move || refusal.get().is_some()>
                <div class="m-refusal">{move || refusal.get().unwrap_or_default()}</div>
            </Show>

            <div class="m-field">
                <label>"Format"</label>
                <Segmented
                    full=true
                    options=vec![
                        SegmentedOption::new("csv", "CSV"),
                        SegmentedOption::new("json", "JSON"),
                        SegmentedOption::new("parquet", "Parquet"),
                    ]
                    active=Signal::derive(move || format_id(&format.get()).to_string())
                    on_change=Callback::new(move |id: String| format.set(format_from_id(&id)))
                />
            </div>
        </Modal>
    }
}

/// Id ↔ enum adapters: `fleet_ui::Segmented` speaks string ids, so the
/// typed option enum stays app-side.
fn format_id(fmt: &ExportFormat) -> &'static str {
    match fmt {
        ExportFormat::Csv => "csv",
        ExportFormat::Json => "json",
        ExportFormat::Parquet => "parquet",
    }
}

fn format_from_id(id: &str) -> ExportFormat {
    match id {
        "json" => ExportFormat::Json,
        "parquet" => ExportFormat::Parquet,
        _ => ExportFormat::Csv,
    }
}

fn mime_for_format(fmt: &ExportFormat) -> &'static str {
    match fmt {
        ExportFormat::Csv => "text/csv",
        ExportFormat::Json => "application/x-ndjson",
        ExportFormat::Parquet => "application/vnd.apache.parquet",
    }
}
