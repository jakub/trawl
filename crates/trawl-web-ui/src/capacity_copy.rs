// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The sentences of the Health page's Disk and retention card
//! (ADR-0042).
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use crate::service_card_fmt::format_bytes;
use trawl_api::{
    DeletionFloor, EnvironmentCapacity, FilesystemRole, LastSweep, Reach, ReachEnd,
    StorageMeasurement, StorageMeasurementStatus, SweepOutcome, WithheldReason,
};

pub fn headroom_state(
    measurement: StorageMeasurement,
) -> Result<(&'static str, u64), &'static str> {
    match (measurement.status, measurement.sample_age_secs) {
        (StorageMeasurementStatus::NotConfigured, _) => Err("Not configured"),
        (StorageMeasurementStatus::NotSampled, _) => Err("Awaiting measurement"),
        (StorageMeasurementStatus::Failed, None) => {
            Err("Measurement unavailable; collection failed")
        }
        (StorageMeasurementStatus::Failed, Some(age)) => {
            Ok(("Collection failed; last complete reading", age))
        }
        (StorageMeasurementStatus::Complete, Some(age)) => Ok(("Complete measurement", age)),
        // The producer requires an age on complete samples (see
        // `storage_reading`).
        (StorageMeasurementStatus::Complete, None) => Err("Measurement unavailable"),
    }
}

pub fn roles_key(roles: &[FilesystemRole]) -> String {
    roles
        .iter()
        .map(|role| match role {
            FilesystemRole::Data => "data",
            FilesystemRole::Wal => "wal",
            FilesystemRole::Spill => "spill",
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn roles_label(roles: &[FilesystemRole]) -> String {
    roles
        .iter()
        .map(|role| match role {
            FilesystemRole::Data => "Data",
            FilesystemRole::Wal => "WAL",
            FilesystemRole::Spill => "Spill",
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

pub fn floor_line(floor: DeletionFloor) -> String {
    match floor {
        DeletionFloor::Off => "Pressure deletion off (floor 0).".into(),
        DeletionFloor::Armed {
            floor_bytes,
            deficit_bytes: 0,
        } => format!("Deletion floor: {}.", format_bytes(floor_bytes)),
        DeletionFloor::Armed {
            floor_bytes,
            deficit_bytes,
        } => format!(
            "Deletion floor: {}. Deficit: {} below the floor.",
            format_bytes(floor_bytes),
            format_bytes(deficit_bytes)
        ),
    }
}

pub fn sweep_line(last: Option<LastSweep>) -> String {
    let Some(last) = last else {
        return "No sweep yet since process start".into();
    };
    let outcome = match last.outcome {
        SweepOutcome::Completed => "Completed",
        SweepOutcome::Suppressed => "Suppressed",
        SweepOutcome::Failed => "Failed",
        SweepOutcome::ExhaustedBelowFloor => "Ran out of candidates below the floor",
    };
    format!("{outcome}, {}s ago", last.age_secs)
}

pub fn policy_line(env: &EnvironmentCapacity) -> String {
    let policy = if env.max_age_days == 0 {
        "keep forever".to_owned()
    } else {
        format!("{} days", env.max_age_days)
    };
    format!(
        "Policy: {policy}. Oldest date: {}. Stored: {}.",
        env.oldest_date,
        format_bytes(env.stored_bytes)
    )
}

fn reach_end(end: ReachEnd, max_age_days: u64) -> String {
    match end {
        ReachEnd::Days { days } => format!("about {days} of {max_age_days} days"),
        ReachEnd::FullPolicy => format!("the full {max_age_days} days"),
        ReachEnd::DiskFillsFirst => "the disk fills before retention is reached".into(),
    }
}

/// The reach sentence for the range from the largest observed day (`low`)
/// to the mean (`high`).
pub fn reach_sentence(low: ReachEnd, high: ReachEnd, max_age_days: u64) -> String {
    let range = match (low, high) {
        (ReachEnd::Days { days: a }, ReachEnd::Days { days: b }) if a == b => {
            format!("about {a} of {max_age_days} days")
        }
        (ReachEnd::Days { days: a }, ReachEnd::Days { days: b }) => {
            format!("about {a}\u{2013}{b} of {max_age_days} days")
        }
        (ReachEnd::FullPolicy, ReachEnd::FullPolicy) => format!("the full {max_age_days} days"),
        (ReachEnd::Days { days }, ReachEnd::FullPolicy) => {
            format!("about {days} days to the full {max_age_days}")
        }
        (ReachEnd::DiskFillsFirst, ReachEnd::DiskFillsFirst) => {
            "the disk fills before retention is reached".into()
        }
        (low, high) => {
            return format!(
                "Reach if the observed days repeat: at the largest observed day, {}; at the mean day, {}.",
                reach_end(low, max_age_days),
                reach_end(high, max_age_days)
            );
        }
    };
    format!("Reach: {range} if the observed days repeat.")
}

pub fn reach_line(reach: &Reach, max_age_days: u64) -> String {
    match reach {
        Reach::Projected {
            observed_first,
            observed_last,
            observed_days,
            low,
            high,
        } => format!(
            "{} Observed: {observed_days} days, {observed_first} to {observed_last}.",
            reach_sentence(*low, *high, max_age_days)
        ),
        Reach::KeepForever {
            observed_first,
            observed_last,
            observed_days,
            mean_daily_bytes,
        } => format!(
            "Mean growth: {} per day. Observed: {observed_days} days, {observed_first} to {observed_last}.",
            format_bytes(*mean_daily_bytes)
        ),
        Reach::Withheld { reason } => match reason {
            WithheldReason::InsufficientHistory => {
                "Reach withheld: not enough observed days yet.".into()
            }
            WithheldReason::RetentionSuppressed => {
                "Reach withheld: a repin ran during the latest samples, or its staging is still on disk, so they may count files twice or miss free space."
                    .into()
            }
            WithheldReason::MeasurementUnavailable => {
                "Reach withheld: measurement unavailable; the disk or Parquet sample is not complete."
                    .into()
            }
        },
    }
}

pub fn empty_reach_line(parquet: StorageMeasurement) -> &'static str {
    match (parquet.status, parquet.sample_age_secs) {
        (StorageMeasurementStatus::Complete, Some(_)) => "No stored date partitions yet.",
        (StorageMeasurementStatus::Failed, Some(_)) => {
            "Collection failed; the last complete scan found no stored date partitions."
        }
        (StorageMeasurementStatus::NotSampled, _) => "Awaiting measurement",
        (StorageMeasurementStatus::Failed, None) => "Measurement unavailable; collection failed",
        (StorageMeasurementStatus::NotConfigured, _) => "Not configured",
        // The producer requires an age on complete samples.
        (StorageMeasurementStatus::Complete, None) => "Measurement unavailable",
    }
}

/// Split `line` into runs, marking each ISO date (`YYYY-MM-DD`, not
/// inside a longer digit run). The card sets each date in its own
/// unbreakable run so a narrow screen never wraps one at its hyphen; the
/// runs join back to `line` unchanged.
pub fn date_runs(line: &str) -> Vec<(&str, bool)> {
    const SHAPE: &[u8; 10] = b"dddd-dd-dd";
    let bytes = line.as_bytes();
    let digit_at = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
    let date_at = |start: usize| {
        bytes.len() >= start + SHAPE.len()
            && SHAPE.iter().enumerate().all(|(offset, want)| match want {
                b'd' => digit_at(start + offset),
                _ => bytes[start + offset] == *want,
            })
            && !(start > 0 && digit_at(start - 1))
            && !digit_at(start + SHAPE.len())
    };
    let mut runs = Vec::new();
    let (mut plain_from, mut at) = (0, 0);
    while at < bytes.len() {
        if date_at(at) {
            if plain_from < at {
                runs.push((&line[plain_from..at], false));
            }
            runs.push((&line[at..at + SHAPE.len()], true));
            at += SHAPE.len();
            plain_from = at;
        } else {
            at += 1;
        }
    }
    if plain_from < bytes.len() {
        runs.push((&line[plain_from..], false));
    }
    runs
}

pub fn growth_excluded_note(excluded: &[String]) -> Option<String> {
    if excluded.is_empty() {
        return None;
    }
    Some(format!(
        "Excludes growth of {}: not enough history yet.",
        excluded.join(", ")
    ))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reach_line_reads_every_end_pair_as_a_sentence() {
        const OBSERVED: &str = " Observed: 7 days, 2026-09-19 to 2026-09-25.";
        let days = |days| ReachEnd::Days { days };
        let line = |low, high, max_age_days| {
            reach_line(
                &Reach::Projected {
                    observed_first: "2026-09-19".into(),
                    observed_last: "2026-09-25".into(),
                    observed_days: 7,
                    low,
                    high,
                },
                max_age_days,
            )
        };
        let full = ReachEnd::FullPolicy;
        let disk = ReachEnd::DiskFillsFirst;
        for (low, high, max_age_days, sentence) in [
            // Reachable with a floor.
            (
                days(38),
                days(52),
                90,
                "Reach: about 38\u{2013}52 of 90 days if the observed days repeat.",
            ),
            (
                days(38),
                days(38),
                90,
                "Reach: about 38 of 90 days if the observed days repeat.",
            ),
            (
                days(38),
                full,
                90,
                "Reach: about 38 days to the full 90 if the observed days repeat.",
            ),
            (
                full,
                full,
                90,
                "Reach: the full 90 days if the observed days repeat.",
            ),
            // Reachable with a floor of 0.
            (
                disk,
                disk,
                7,
                "Reach: the disk fills before retention is reached if the observed days repeat.",
            ),
            (
                disk,
                full,
                7,
                "Reach if the observed days repeat: at the largest observed day, the disk fills \
                 before retention is reached; at the mean day, the full 7 days.",
            ),
            // Not produced by the server, still a sentence.
            (
                disk,
                days(3),
                7,
                "Reach if the observed days repeat: at the largest observed day, the disk fills \
                 before retention is reached; at the mean day, about 3 of 7 days.",
            ),
            (
                full,
                disk,
                7,
                "Reach if the observed days repeat: at the largest observed day, the full 7 days; \
                 at the mean day, the disk fills before retention is reached.",
            ),
        ] {
            assert_eq!(
                line(low, high, max_age_days),
                format!("{sentence}{OBSERVED}"),
                "{low:?} {high:?}"
            );
        }
    }

    #[test]
    fn empty_reach_says_measured_only_after_a_complete_scan() {
        let parquet = |status, sample_age_secs| StorageMeasurement {
            status,
            sample_age_secs,
        };
        for (measurement, line) in [
            (
                parquet(StorageMeasurementStatus::Complete, Some(2)),
                "No stored date partitions yet.",
            ),
            (
                parquet(StorageMeasurementStatus::Complete, Some(0)),
                "No stored date partitions yet.",
            ),
            (
                parquet(StorageMeasurementStatus::NotSampled, None),
                "Awaiting measurement",
            ),
            (
                parquet(StorageMeasurementStatus::Failed, None),
                "Measurement unavailable; collection failed",
            ),
            (
                parquet(StorageMeasurementStatus::Failed, Some(3600)),
                "Collection failed; the last complete scan found no stored date partitions.",
            ),
            (
                parquet(StorageMeasurementStatus::NotConfigured, None),
                "Not configured",
            ),
            (
                parquet(StorageMeasurementStatus::Complete, None),
                "Measurement unavailable",
            ),
        ] {
            assert_eq!(empty_reach_line(measurement), line, "{measurement:?}");
        }
    }

    #[test]
    fn date_runs_split_out_each_iso_date_and_keep_the_text() {
        let joined =
            |runs: Vec<(&str, bool)>| runs.into_iter().map(|(text, _)| text).collect::<String>();
        let line = "Policy: 90 days. Oldest date: 2026-08-15. Stored: 111.1 GB.";
        assert_eq!(
            date_runs(line),
            [
                ("Policy: 90 days. Oldest date: ", false),
                ("2026-08-15", true),
                (". Stored: 111.1 GB.", false),
            ]
        );
        let observed = "Observed: 7 days, 2026-09-19 to 2026-09-25.";
        assert_eq!(
            date_runs(observed),
            [
                ("Observed: 7 days, ", false),
                ("2026-09-19", true),
                (" to ", false),
                ("2026-09-25", true),
                (".", false),
            ]
        );
        assert_eq!(date_runs("2026-09-25"), [("2026-09-25", true)]);
        for plain in [
            "",
            "Reach withheld: not enough observed days yet.",
            "12026-09-25",
            "2026-09-251",
            "2026-9-25 and 2026/09/25",
            "Deficit: 274 MB below the floor.",
        ] {
            let runs = date_runs(plain);
            assert!(runs.iter().all(|(_, date)| !date), "{plain}: {runs:?}");
            assert_eq!(joined(runs), plain);
        }
        for text in [line, observed] {
            assert_eq!(joined(date_runs(text)), text);
        }
    }

    #[test]
    fn withheld_reach_carries_no_digit() {
        for reason in [
            WithheldReason::InsufficientHistory,
            WithheldReason::RetentionSuppressed,
            WithheldReason::MeasurementUnavailable,
        ] {
            let line = reach_line(&Reach::Withheld { reason }, 90);
            assert!(line.starts_with("Reach withheld: "), "{line}");
            assert!(!line.chars().any(|c| c.is_ascii_digit()), "{line}");
        }
    }

    #[test]
    fn floor_and_sweep_lines_state_facts() {
        assert_eq!(
            floor_line(DeletionFloor::Off),
            "Pressure deletion off (floor 0)."
        );
        assert_eq!(
            floor_line(DeletionFloor::Armed {
                floor_bytes: 1_073_741_824,
                deficit_bytes: 0
            }),
            "Deletion floor: 1.1 GB."
        );
        assert_eq!(
            floor_line(DeletionFloor::Armed {
                floor_bytes: 1_073_741_824,
                deficit_bytes: 273_741_824
            }),
            "Deletion floor: 1.1 GB. Deficit: 274 MB below the floor."
        );
        assert_eq!(sweep_line(None), "No sweep yet since process start");
        assert_eq!(
            sweep_line(Some(LastSweep {
                outcome: SweepOutcome::Suppressed,
                age_secs: 300
            })),
            "Suppressed, 300s ago"
        );
        assert_eq!(
            sweep_line(Some(LastSweep {
                outcome: SweepOutcome::ExhaustedBelowFloor,
                age_secs: 61
            })),
            "Ran out of candidates below the floor, 61s ago"
        );
        assert_eq!(
            headroom_state(StorageMeasurement {
                status: StorageMeasurementStatus::Failed,
                sample_age_secs: Some(3600)
            }),
            Ok(("Collection failed; last complete reading", 3600))
        );
        assert_eq!(
            headroom_state(StorageMeasurement {
                status: StorageMeasurementStatus::NotSampled,
                sample_age_secs: None
            }),
            Err("Awaiting measurement")
        );
    }

    /// Each Health e2e capacity fixture is a state the server can emit
    /// (`tests/e2e_wire_fixture_contract.rs`). Its key sentences, read
    /// through the copy the card renders, are the ones the spec asserts.
    #[test]
    fn fixture_scenarios_read_as_their_sentences() {
        const OBSERVED: &str = "Observed: 7 days, 2026-09-19 to 2026-09-25.";
        const LAB_OBSERVED: &str = "Observed: 6 days, 2026-09-20 to 2026-09-25.";
        let capacity = |text: &str| -> trawl_api::Capacity {
            let part: serde_json::Value = serde_json::from_str(text).unwrap();
            serde_json::from_value(part["capacity"].clone()).unwrap()
        };
        let fills = "Reach if the observed days repeat: at the largest observed day, the disk \
                     fills before retention is reached;";
        let history = "Reach withheld: not enough observed days yet.";
        for (fixture, text, lines, excluded) in [
            (
                "complete",
                include_str!("../e2e/harness/wire/health-capacity-complete.json"),
                vec![
                    ("prod", format!("Reach: about 38\u{2013}52 of 90 days if the observed days repeat. {OBSERVED}")),
                    ("staging", format!("Reach: about 12\u{2013}17 of 30 days if the observed days repeat. {OBSERVED}")),
                    ("lab", format!("Reach: about 3\u{2013}4 of 7 days if the observed days repeat. {LAB_OBSERVED}")),
                    ("archive", format!("Mean growth: 120 MB per day. {OBSERVED}")),
                    ("k8s", history.to_owned()),
                ],
                Some("Excludes growth of k8s: not enough history yet."),
            ),
            (
                "failed-retained",
                include_str!("../e2e/harness/wire/health-capacity-failed-retained.json"),
                ["archive", "edge", "k8s", "prod"]
                    .map(|env| (env, "Reach withheld: measurement unavailable; the disk or Parquet sample is not complete.".to_owned()))
                    .to_vec(),
                None,
            ),
            (
                "repin-suppressed",
                include_str!("../e2e/harness/wire/health-capacity-repin-suppressed.json"),
                ["k8s", "prod", "staging"]
                    .map(|env| (env, "Reach withheld: a repin ran during the latest samples, or its staging is still on disk, so they may count files twice or miss free space.".to_owned()))
                    .to_vec(),
                None,
            ),
            (
                "floor-zero",
                include_str!("../e2e/harness/wire/health-capacity-floor-zero.json"),
                vec![
                    ("prod", format!("{fills} at the mean day, the full 90 days. {OBSERVED}")),
                    ("lab", format!("{fills} at the mean day, the full 7 days. {LAB_OBSERVED}")),
                    ("fresh", history.to_owned()),
                ],
                Some("Excludes growth of fresh: not enough history yet."),
            ),
            (
                "pressure",
                include_str!("../e2e/harness/wire/health-capacity-pressure.json"),
                ["archive", "lab", "prod"]
                    .map(|env| (env, history.to_owned()))
                    .to_vec(),
                Some("Excludes growth of lab, prod: not enough history yet."),
            ),
        ] {
            let capacity = capacity(text);
            let rendered = capacity
                .environments
                .iter()
                .map(|env| (env.env.as_str(), reach_line(&env.reach, env.max_age_days)))
                .collect::<Vec<_>>();
            for (env, line) in &lines {
                assert!(
                    rendered.contains(&(*env, line.clone())),
                    "{fixture} {env}: {rendered:#?}"
                );
            }
            assert_eq!(rendered.len(), lines.len(), "{fixture}");
            assert_eq!(
                growth_excluded_note(&capacity.growth_excluded).as_deref(),
                excluded,
                "{fixture}"
            );
        }
    }
}
