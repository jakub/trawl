// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `compare` read (ADR-0058): one filter's group rates in a
//! foreground window against a baseline window, from one statement.
//!
//! The statement is trawl-core's [`correlate::compare_query`]: one scan
//! over the hull of both windows, each event labelled with the window it
//! falls in, counted per group for both at once, then rates per minute,
//! their change and each group's status, ranked by absolute rate change,
//! then count, then group key. The captured rows ARE the ranked table, so
//! the executed text reproduces the answer and `read` pages it like any
//! capture. The statement aggregates, so the capture is whole-shaped.
//!
//! Both windows come from one snapshot: the one execution takes one
//! hot-buffer snapshot and one catalog pin set, under the publication read
//! guard the capture holds for the statement, so an event ingested while
//! it runs counts in neither window; two separate reads are not covered,
//! and retention or compaction may change the corpus between them.
//!
//! # Before the capture
//!
//! Every refusal of the request runs before [`service::run`], so it takes
//! nothing:
//!
//! 1. The `query` permission.
//! 2. The group fields: at most [`COMPARE_MAX_GROUP_FIELDS`], each at most
//!    [`MAX_FIELD_NAME_BYTES`], none twice under the DSL field fold, each
//!    refused as `bad_request` naming the cap and at most
//!    [`ERROR_EXCERPT_BYTES`] of the value sent. A name the statement
//!    assigns is refused by the builder (`correlation_invalid`).
//! 3. The windows, each resolved as a capture's window is
//!    ([`bounds::window_bounds`]) at the results clock: `last` back from
//!    now, both becoming a concrete `[start, end)`. The baseline defaults
//!    to the window of equal duration immediately before the foreground.
//!    Windows may touch; overlapping ones (`max(starts) < min(ends)`) are
//!    `windows_overlap`.
//! 4. The filter, parsed by the builder: search tokens only, so a pipe
//!    stage or a time clause is `correlation_invalid`, naming it.
//!
//! The capture's window is the hull of the two, so the span limit applies
//! to the hull through the capture's own bounds.
//!
//! # The facts
//!
//! `rows_ranked` counts the ranked rows. More than
//! [`CORRELATION_MAX_GROUPS`] is refused as `result_too_large`, with the
//! hint to raise `min_count` or drop a group field; a capture whose
//! `max_rows` is lower refuses earlier, as any whole-shaped capture over
//! its cut does.
//!
//! The warnings say what the read cannot vouch for, never that anything
//! is complete:
//!
//! - `retention_reach`: a window starts before the oldest date partition
//!   retention kept for an env the filter selects (the env it pins, else
//!   every env on disk). At most one per window: it counts the envs the
//!   window starts before, names the first eight by name with the day each
//!   is kept from, counts the rest, and says data before the latest of
//!   those days may be missing, which holds for every one of them.
//!   Partitions are named by the day they were written, not by their
//!   events' `_time`, so the warning says data before that day may be
//!   missing, not that data after it is all there. The partitions are
//!   listed on the worker after the statement ran, so one retention
//!   removed meanwhile is seen. Finding the selected envs is one pass
//!   over the data root's entries that counts every env but keeps only
//!   the first [`REACH_MAX_ENVS_SCANNED`] names. The walk is then one pass
//!   over those in name order that keeps only each window's count, latest
//!   day and first names. Both read the work's cancellation latch, between
//!   entries and between envs. Past the cap the warning says it checked
//!   the first N of M, and a window that starts before none of those
//!   still warns, since the rest were not checked.
//! - `degraded_pin`: a group field whose pin is degraded (ADR-0011), so
//!   events whose value was shelved group under null. Read from the
//!   in-process set the schema-refresh tick maintains, up to one tick
//!   stale, as the query notice reads it.
//!
//! So a compare carries at most four warnings, two of each code, whatever
//! the number of envs on disk.

use std::path::Path;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use fleet_auth::VerifiedKey;
use trawl_api::results::{
    COMPARE_MAX_GROUP_FIELDS, CaptureKind, CaptureWindow, CompareFacts, CompareRequest,
    CompareWarning, CompareWarningCode, CompareWindow, Correlation, CorrelationResponse, RateUnit,
};
use trawl_api::value::QueryResult;
use trawl_core::correlate::{self, Window};
use trawl_core::sanitize::{ERROR_EXCERPT_BYTES, excerpt};
use trawl_core::schema::{MAX_FIELD_NAME_BYTES, catalog_key};
use trawl_engine::cancel::CancelLatch;
use trawl_engine::error::EngineError;
use trawl_engine::timing::PhaseClock;

use super::bounds::{self, Limits, WindowInput};
use super::service::{self, CORRELATION_MAX_GROUPS, CaptureOverflow, Selection, Spec};
use crate::error::ServerError;
use crate::state::AppState;

/// Compare a filter's group rates across two windows (module docs).
///
/// # Errors
/// 403 without the `query` permission; 400 for the group fields, a window,
/// `windows_overlap` or `correlation_invalid`; then every refusal a
/// capture makes, and `result_too_large` over the group cap.
pub async fn compare(
    state: &AppState,
    verified: &VerifiedKey,
    request_id: String,
    req: CompareRequest,
) -> Result<CorrelationResponse, ServerError> {
    service::require_query(verified)?;
    check_groups(&req.group_by)?;
    let capture = &state.results.capture;
    let limits = Limits {
        default_window: std::time::Duration::from_secs(capture.default_window_secs),
        max_span: std::time::Duration::from_secs(capture.max_span_secs),
    };
    let now = state.results.clock.now();
    let resolve = |window: CaptureWindow| {
        bounds::window_bounds(&input(window), now, &limits).map_err(ServerError::CaptureBounds)
    };
    let foreground = resolve(req.foreground)?;
    let baseline = if let Some(window) = req.baseline {
        resolve(window)?
    } else {
        let start = foreground
            .0
            .checked_sub_signed(foreground.1 - foreground.0)
            .ok_or_else(|| {
                ServerError::BadRequest(
                    "the default baseline, the window of equal duration before the \
                     foreground, starts outside the representable range; give a baseline"
                        .into(),
                )
            })?;
        (start, foreground.0)
    };
    if foreground.0.max(baseline.0) < foreground.1.min(baseline.1) {
        return Err(ServerError::WindowsOverlap(format!(
            "the foreground window [{}, {}) and the baseline window [{}, {}) overlap; windows \
             may touch but not overlap",
            micros(foreground.0),
            micros(foreground.1),
            micros(baseline.0),
            micros(baseline.1),
        )));
    }
    let composed = correlate::compare_query(
        &req.filter,
        window(foreground),
        window(baseline),
        &req.group_by,
        req.min_count,
    )?;

    let degraded = crate::handlers::degraded_fields_for(state, [composed.text()]);
    let degraded_pins = req
        .group_by
        .iter()
        .filter(|field| degraded.contains(&catalog_key(field)))
        .map(|field| CompareWarning {
            code: CompareWarningCode::DegradedPin,
            message: format!(
                "group field {field} has a degraded pin: events whose value was shelved count \
                 under a null {field}"
            ),
            field: Some(field.clone()),
        })
        .collect();

    let hull = WindowInput {
        earliest: Some(micros(foreground.0.min(baseline.0))),
        latest: Some(micros(foreground.1.max(baseline.1))),
        last: None,
    };
    let spec = Spec {
        kind: CaptureKind::Compare,
        query: composed.text().to_owned(),
        window: Some(hull),
        shape: None,
        selection: Box::new(Ranking {
            base_dir: state.query.pool.base_dir().to_owned(),
            filter: req.filter,
            windows: [("foreground", foreground), ("baseline", baseline)],
            degraded_pins,
            ranked: 0,
            facts: CompareFacts {
                foreground: wire_window(foreground),
                baseline: wire_window(baseline),
                group_by: req.group_by,
                min_count: req.min_count,
                rate_unit: RateUnit::PerMinute,
                rows_ranked: 0,
                warnings: Vec::new(),
            },
        }),
    };
    let captured = service::run(state, verified, request_id, spec).await?;
    Ok(CorrelationResponse {
        result: captured.result,
        execution: captured.execution,
    })
}

/// Step 2: the group fields' cap, their lengths, and their distinctness
/// under the fold. A name over [`MAX_FIELD_NAME_BYTES`] is refused by its
/// length alone; the duplicate refusal quotes at most
/// [`ERROR_EXCERPT_BYTES`] of each name.
fn check_groups(groups: &[String]) -> Result<(), ServerError> {
    if groups.len() > COMPARE_MAX_GROUP_FIELDS {
        return Err(ServerError::BadRequest(format!(
            "{} group fields given; compare groups by at most {COMPARE_MAX_GROUP_FIELDS}",
            groups.len()
        )));
    }
    if let Some(long) = groups.iter().find(|name| name.len() > MAX_FIELD_NAME_BYTES) {
        return Err(ServerError::BadRequest(format!(
            "a group field name is {} bytes; at most {MAX_FIELD_NAME_BYTES} are accepted",
            long.len()
        )));
    }
    if let [first, second] = groups
        && catalog_key(first) == catalog_key(second)
    {
        return Err(ServerError::BadRequest(format!(
            "group fields {} and {} name one field; give it once",
            excerpt(first, ERROR_EXCERPT_BYTES),
            excerpt(second, ERROR_EXCERPT_BYTES)
        )));
    }
    Ok(())
}

fn input(window: CaptureWindow) -> WindowInput {
    WindowInput {
        earliest: window.earliest,
        latest: window.latest,
        last: window.last,
    }
}

/// A resolved window as the builder takes it, in microseconds.
fn window((start, end): Span) -> Window {
    Window {
        start_us: start.timestamp_micros(),
        end_us: end.timestamp_micros(),
    }
}

fn wire_window((start, end): Span) -> CompareWindow {
    CompareWindow {
        start: micros(start),
        end: micros(end),
        duration_ms: u64::try_from((end - start).num_milliseconds()).unwrap_or(0),
    }
}

/// An instant as the wire spells it: RFC 3339 UTC with microseconds.
fn micros(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// The oldest date partition retention kept under `{base}/{env}`; `None`
/// when the env has none or cannot be listed, which makes no claim.
fn oldest_partition(base: &str, env: &str) -> Option<NaiveDate> {
    std::fs::read_dir(Path::new(base).join(env))
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name();
            crate::env_dirs::date_partition(env, name.to_str()?)
        })
        .min()
}

/// A resolved window, `[start, end)`.
type Span = (DateTime<Utc>, DateTime<Utc>);

/// The compare read's selection (module docs, "The facts").
struct Ranking {
    base_dir: String,
    filter: String,
    windows: [(&'static str, Span); 2],
    degraded_pins: Vec<CompareWarning>,
    ranked: u64,
    /// The facts, every part but the count and the warnings known before
    /// the read runs.
    facts: CompareFacts,
}

/// The most envs a `retention_reach` warning names; the rest are counted.
const REACH_MAX_NAMED_ENVS: usize = 8;

/// The most selected envs whose partitions retention reach lists; past it
/// the warning says how many it checked (module docs, "The facts").
///
/// Ingest bounds an env's name (32 bytes) but not how many there are, so
/// the cap comes from the walk's cost. Each env is one directory listing,
/// about 10 µs once the capture's own source walk has warmed it (10,000
/// envs measured at about 95 ms in a debug build), so 1024 add about 10 ms
/// to a capture with a 15-second default deadline. A cold listing can cost
/// a disk seek. Finding the envs still reads every entry of the data root,
/// to count them, but keeps no more than this many names. Both passes read
/// the work's cancellation latch, which the deadline sets: discovery
/// between entries, the walk between envs.
const REACH_MAX_ENVS_SCANNED: usize = 1024;

/// What retention reach found for one window: the envs whose oldest kept
/// day is after the window's start.
#[derive(Debug, Default)]
struct Reached {
    count: usize,
    latest: Option<NaiveDate>,
    /// The first [`REACH_MAX_NAMED_ENVS`], in name order.
    named: Vec<String>,
}

impl Ranking {
    /// At most one warning per window: the selected envs whose oldest
    /// partition starts after the window does (module docs).
    ///
    /// # Errors
    /// The capture is cancelled, by its caller or its deadline.
    fn retention_reach(&self, cancel: &CancelLatch) -> Result<Vec<CompareWarning>, ServerError> {
        let envs = crate::source::first_selected_envs(
            &self.base_dir,
            &self.filter,
            REACH_MAX_ENVS_SCANNED,
            cancel,
        )?;
        self.reach_over(envs.total, envs.names.iter().map(String::as_str), cancel)
    }

    /// [`Self::retention_reach`] over `envs`, `selected` of them, in name
    /// order: one pass, at most [`REACH_MAX_ENVS_SCANNED`] listed, nothing
    /// kept per env but each window's count, latest day and first names.
    fn reach_over<'a>(
        &self,
        selected: usize,
        envs: impl Iterator<Item = &'a str>,
        cancel: &CancelLatch,
    ) -> Result<Vec<CompareWarning>, ServerError> {
        let mut reached: [Reached; 2] = Default::default();
        let mut scanned = 0_usize;
        for env in envs.take(REACH_MAX_ENVS_SCANNED) {
            if cancel.latched() {
                return Err(ServerError::Engine(EngineError::Cancelled));
            }
            scanned += 1;
            let Some(date) = oldest_partition(&self.base_dir, env) else {
                continue;
            };
            let Some(kept_from) = date.and_hms_opt(0, 0, 0).map(|at| at.and_utc()) else {
                continue;
            };
            for (&(_, (start, _)), found) in self.windows.iter().zip(&mut reached) {
                if start < kept_from {
                    found.count += 1;
                    found.latest = found.latest.max(Some(date));
                    if found.named.len() < REACH_MAX_NAMED_ENVS {
                        found.named.push(format!("{env} (kept from {date})"));
                    }
                }
            }
        }
        let capped = (scanned < selected).then(|| {
            format!(
                "retention reach was checked for the first {scanned} of {selected} selected \
                 environments"
            )
        });
        Ok(self
            .windows
            .iter()
            .zip(reached)
            .filter_map(|(&(which, (start, _)), found)| {
                let message = match (found.latest, &capped) {
                    (Some(latest), _) => {
                        let mut named = found.named;
                        if found.count > REACH_MAX_NAMED_ENVS {
                            named.push(format!("and {} more", found.count - REACH_MAX_NAMED_ENVS));
                        }
                        let count = found.count;
                        let noun = if count == 1 { "env" } else { "envs" };
                        let reach = format!(
                            "the {which} window starts at {}, before the oldest day retention \
                             kept for {count} selected {noun}: {}; data before {latest} may be \
                             missing",
                            micros(start),
                            named.join(", ")
                        );
                        match &capped {
                            Some(capped) => format!("{reach}; {capped}"),
                            None => reach,
                        }
                    }
                    (None, Some(capped)) => format!(
                        "the {which} window starts at {}; {capped}, none of which it starts \
                         before; the other {} were not checked",
                        micros(start),
                        selected - scanned
                    ),
                    (None, None) => return None,
                };
                Some(CompareWarning {
                    code: CompareWarningCode::RetentionReach,
                    message,
                    field: None,
                })
            })
            .collect())
    }
}

impl Selection for Ranking {
    /// Count the ranked rows, refused over the group cap; the statement's
    /// order stays.
    fn before_cut(&mut self, result: &mut QueryResult, _: &PhaseClock) -> Result<(), ServerError> {
        self.ranked = u64::try_from(result.rows.len()).unwrap_or(u64::MAX);
        if self.ranked > CORRELATION_MAX_GROUPS {
            return Err(ServerError::CaptureTooLarge(CaptureOverflow::Groups {
                read: "compare",
                groups: self.ranked,
                max_groups: CORRELATION_MAX_GROUPS,
                hint: "raise min_count or drop a group field",
            }));
        }
        Ok(())
    }

    fn after_cut(
        &mut self,
        _: &QueryResult,
        cancel: &CancelLatch,
    ) -> Result<Option<Correlation>, ServerError> {
        let mut facts = self.facts.clone();
        facts.rows_ranked = self.ranked;
        facts.warnings = self.retention_reach(cancel)?;
        facts.warnings.extend(self.degraded_pins.iter().cloned());
        Ok(Some(Correlation::Compare(facts)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_fields_are_capped_and_distinct_under_the_fold() {
        assert!(check_groups(&[]).is_ok());
        assert!(check_groups(&["host".into(), "service".into()]).is_ok());
        let three = ["a".to_owned(), "b".into(), "c".into()];
        let message = check_groups(&three).unwrap_err().to_string();
        assert!(message.contains('3') && message.contains('2'), "{message}");
        let message = check_groups(&["Host".into(), "host".into()])
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("\"Host\"") && message.contains("\"host\""),
            "{message}"
        );
        let message = check_groups(&["h".repeat(MAX_FIELD_NAME_BYTES + 1)])
            .unwrap_err()
            .to_string();
        assert!(
            message.ends_with("a group field name is 256 bytes; at most 255 are accepted"),
            "{message}"
        );
    }

    #[test]
    fn more_ranked_groups_than_the_cap_are_refused_naming_the_fix() {
        let at = DateTime::parse_from_rfc3339("2024-01-19T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let span = (at, at + chrono::TimeDelta::hours(1));
        let mut ranking = Ranking {
            base_dir: String::new(),
            filter: "*".into(),
            windows: [("foreground", span), ("baseline", span)],
            degraded_pins: Vec::new(),
            ranked: 0,
            facts: CompareFacts {
                foreground: wire_window(span),
                baseline: wire_window(span),
                group_by: vec!["user".into()],
                min_count: 1,
                rate_unit: RateUnit::PerMinute,
                rows_ranked: 0,
                warnings: Vec::new(),
            },
        };
        let rows = usize::try_from(CORRELATION_MAX_GROUPS).unwrap();
        let mut result = QueryResult {
            columns: vec![trawl_api::value::Column::new(
                "user",
                trawl_api::value::ColumnType::String,
            )],
            rows: vec![vec![trawl_api::value::Value::Null]; rows],
        };
        assert!(ranking.before_cut(&mut result, &PhaseClock::new()).is_ok());
        result.rows.push(vec![trawl_api::value::Value::Null]);
        let refused = ranking.before_cut(&mut result, &PhaseClock::new());
        let Err(ServerError::CaptureTooLarge(overflow)) = refused else {
            panic!("{refused:?}");
        };
        let message = overflow.to_string();
        assert!(message.contains("5001 groups"), "{message}");
        assert!(
            message.contains("raise min_count or drop a group field"),
            "{message}"
        );
    }

    fn instant(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// A ranking over `base` with every env selected and the two windows
    /// starting at `foreground` and `baseline`.
    fn reaching(base: &Path, foreground: &str, baseline: &str) -> Ranking {
        let span = |start: &str| (instant(start), instant(start) + chrono::TimeDelta::hours(1));
        let (fg, bl) = (span(foreground), span(baseline));
        Ranking {
            base_dir: base.to_str().unwrap().to_owned(),
            filter: "*".into(),
            windows: [("foreground", fg), ("baseline", bl)],
            degraded_pins: Vec::new(),
            ranked: 0,
            facts: CompareFacts {
                foreground: wire_window(fg),
                baseline: wire_window(bl),
                group_by: Vec::new(),
                min_count: 1,
                rate_unit: RateUnit::PerMinute,
                rows_ranked: 0,
                warnings: Vec::new(),
            },
        }
    }

    fn partitions(base: &Path, envs: &[(&str, &str)]) {
        for (env, date) in envs {
            std::fs::create_dir_all(base.join(env).join(date)).unwrap();
        }
    }

    /// One warning per window, listing every env it starts before in name
    /// order with its oldest day, and the latest of those days as the one
    /// before which data may be missing.
    #[test]
    fn retention_reach_is_one_warning_per_window_listing_the_envs() {
        let tmp = tempfile::tempdir().unwrap();
        partitions(
            tmp.path(),
            &[
                ("gamma", "2024-01-17"),
                ("gamma", "2024-01-20"),
                ("alpha", "2024-01-18"),
                ("beta", "2024-01-19"),
            ],
        );
        let ranking = reaching(tmp.path(), "2024-01-18T12:00:00Z", "2024-01-16T00:00:00Z");
        let warnings = ranking.retention_reach(&CancelLatch::never()).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|w| w.code == CompareWarningCode::RetentionReach && w.field.is_none())
        );
        assert_eq!(
            warnings[0].message,
            "the foreground window starts at 2024-01-18T12:00:00.000000Z, before the oldest \
             day retention kept for 1 selected env: beta (kept from 2024-01-19); data before \
             2024-01-19 may be missing"
        );
        assert_eq!(
            warnings[1].message,
            "the baseline window starts at 2024-01-16T00:00:00.000000Z, before the oldest day \
             retention kept for 3 selected envs: alpha (kept from 2024-01-18), beta (kept from \
             2024-01-19), gamma (kept from 2024-01-17); data before 2024-01-19 may be missing"
        );
    }

    /// At most eight envs are named; the rest are counted.
    #[test]
    fn retention_reach_names_eight_envs_and_counts_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let envs: Vec<String> = (1..=12).map(|n| format!("e{n:02}")).collect();
        let pairs: Vec<(&str, &str)> = envs.iter().map(|e| (e.as_str(), "2024-01-19")).collect();
        partitions(tmp.path(), &pairs);
        let ranking = reaching(tmp.path(), "2024-01-18T01:00:00Z", "2024-01-18T00:00:00Z");
        let warnings = ranking.retention_reach(&CancelLatch::never()).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        for warning in &warnings {
            let message = &warning.message;
            assert!(
                message.contains("for 12 selected envs: e01 (kept from 2024-01-19)"),
                "{message}"
            );
            assert!(
                message.contains("e08 (kept from 2024-01-19), and 4 more;"),
                "{message}"
            );
            assert!(!message.contains("e09"), "{message}");
            assert!(
                message.ends_with("data before 2024-01-19 may be missing"),
                "{message}"
            );
        }
    }

    /// Past [`REACH_MAX_ENVS_SCANNED`] selected envs the walk stops, and
    /// the warning counts and names only what it checked and says so: the
    /// first eight names in order, the count of the first 1024, and their
    /// latest day, not that of an env past the cap.
    #[test]
    fn retention_reach_checks_the_first_envs_and_says_how_many() {
        let tmp = tempfile::tempdir().unwrap();
        for n in 0..10_000 {
            let date = match n {
                500 => "2024-01-25",
                5000 => "2024-02-01",
                _ => "2024-01-19",
            };
            std::fs::create_dir_all(tmp.path().join(format!("e{n:05}")).join(date)).unwrap();
        }
        let ranking = reaching(tmp.path(), "2024-01-18T01:00:00Z", "2024-01-18T00:00:00Z");
        let warnings = ranking.retention_reach(&CancelLatch::never()).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let named: Vec<String> = (0..8)
            .map(|n| format!("e{n:05} (kept from 2024-01-19)"))
            .collect();
        assert_eq!(
            warnings[0].message,
            format!(
                "the foreground window starts at 2024-01-18T01:00:00.000000Z, before the oldest \
                 day retention kept for 1024 selected envs: {}, and 1016 more; data before \
                 2024-01-25 may be missing; retention reach was checked for the first 1024 of \
                 10000 selected environments",
                named.join(", ")
            )
        );
    }

    /// A capped walk that finds no env the window starts before still
    /// warns, since the envs past the cap were not checked.
    #[test]
    fn a_capped_walk_that_finds_nothing_says_what_it_did_not_check() {
        let tmp = tempfile::tempdir().unwrap();
        for n in 0..1100 {
            std::fs::create_dir_all(tmp.path().join(format!("e{n:05}")).join("2024-01-17"))
                .unwrap();
        }
        let ranking = reaching(tmp.path(), "2024-01-18T01:00:00Z", "2024-01-18T00:00:00Z");
        let warnings = ranking.retention_reach(&CancelLatch::never()).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(
            warnings[1].message,
            "the baseline window starts at 2024-01-18T00:00:00.000000Z; retention reach was \
             checked for the first 1024 of 1100 selected environments, none of which it starts \
             before; the other 76 were not checked"
        );
        let below = reaching(tmp.path(), "2024-01-16T01:00:00Z", "2024-01-16T00:00:00Z");
        let quiet = tempfile::tempdir().unwrap();
        partitions(quiet.path(), &[("prod", "2024-01-17")]);
        let below = Ranking {
            base_dir: quiet.path().to_str().unwrap().to_owned(),
            ..below
        };
        let warnings = below.retention_reach(&CancelLatch::never()).unwrap();
        assert!(
            warnings
                .iter()
                .all(|w| !w.message.contains("checked for the first")),
            "{warnings:?}"
        );
    }

    /// A latched cancellation stops the walk at the next env.
    #[test]
    fn a_cancelled_latch_stops_the_walk() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let envs: Vec<String> = (0..100).map(|n| format!("e{n:03}")).collect();
        let pairs: Vec<(&str, &str)> = envs.iter().map(|e| (e.as_str(), "2024-01-19")).collect();
        partitions(tmp.path(), &pairs);
        let ranking = reaching(tmp.path(), "2024-01-18T01:00:00Z", "2024-01-18T00:00:00Z");
        let flag = Arc::new(AtomicBool::new(false));
        let pulled = AtomicUsize::new(0);
        let walk = envs.iter().map(|env| {
            if pulled.fetch_add(1, Ordering::SeqCst) == 3 {
                flag.store(true, Ordering::SeqCst);
            }
            env.as_str()
        });
        let refused = ranking.reach_over(envs.len(), walk, &CancelLatch::new(Arc::clone(&flag)));
        assert!(
            matches!(refused, Err(ServerError::Engine(EngineError::Cancelled))),
            "{refused:?}"
        );
        assert_eq!(
            pulled.load(Ordering::SeqCst),
            4,
            "the walk stopped at the latch"
        );
    }

    /// A latched cancellation refuses retention reach as a whole, envs
    /// found from disk included, not only the walk over envs it is given.
    #[test]
    fn a_cancelled_latch_stops_retention_reach_finding_envs() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let tmp = tempfile::tempdir().unwrap();
        partitions(tmp.path(), &[("prod", "2024-01-19"), ("lab", "2024-01-19")]);
        let ranking = reaching(tmp.path(), "2024-01-18T01:00:00Z", "2024-01-18T00:00:00Z");
        let cancelled = CancelLatch::new(Arc::new(AtomicBool::new(true)));
        let refused = ranking.retention_reach(&cancelled);
        assert!(
            matches!(refused, Err(ServerError::Engine(EngineError::Cancelled))),
            "{refused:?}"
        );
    }

    #[test]
    fn a_window_reads_as_microseconds_and_milliseconds() {
        let start = DateTime::parse_from_rfc3339("2024-01-19T10:00:00.000001Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = start + chrono::TimeDelta::hours(1);
        let wire = wire_window((start, end));
        assert_eq!(wire.start, "2024-01-19T10:00:00.000001Z");
        assert_eq!(wire.end, "2024-01-19T11:00:00.000001Z");
        assert_eq!(wire.duration_ms, 3_600_000);
        assert_eq!(window((start, end)).duration_us(), 3_600_000_000);
    }
}
