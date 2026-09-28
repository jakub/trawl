// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The sentences of the Health page's Disk and retention card
//! (ADR-0042). Pure so native `cargo test` pins the copy: a headroom row
//! shares its attempt's status and age, a floor is stated with its
//! deficit only while below it, a reach range never averages mixed ends,
//! and a withheld reach carries no digit.
//!
//! On native, only the tests consume some of these items: the card that
//! renders them is wasm-only.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use crate::service_card_fmt::format_bytes;
use trawl_api::{
    DeletionFloor, EnvironmentCapacity, FilesystemRole, LastSweep, Reach, ReachEnd,
    StorageMeasurement, StorageMeasurementStatus, SweepOutcome, WithheldReason,
};

/// The shared state of one headroom attempt (ADR-0042): every row comes
/// from the same attempt, so each row carries the same prefix and age.
/// `Err` is the one line the group shows when no complete sample exists;
/// a failed attempt that retained a sample keeps its aged rows.
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

/// The wire names of a row's roles, for `data-roles`: `"data wal"`.
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

/// The row's title: the roles that share the filesystem, `"Data + WAL"`.
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

/// The data row's floor: the floor and, only while below it, the deficit.
/// Equality is not a deficit, so a zero deficit is not mentioned.
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

/// The last sweep in words with its age, or the fact that none ran yet.
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

/// The environment's policy beside the facts that survive a restart.
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

/// One end of a range when the two ends need spelling out separately.
fn reach_end(end: ReachEnd, max_age_days: u64) -> String {
    match end {
        ReachEnd::Days { days } => format!("about {days} days"),
        ReachEnd::FullPolicy => format!("the full {max_age_days} days"),
        ReachEnd::DiskFillsFirst => "the disk filling before retention is reached".into(),
    }
}

/// The range from the largest observed day (`low`) to the mean (`high`).
/// Matching ends collapse into one phrase; mixed ends stay honest.
pub fn reach_range(low: ReachEnd, high: ReachEnd, max_age_days: u64) -> String {
    match (low, high) {
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
        (low, high) => format!(
            "{} to {}",
            reach_end(low, max_age_days),
            reach_end(high, max_age_days)
        ),
    }
}

/// The reach sentence. A projection names the observed days it came
/// from and the condition it holds under. Keep-forever data gets its
/// growth and no time claim. A withheld reach gets its reason and no
/// digit at all, so nothing on the row can be read as a number of days.
pub fn reach_line(reach: &Reach, max_age_days: u64) -> String {
    match reach {
        Reach::Projected {
            observed_first,
            observed_last,
            observed_days,
            low,
            high,
        } => format!(
            "Reach: {} if the observed days repeat. Observed: {observed_days} days, {observed_first} to {observed_last}.",
            reach_range(*low, *high, max_age_days)
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
                "Reach withheld: a repin holds two generations, so stored bytes are inflated."
                    .into()
            }
            WithheldReason::MeasurementUnavailable => {
                "Reach withheld: measurement unavailable; the disk or Parquet sample is not complete."
                    .into()
            }
        },
    }
}

/// The environments whose growth the projection left out (ADR-0042): a
/// finite policy with too few observed days is reserved at its stored
/// bytes. `None` when nothing was left out.
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
    fn reach_range_reads_as_days_of_policy_and_never_averages_mixed_ends() {
        let days = |days| ReachEnd::Days { days };
        assert_eq!(
            reach_range(days(38), days(52), 90),
            "about 38\u{2013}52 of 90 days"
        );
        assert_eq!(reach_range(days(38), days(38), 90), "about 38 of 90 days");
        assert_eq!(
            reach_range(ReachEnd::FullPolicy, ReachEnd::FullPolicy, 90),
            "the full 90 days"
        );
        assert_eq!(
            reach_range(days(38), ReachEnd::FullPolicy, 90),
            "about 38 days to the full 90"
        );
        assert_eq!(
            reach_range(ReachEnd::DiskFillsFirst, ReachEnd::DiskFillsFirst, 7),
            "the disk fills before retention is reached"
        );
        assert_eq!(
            reach_range(ReachEnd::DiskFillsFirst, ReachEnd::FullPolicy, 7),
            "the disk filling before retention is reached to the full 7 days"
        );
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
}
