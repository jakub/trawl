// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Initial guidance shared by the Events and Visualization tabs.

use leptos::prelude::*;

#[component]
pub fn SearchQuickStart(on_run: Callback<&'static str>) -> impl IntoView {
    let examples = [
        ("Explore events", "* | head 20"),
        ("Compare services", "* | stats count() by service"),
        (
            "Rank services by errors",
            "_severity>=error | stats count() as errors by service | sort -errors | head 10",
        ),
        (
            "Chart web warnings and errors",
            "service=web _severity>=warn | timechart span=5m count()",
        ),
    ];
    let sender_fields = [
        ("service", "Service name"),
        ("host", "Origin host"),
        ("env", "Environment"),
        ("message", "Sender message"),
    ];
    let reserved_fields = [
        ("_time", "Event time"),
        ("_ingested", "Arrival time"),
        ("_raw", "Original event text"),
        ("_severity", "Derived severity, 1–24"),
        ("_producer", "Ingest source: http, syslog, trawld"),
        ("_repairs", "Ingest repair codes"),
    ];
    let severity_bands = [
        ("trace", "1–4"),
        ("debug", "5–8"),
        ("info", "9–12"),
        ("warn", "13–16"),
        ("error", "17–20"),
        ("fatal", "21–24"),
    ];

    view! {
        <div id="search-results" class="results search-quick-start" role="region" aria-label="Search results" tabindex="0">
            <div class="qs-guide">
                <h2>"Quick start"</h2>
                <div class="qs-examples">
                    {examples.into_iter().map(|(name, query)| view! {
                        <div class="qs-example">
                            <code>{query}</code>
                            <button type="button" class="btn-sec btn-xs" aria-label=format!("Run {name}") on:click=move |_| on_run.run(query)>"Run"</button>
                        </div>
                    }).collect_view()}
                </div>
                <div class="qs-reference">
                    <section>
                        <h3>"Default sender fields"</h3>
                        <dl class="qs-fields">
                            {sender_fields.into_iter().map(|(name, meaning)| view! {
                                <div><dt><code>{name}</code></dt><dd>{meaning}</dd></div>
                            }).collect_view()}
                        </dl>
                        <p class="qs-field-note"><code>"timestamp"</code>", "<code>"level"</code>" and "<code>"severity"</code>" are sender fields, not aliases."</p>
                    </section>
                    <section>
                        <h3>"Reserved fields"</h3>
                        <dl class="qs-fields">
                            {reserved_fields.into_iter().map(|(name, meaning)| view! {
                                <div><dt><code>{name}</code></dt><dd>{meaning}</dd></div>
                            }).collect_view()}
                        </dl>
                    </section>
                </div>
                <section class="qs-severity">
                    <h3>"Severity"</h3>
                    <div class="qs-bands">
                        {severity_bands.into_iter().map(|(name, numbers)| view! {
                            <span>{name}<span>{numbers}</span></span>
                        }).collect_view()}
                    </div>
                    <div class="qs-predicates">
                        <span><code>"_severity=error"</code>" matches 17–20"</span>
                        <span><code>"_severity>=error"</code>" matches 17–24"</span>
                    </div>
                </section>
                <div class="qs-links">
                    <a href="https://trawl.sh/reference/dsl/" target="_blank" rel="noopener noreferrer">"Full query reference ↗"</a>
                    <a href="https://trawl.sh/reference/events/" target="_blank" rel="noopener noreferrer">"Event reference ↗"</a>
                </div>
            </div>
        </div>
    }
}
