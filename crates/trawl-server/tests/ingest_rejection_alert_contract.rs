// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which `RejectReason`s `TrawlIngestEventsRejected` alerts on (ADR-0034,
//! ADR-0053).
//!
//! The alert selects `trawl_ingest_events_rejected_total` by excluding the
//! reasons that have their own signal: `wal_failure` is
//! `TrawlHttpPersistenceRejection`, and `hot_buffer_full` and
//! `ingest_batch_too_large` are whole-request 503 and 413 refusals that a
//! sender sees. Everything else is a per-event refusal inside a request
//! that answered normally. A new reason alerts without a rule edit, so
//! adding a variant must make someone decide which side it is on: the
//! match below has no wildcard arm.
//!
//! This test ties that decision to the exclusion matcher in the expected
//! inventory, the plain rule pack and the chart template. The Python
//! rendering test proves the packs equal the inventory, and promtool proves
//! the behaviour.

use std::collections::BTreeSet;

use serde_json::Value;
use trawl_server::ingest::envelope::RejectReason;

const EXPECTED: &str = include_str!("../../../monitoring/prometheus/tests/expected.json");
const PLAIN: &str = include_str!("../../../monitoring/prometheus/trawl.rules.yml");
const CHART: &str = include_str!("../../../chart/trawl/templates/prometheusrule.yaml");

/// Whether `TrawlIngestEventsRejected` alerts on `reason`.
fn alerted(reason: RejectReason) -> bool {
    match reason {
        RejectReason::MissingService
        | RejectReason::ServiceNotString
        | RejectReason::EmptyService
        | RejectReason::ServiceTooLong
        | RejectReason::InvalidChars
        | RejectReason::InvalidEnv
        | RejectReason::EnvNotAllowed
        | RejectReason::HostMissingFromRelay
        | RejectReason::NotObject
        | RejectReason::InvalidJson => true,
        // Own alert (`TrawlHttpPersistenceRejection`), and the whole-request
        // 503 and 413 refusals.
        RejectReason::WalFailure
        | RejectReason::HotBufferFull
        | RejectReason::IngestBatchTooLarge => false,
    }
}

fn reasons(want_alerted: bool) -> Vec<&'static str> {
    RejectReason::ALL
        .iter()
        .filter(|reason| alerted(**reason) == want_alerted)
        .map(|reason| reason.as_str())
        .collect()
}

#[test]
fn the_two_classes_partition_every_reject_reason() {
    let (alerting, excluded) = (reasons(true), reasons(false));
    assert!(!alerting.is_empty() && !excluded.is_empty());
    let all: BTreeSet<&str> = RejectReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(all.len(), RejectReason::ALL.len(), "labels are distinct");
    let alerting: BTreeSet<&str> = alerting.into_iter().collect();
    let excluded: BTreeSet<&str> = excluded.into_iter().collect();
    assert!(alerting.is_disjoint(&excluded));
    assert_eq!(&alerting | &excluded, all);
}

#[test]
fn the_alert_excludes_exactly_the_reasons_with_their_own_signal() {
    let matcher = format!("reason!~\"{}\"", reasons(false).join("|"));

    let inventory: Vec<Value> = serde_json::from_str(EXPECTED).expect("expected.json parses");
    let entries: Vec<&Value> = inventory
        .iter()
        .filter(|entry| entry["alert"] == "TrawlIngestEventsRejected")
        .collect();
    assert_eq!(entries.len(), 1, "one inventory entry");
    let entry = entries[0];
    assert_eq!(entry["family"], "increase");
    assert_eq!(entry["metric"], "trawl_ingest_events_rejected_total");
    assert_eq!(entry["matcher"], matcher.as_str());

    let variants: BTreeSet<String> = entry["variants"]
        .as_array()
        .expect("variants is a list")
        .iter()
        .map(|variant| {
            let labels = variant.as_object().expect("a variant is a label set");
            assert_eq!(labels.len(), 1, "{variant}");
            labels["reason"].as_str().expect("reason label").to_owned()
        })
        .collect();
    let alerting: BTreeSet<String> = reasons(true).into_iter().map(str::to_owned).collect();
    assert_eq!(
        variants, alerting,
        "the inventory's variants are the alerted class"
    );

    assert!(
        PLAIN.contains(&format!("{{job=\"trawl\",{matcher}}}[10m]")),
        "the plain rule pack does not select with {matcher}"
    );
    assert!(
        CHART.contains(&format!(",{matcher}}}[10m]")),
        "the chart template does not select with {matcher}"
    );
}
