// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ExportModal/>` — download query results as CSV, JSON, or Parquet.
//!
//! Built on `fleet_ui::Modal` (issue #28): the shell owns the scrim,
//! Escape, Cmd/Ctrl+Enter submit, and the header (Download icon chip +
//! close); this component owns the format picker, the download flow,
//! and the footer hint/buttons.

use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::ExportFormat;

use crate::api;
use crate::download;
use fleet_ui::{Btn, Icon, Kbd, Modal, ToastBus, ToastKind, Variant};

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

    let q_for_submit = query.clone();
    let do_download = Callback::new(move |()| {
        if downloading.get_untracked() {
            return;
        }
        downloading.set(true);
        let q = q_for_submit.clone();
        spawn_local(async move {
            let fmt = format.get_untracked();
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
                        on_close.run(true);
                    }
                    downloading.set(false);
                }
                Err(e) => {
                    downloading.set(false);
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
                <div class="hint">
                    <Kbd>"⌘⏎"</Kbd>
                    " download"
                    <span style="opacity:.5">"·"</span>
                    <Kbd>"Esc"</Kbd>
                    " cancel"
                </div>
                <Btn variant=Variant::Secondary on_click=cancel>"Cancel"</Btn>
                <Btn variant=Variant::Primary disabled=downloading on_click=do_download>
                    {move || if downloading.get() { "Downloading…" } else { "Download" }}
                </Btn>
            }.into_any())
        >
            <div class="m-field">
                <label>"Query"</label>
                <div class="preview" title=query.clone()>{query.clone()}</div>
            </div>

            <div class="m-field">
                <label>"Format"</label>
                <div class="export-formats">
                    <FormatButton label="CSV" value=ExportFormat::Csv current=format/>
                    <FormatButton label="JSON" value=ExportFormat::Json current=format/>
                    <FormatButton label="Parquet" value=ExportFormat::Parquet current=format/>
                </div>
            </div>
        </Modal>
    }
}

#[component]
fn FormatButton(
    label: &'static str,
    value: ExportFormat,
    current: RwSignal<ExportFormat>,
) -> impl IntoView {
    let value_for_click = value.clone();
    view! {
        <button
            class="fmt-btn"
            class:active=move || current.get() == value
            on:click=move |_| current.set(value_for_click.clone())
        >
            {label}
        </button>
    }
}

fn mime_for_format(fmt: &ExportFormat) -> &'static str {
    match fmt {
        ExportFormat::Csv => "text/csv",
        ExportFormat::Json => "application/x-ndjson",
        ExportFormat::Parquet => "application/vnd.apache.parquet",
    }
}
