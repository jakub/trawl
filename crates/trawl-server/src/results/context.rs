// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `context` read (ADR-0058): the rows around one captured row, read
//! again as a capture of their own.
//!
//! The request names a focus row, row `row` of the caller's capture
//! `result_id`. The read covers `seconds` either side of the focus row's
//! `_time` (`[F − seconds, F + seconds + 1 µs)`, so the focus instant
//! itself is inside) on the focus keys `scope` names, with the values the
//! focus row holds. It answers three groups of rows, each ordered by
//! `_time`: up to `before` rows strictly earlier than `F`, up to
//! [`CONTEXT_SAME_TIME_ROWS`] rows at exactly `F`, and up to `after` rows
//! strictly later. The earlier rows are the ones nearest `F`, as are the
//! later ones. A window that holds no rows, such as one whose source logs
//! retention has since removed, answers three empty groups.
//!
//! The focus row is not copied into the answer. Its facts name it, by
//! result id and row, with the time, env and host the read took from it,
//! and the same-time group lists every row the new read found at `F`. The
//! focus event's own record is among them, indistinguishable by design:
//! this is a new read, and equal timestamps establish no order
//! ([`CONTEXT_STATEMENT`]).
//!
//! # Before the capture
//!
//! Everything that can refuse the request runs before [`service::run`], so
//! a refused read takes no permit, no slot and no reservation, and writes
//! nothing:
//!
//! 1. The authority ([`service::authorize`]): the `query` permission and,
//!    for a read attached to an investigation (ADR-0059), the write
//!    authority and the purpose's bound.
//! 2. The caps: `before` and `after` at most [`CONTEXT_MAX_SIDE_ROWS`].
//! 3. The focus lookup: one row of the caller's capture, read as
//!    [`service::page`] reads it, ownership and lifecycle included, so
//!    another key's temporary result is not found; another key's kept
//!    evidence is, for a key that reads investigations. A row past the end, a `_time`
//!    that is missing, null or not a timestamp, or a key the scope
//!    constrains that is missing or null is `focus_incomplete`; two
//!    columns of a name the read needs are `focus_ambiguous`. Any result
//!    shape is accepted when the row carries the fields.
//! 4. The window arithmetic. `seconds` has no cap of its own: the window
//!    meets the same limits as any capture's, so a span over
//!    `max_span_secs` is the bounds' `SpanTooLong`.
//!
//! The focus lookup precedes the read's timing account, which `run` opens.
//! It is a page read of a capture that already exists, outside the
//! executor permits, as every `GET /api/v1/results/{id}` is, and a page
//! read opens no timing account. Booking it under `dsl_check` would move
//! the composition of the read's text inside the capture core, which takes
//! that text as its input; a refused lookup is the caller's input
//! refused, like the request-shape check that also opens no account.
//!
//! # Attached
//!
//! A read naming an investigation is attached as a search capture is
//! ([`service`]'s module docs, "An attached capture"): its step is
//! appended once the capture core has planned it, so a refusal above
//! records none, and its result is kept as the investigation's evidence.
//! The focus result is not promoted: attaching is the read's own choice.

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use fleet_auth::VerifiedKey;
use trawl_api::results::{
    CONTEXT_MAX_SIDE_ROWS, CONTEXT_SAME_TIME_ROWS, CONTEXT_STATEMENT, CaptureKind, ContextFacts,
    ContextGroups, ContextRequest, ContextScope, Correlation, CorrelationResponse, FocusRow,
    RowRange, ScopeUsed,
};
use trawl_api::value::{ColumnType, QueryResult, Value};
use trawl_core::correlate;
use trawl_core::schema::TIME;
use trawl_engine::cancel::CancelLatch;
use trawl_engine::timing::PhaseClock;

use super::bounds::WindowInput;
use super::service::{self, Selection, Spec};
use crate::error::ServerError;
use crate::state::AppState;

/// Read the rows around a focus row (module docs).
///
/// # Errors
/// 403 without the `query` permission, or attached without write
/// authority on investigations; 400 for the attachment's purpose, a cap,
/// `focus_incomplete` or `focus_ambiguous`; the page read's 404, 410, 409
/// and 500 for the focus result; then every refusal a capture makes, an
/// attached capture's included.
pub async fn context(
    state: &AppState,
    verified: &VerifiedKey,
    request_id: String,
    req: ContextRequest,
) -> Result<CorrelationResponse, ServerError> {
    service::authorize(verified, req.investigation.as_ref())?;
    for (what, rows) in [("before", req.before), ("after", req.after)] {
        if rows > CONTEXT_MAX_SIDE_ROWS {
            return Err(ServerError::BadRequest(format!(
                "{what} is {rows}; a context read returns at most {CONTEXT_MAX_SIDE_ROWS} rows \
                 either side"
            )));
        }
    }
    let focus = focus(state, verified, &req.result_id, req.row, req.scope).await?;
    let window = window(focus.time, req.seconds)?;
    let composed = correlate::context_query(focus.env.as_deref(), focus.host.as_deref())?;
    let facts = ContextFacts {
        focus: FocusRow {
            result_id: req.result_id,
            row: req.row,
            time: micros(focus.time),
            env: focus.row_env,
            host: focus.row_host,
        },
        scope_used: ScopeUsed {
            env: focus.env,
            host: focus.host,
            seconds: req.seconds,
        },
        groups: ContextGroups {
            earlier: RowRange { start: 0, end: 0 },
            same_time: RowRange { start: 0, end: 0 },
            later: RowRange { start: 0, end: 0 },
        },
        statement: CONTEXT_STATEMENT.to_owned(),
    };
    let spec = Spec {
        kind: CaptureKind::Context,
        query: composed.text().to_owned(),
        window: Some(window),
        shape: None,
        selection: Box::new(Around {
            focus: focus.time,
            before: side_rows(req.before),
            after: side_rows(req.after),
            facts,
        }),
        investigation: req.investigation,
    };
    service::run(state, verified, request_id, spec)
        .await
        .map(CorrelationResponse::from)
}

/// What the read takes from the focus row.
struct Focus {
    /// The focus row's `_time`.
    time: DateTime<Utc>,
    /// The `env` the read is held to: the focus row's, when the scope
    /// names it.
    env: Option<String>,
    /// The `host` the read is held to, likewise.
    host: Option<String>,
    /// The focus row's `env`, whether or not the scope names it.
    row_env: Option<String>,
    /// The focus row's `host`, likewise.
    row_host: Option<String>,
}

/// Step 3: read the focus row and take its `_time` and the keys `scope`
/// constrains.
async fn focus(
    state: &AppState,
    verified: &VerifiedKey,
    result_id: &str,
    row: u64,
    scope: ContextScope,
) -> Result<Focus, ServerError> {
    let page = match service::page(state, verified, result_id, Some(row), Some(1), None).await {
        Ok(page) => page,
        // With one row asked and every column, the offset is the only
        // part of the request a page refuses: the row is past the end.
        // The row count comes from an empty page at the start.
        Err(ServerError::BadRequest(_)) => {
            let empty = service::page(state, verified, result_id, Some(0), Some(0), None).await?;
            return Err(past_the_end(row, empty.pagination.total));
        }
        Err(e) => return Err(e),
    };
    let Some(cells) = page.page.rows.first() else {
        return Err(past_the_end(row, page.pagination.total));
    };
    let result = &page.page;

    let time = match single(result, cells, TIME)? {
        Some((ColumnType::Timestamp, Value::String(text))) => DateTime::parse_from_rfc3339(text)
            .map(|at| at.with_timezone(&Utc))
            .map_err(|_| not_a_timestamp(row))?,
        Some((_, Value::Null)) | None => {
            return Err(ServerError::FocusIncomplete(format!(
                "focus row {row} has no `_time`; a context read is centred on it"
            )));
        }
        Some(_) => return Err(not_a_timestamp(row)),
    };
    let (env_needed, host_needed) = match scope {
        ContextScope::EnvAndHost => (true, true),
        ContextScope::Env => (true, false),
        ContextScope::Host => (false, true),
    };
    let row_env = key(result, cells, row, "env", env_needed)?;
    let row_host = key(result, cells, row, "host", host_needed)?;
    Ok(Focus {
        time,
        env: row_env.clone().filter(|_| env_needed),
        host: row_host.clone().filter(|_| host_needed),
        row_env,
        row_host,
    })
}

/// The type and cell of the one column named `name` in `cells`: `None`
/// when the result has no such column, `focus_ambiguous` when it has two.
fn single<'a>(
    result: &'a QueryResult,
    cells: &'a [Value],
    name: &str,
) -> Result<Option<(&'a ColumnType, &'a Value)>, ServerError> {
    let mut found = result
        .columns
        .iter()
        .zip(cells)
        .filter(|(column, _)| column.name == name);
    let first = found.next();
    if found.next().is_some() {
        return Err(ServerError::FocusAmbiguous(format!(
            "the focus result has two `{name}` columns, so the focus row's `{name}` is not one \
             value"
        )));
    }
    Ok(first.map(|(column, value)| (&column.ty, value)))
}

/// The focus row's text value of `name`. A key the scope constrains must
/// hold one; one it does not is reported when the row holds exactly one
/// text value and is otherwise absent, never refused.
fn key(
    result: &QueryResult,
    cells: &[Value],
    row: u64,
    name: &str,
    needed: bool,
) -> Result<Option<String>, ServerError> {
    let found = match single(result, cells, name) {
        Ok(found) => found,
        Err(ambiguous) if needed => return Err(ambiguous),
        Err(_) => None,
    };
    match found {
        Some((_, Value::String(text))) => Ok(Some(text.clone())),
        _ if !needed => Ok(None),
        Some((_, Value::Null)) | None => Err(ServerError::FocusIncomplete(format!(
            "focus row {row} has no `{name}`, which the context scope holds the read to; \
             choose a scope without it"
        ))),
        Some(_) => Err(ServerError::FocusIncomplete(format!(
            "focus row {row}'s `{name}` is not text, which the context scope holds the read to; \
             choose a scope without it"
        ))),
    }
}

fn past_the_end(row: u64, rows: usize) -> ServerError {
    ServerError::FocusIncomplete(format!(
        "focus row {row} is past the end of the result, which has {rows} captured rows"
    ))
}

fn not_a_timestamp(row: u64) -> ServerError {
    ServerError::FocusIncomplete(format!(
        "focus row {row}'s `_time` is not a timestamp; a context read is centred on it"
    ))
}

/// Step 4: `[F − seconds, F + seconds + 1 µs)` as a capture window, so the
/// bounds measure it as they measure any capture's.
fn window(focus: DateTime<Utc>, seconds: u64) -> Result<WindowInput, ServerError> {
    let span = i64::try_from(seconds).ok().and_then(TimeDelta::try_seconds);
    let bounds = span.and_then(|span| {
        let start = focus.checked_sub_signed(span)?;
        let end = focus
            .checked_add_signed(span)?
            .checked_add_signed(TimeDelta::microseconds(1))?;
        Some((start, end))
    });
    let Some((start, end)) = bounds else {
        return Err(ServerError::BadRequest(format!(
            "seconds is {seconds}; the window it makes around the focus row has an instant \
             outside the representable range"
        )));
    };
    Ok(WindowInput {
        earliest: Some(micros(start)),
        latest: Some(micros(end)),
        last: None,
    })
}

/// An instant as the wire spells it: RFC 3339 UTC with microseconds.
fn micros(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// A side cap as a row count; the caps are checked first, so it fits.
fn side_rows(rows: u64) -> usize {
    usize::try_from(rows).unwrap_or(usize::MAX)
}

/// Where a row sits against the focus instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    Earlier,
    Same,
    Later,
}

/// The context read's selection (module docs).
struct Around {
    focus: DateTime<Utc>,
    before: usize,
    after: usize,
    /// The facts, every part but the groups known before the read runs.
    facts: ContextFacts,
}

impl Around {
    /// The position of the result's `_time` column.
    fn time_column(result: &QueryResult) -> Result<usize, ServerError> {
        result
            .columns
            .iter()
            .position(|column| column.name == TIME)
            .ok_or_else(|| ServerError::Internal("a context read's result has no `_time`".into()))
    }

    /// Where `row` sits, by its `_time`; `None` for a cell no instant
    /// reads from, which no group holds.
    fn side(&self, row: &[Value], time: usize) -> Option<(DateTime<Utc>, Side)> {
        let Value::String(text) = &row[time] else {
            return None;
        };
        let at = DateTime::parse_from_rfc3339(text).ok()?.with_timezone(&Utc);
        let side = match at.cmp(&self.focus) {
            std::cmp::Ordering::Less => Side::Earlier,
            std::cmp::Ordering::Equal => Side::Same,
            std::cmp::Ordering::Greater => Side::Later,
        };
        Some((at, side))
    }

    /// How many of `kept`'s rows sit in each group, checking that they
    /// keep the groups' order.
    fn counts(&self, kept: &QueryResult) -> Result<[u64; 3], ServerError> {
        let time = Self::time_column(kept)?;
        let mut counts = [0_u64; 3];
        let mut last = Side::Earlier;
        for row in &kept.rows {
            let (_, side) = self.side(row, time).ok_or_else(|| {
                ServerError::Internal("a context read kept a row without an instant".into())
            })?;
            if side < last {
                return Err(ServerError::Internal(
                    "a context read's kept rows are out of group order".into(),
                ));
            }
            last = side;
            counts[side as usize] += 1;
        }
        Ok(counts)
    }
}

impl Selection for Around {
    /// Keep the `before` rows nearest before the focus instant, the rows
    /// at it up to [`CONTEXT_SAME_TIME_ROWS`] and the `after` rows nearest
    /// after it, in that order and each ascending by `_time`.
    ///
    /// The capture's order is `_time DESC, _ingested DESC`; reversed it is
    /// ascending in both, and a stable sort on the instant alone keeps it
    /// so whatever the engine's text order of equal instants.
    ///
    /// A result with no rows selects nothing, whatever its columns: when no
    /// file matches the read's source and the hot buffer holds nothing, the
    /// engine answers no columns at all.
    fn before_cut(&mut self, result: &mut QueryResult, _: &PhaseClock) -> Result<(), ServerError> {
        if result.rows.is_empty() {
            return Ok(());
        }
        let time = Self::time_column(result)?;
        let mut rows: Vec<(DateTime<Utc>, Side, Vec<Value>)> = std::mem::take(&mut result.rows)
            .into_iter()
            .rev()
            .filter_map(|row| {
                let (at, side) = self.side(&row, time)?;
                Some((at, side, row))
            })
            .collect();
        rows.sort_by_key(|(at, _, _)| *at);

        let same_cap = usize::try_from(CONTEXT_SAME_TIME_ROWS).unwrap_or(usize::MAX);
        let earlier = rows
            .iter()
            .filter(|(_, side, _)| *side == Side::Earlier)
            .count();
        // Ascending: the nearest earlier rows are the last ones, the
        // nearest later rows the first ones.
        let skip_earlier = earlier.saturating_sub(self.before);
        let mut seen_earlier = 0;
        let mut seen_same = 0;
        let mut seen_later = 0;
        result.rows = rows
            .into_iter()
            .filter(|(_, side, _)| match side {
                Side::Earlier => {
                    seen_earlier += 1;
                    seen_earlier > skip_earlier
                }
                Side::Same => {
                    seen_same += 1;
                    seen_same <= same_cap
                }
                Side::Later => {
                    seen_later += 1;
                    seen_later <= self.after
                }
            })
            .map(|(_, _, row)| row)
            .collect();
        Ok(())
    }

    /// The groups as ranges of the kept rows, which keep the order above:
    /// the cut keeps a prefix of it. No kept rows are three empty groups.
    fn after_cut(
        &mut self,
        kept: &QueryResult,
        _: &CancelLatch,
    ) -> Result<Option<Correlation>, ServerError> {
        let [earlier, same, later] = if kept.rows.is_empty() {
            [0; 3]
        } else {
            self.counts(kept)?
        };
        let mut facts = self.facts.clone();
        facts.groups = ContextGroups {
            earlier: RowRange {
                start: 0,
                end: earlier,
            },
            same_time: RowRange {
                start: earlier,
                end: earlier + same,
            },
            later: RowRange {
                start: earlier + same,
                end: earlier + same + later,
            },
        };
        Ok(Some(Correlation::Context(facts)))
    }
}

#[cfg(test)]
mod tests {
    use trawl_api::value::Column;

    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn around(before: usize, after: usize) -> Around {
        Around {
            focus: at("2024-01-16T10:00:10Z"),
            before,
            after,
            facts: ContextFacts {
                focus: FocusRow {
                    result_id: "0".repeat(32),
                    row: 0,
                    time: "2024-01-16T10:00:10.000000Z".into(),
                    env: None,
                    host: None,
                },
                scope_used: ScopeUsed {
                    env: None,
                    host: None,
                    seconds: 30,
                },
                groups: ContextGroups {
                    earlier: RowRange { start: 0, end: 0 },
                    same_time: RowRange { start: 0, end: 0 },
                    later: RowRange { start: 0, end: 0 },
                },
                statement: CONTEXT_STATEMENT.into(),
            },
        }
    }

    /// Rows as the capture orders them, newest first: `(second, seq)`.
    fn descending(rows: &[(u32, u32)]) -> QueryResult {
        QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("seq", ColumnType::Integer),
            ],
            rows: rows
                .iter()
                .map(|&(second, seq)| {
                    vec![
                        Value::String(format!("2024-01-16T10:00:{second:02}.000000Z")),
                        Value::Integer(i64::from(seq)),
                    ]
                })
                .collect(),
        }
    }

    fn seqs(result: &QueryResult) -> Vec<i64> {
        result
            .rows
            .iter()
            .map(|row| match row[1] {
                Value::Integer(seq) => seq,
                _ => unreachable!(),
            })
            .collect()
    }

    fn select(selection: &mut Around, result: &mut QueryResult) -> ContextGroups {
        let clock = PhaseClock::new();
        selection.before_cut(result, &clock).unwrap();
        match selection.after_cut(result, &CancelLatch::never()).unwrap() {
            Some(Correlation::Context(facts)) => facts.groups,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_nearest_rows_either_side_and_every_equal_row_are_kept_in_order() {
        // Seconds 5..=15, newest first; two rows at the focus second 10.
        let mut result = descending(&[
            (15, 15),
            (14, 14),
            (13, 13),
            (12, 12),
            (11, 11),
            (10, 101),
            (10, 100),
            (9, 9),
            (8, 8),
            (7, 7),
            (6, 6),
            (5, 5),
        ]);
        let groups = select(&mut around(2, 3), &mut result);
        assert_eq!(seqs(&result), vec![8, 9, 100, 101, 11, 12, 13]);
        assert_eq!(groups.earlier, RowRange { start: 0, end: 2 });
        assert_eq!(groups.same_time, RowRange { start: 2, end: 4 });
        assert_eq!(groups.later, RowRange { start: 4, end: 7 });
    }

    #[test]
    fn the_same_time_group_stops_at_its_cap() {
        let rows: Vec<(u32, u32)> = (0..25).map(|seq| (10, seq)).collect();
        let mut result = descending(&rows);
        let groups = select(&mut around(10, 10), &mut result);
        let cap = usize::try_from(CONTEXT_SAME_TIME_ROWS).unwrap();
        assert_eq!(result.rows.len(), cap);
        assert_eq!(groups.same_time, RowRange { start: 0, end: 20 });
        assert_eq!(groups.earlier, RowRange { start: 0, end: 0 });
        assert_eq!(groups.later, RowRange { start: 20, end: 20 });
    }

    #[test]
    fn a_cut_prefix_still_names_its_groups() {
        let mut result = descending(&[(12, 12), (11, 11), (10, 10), (9, 9), (8, 8)]);
        let mut selection = around(10, 10);
        selection
            .before_cut(&mut result, &PhaseClock::new())
            .unwrap();
        result.rows.truncate(3);
        let Some(Correlation::Context(facts)) =
            selection.after_cut(&result, &CancelLatch::never()).unwrap()
        else {
            panic!("context facts");
        };
        assert_eq!(facts.groups.earlier, RowRange { start: 0, end: 2 });
        assert_eq!(facts.groups.same_time, RowRange { start: 2, end: 3 });
        assert_eq!(facts.groups.later, RowRange { start: 3, end: 3 });
    }

    #[test]
    fn a_read_with_no_columns_selects_three_empty_groups() {
        // The engine's answer when no file matches the source and the hot
        // buffer holds nothing: no columns and no rows.
        let mut result = QueryResult {
            columns: vec![],
            rows: vec![],
        };
        let groups = select(&mut around(10, 10), &mut result);
        assert!(result.rows.is_empty());
        assert_eq!(groups.earlier, RowRange { start: 0, end: 0 });
        assert_eq!(groups.same_time, RowRange { start: 0, end: 0 });
        assert_eq!(groups.later, RowRange { start: 0, end: 0 });
    }

    #[test]
    fn rows_without_a_time_column_stay_an_internal_error() {
        let mut result = QueryResult {
            columns: vec![Column::new("seq", ColumnType::Integer)],
            rows: vec![vec![Value::Integer(1)]],
        };
        let mut selection = around(10, 10);
        let refused = selection.before_cut(&mut result, &PhaseClock::new());
        assert!(
            matches!(refused, Err(ServerError::Internal(_))),
            "{refused:?}"
        );
        let refused = selection.after_cut(&result, &CancelLatch::never());
        assert!(
            matches!(refused, Err(ServerError::Internal(_))),
            "{refused:?}"
        );
    }

    #[test]
    fn the_window_holds_the_focus_instant_and_is_measured_as_a_capture() {
        let window = window(at("2024-01-16T10:00:10.250000Z"), 30).unwrap();
        assert_eq!(
            window.earliest.as_deref(),
            Some("2024-01-16T09:59:40.250000Z")
        );
        assert_eq!(
            window.latest.as_deref(),
            Some("2024-01-16T10:00:40.250001Z")
        );
        assert!(window.last.is_none());
        assert!(super::window(at("2024-01-16T10:00:10Z"), u64::MAX).is_err());
    }
}
