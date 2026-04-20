// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ExportModal/>` — download query results as CSV, JSON, or Parquet.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys;
use trawl_api::ExportFormat;
use wasm_bindgen::JsCast;

use crate::api;
use crate::components::toast::{ToastBus, ToastKind};
use crate::download;

#[component]
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub fn ExportModal(
    /// The DSL query to export. Shown read-only in the preview strip.
    query: String,
    /// Bus for success / error toasts.
    bus: ToastBus,
    /// Called on close — `true` if a download completed, `false` on cancel.
    on_close: Callback<bool>,
) -> impl IntoView {
    let format = RwSignal::new(ExportFormat::Csv);
    let downloading = RwSignal::new(false);

    let q_for_submit = query.clone();
    let do_download = move || {
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
    };
    let do_download_click = do_download.clone();
    let do_download_key = do_download.clone();

    let cancel = move || on_close.run(false);

    let on_keydown = move |e: web_sys::KeyboardEvent| match e.key().as_str() {
        "Escape" => {
            e.prevent_default();
            cancel();
        }
        "Enter" if e.meta_key() || e.ctrl_key() => {
            e.prevent_default();
            do_download_key();
        }
        _ => {}
    };

    view! {
        <div
            class="modal-scrim"
            on:mousedown=move |e: web_sys::MouseEvent| {
                if let Some(target) = e.target()
                    && let Some(el) = target.dyn_ref::<web_sys::Element>()
                    && el.class_name().contains("modal-scrim")
                {
                    cancel();
                }
            }
            on:keydown=on_keydown
        >
            <div class="modal" role="dialog" aria-modal="true">
                <div class="m-hd">
                    <span class="ic"><DownloadIcon/></span>
                    <span class="t">"Export results"</span>
                    <span class="x" title="Close (Esc)" on:click=move |_| cancel()>
                        <CloseIcon/>
                    </span>
                </div>

                <div class="m-body">
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
                </div>

                <div class="m-ft">
                    <div class="hint">
                        <span class="kbd">"⌘⏎"</span>
                        " download"
                        <span style="opacity:.5">"·"</span>
                        <span class="kbd">"Esc"</span>
                        " cancel"
                    </div>
                    <button class="btn-sec" on:click=move |_| cancel()>"Cancel"</button>
                    <button
                        class="btn-pri"
                        disabled=move || downloading.get()
                        on:click=move |_| do_download_click()
                    >
                        {move || if downloading.get() { "Downloading…" } else { "Download" }}
                    </button>
                </div>
            </div>
        </div>
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

#[component]
fn DownloadIcon() -> impl IntoView {
    view! {
        <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="M8 2v9M4 8l4 4 4-4M3 14h10"/>
        </svg>
    }
}

#[component]
fn CloseIcon() -> impl IntoView {
    view! {
        <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="m4 4 8 8M12 4l-8 8"/>
        </svg>
    }
}
