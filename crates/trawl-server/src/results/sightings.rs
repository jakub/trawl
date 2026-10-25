// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `sightings` read (ADR-0058): where one literal appears, counted
//! exactly over every match, with the most recent matches captured.
//!
//! The statement is trawl-core's [`correlate::sightings_query`]: the
//! literal as one quoted text search over `message` and `_raw`, then
//! `eventstats` writing each (`service`, `env`) group's match count, first
//! and last `_time` onto every match. It has no head and no sort of its
//! own. The capture imposes the event shape ([`Shape::Events`]), so the
//! lane orders the matches newest first and cuts them like any search of
//! events, while [`bounds::classify`](super::bounds::classify) would call
//! the `eventstats` stage whole. The window functions see every match
//! before the cut does.
//!
//! # Two populations
//!
//! The selection reads the groups from the UNCUT rows, before the cut:
//! `matches_total` is every match in the bounds, and each group's count,
//! first and last seen are exact over all of them, a group whose matches
//! are all older than the cut included. It reorders and drops nothing.
//! After the cut it attributes the literal to fields over the KEPT rows,
//! under the text search's own containment rule
//! ([`correlate::contains_ci`]). Both denominators are named in the facts:
//! field counts are over `captured`, group counts over `matches_total`.
//!
//! The eligible columns are every column except `_raw` and the three
//! `sighting_*` columns, compared under the DSL's field fold; `fields`
//! naming one of those is refused. Attribution runs over the request's
//! `fields` among them, or, without them, over all of them. `fields`
//! restricts the per-field list only: the search, its matches, the groups
//! and `raw_only_rows` are the same with or without it. A cell is
//! attributed when its displayed text holds the literal: text is itself,
//! an integer its decimal digits, a boolean `true` or `false`, a float
//! Rust's shortest round-trip `Display` (`12.5`, `0.1`, `3` for 3.0,
//! `100000000000000000000` for 1e20, never an exponent), and a timestamp
//! the text the engine already wrote. A null or a list holds nothing.
//! The list keeps the [`ATTRIBUTION_MAX_FIELDS`] fields holding it in the
//! most kept matches, ties by name, and counts the rest as
//! `fields_omitted`; at most [`MAX_FIELDS`] `fields` never reach the cut.
//! `raw_only_rows` counts the kept matches in which no eligible column
//! holds it, whatever `fields` says: matched through `_raw` alone. The
//! facts carry the validated `fields` as `attribution_fields`, so a later
//! page states the same restriction.
//!
//! The `sighting_*` columns overwrite user fields of the same names
//! before the matches are captured; those names are the statement's.
//!
//! # Bounds on the answer
//!
//! A read over more matches than `max_result_rows` is refused by the
//! engine's row cap, `result_too_large`, before anything is captured. Its
//! message names the row limit but not the fix, which is to narrow the
//! window or the literal. The facts document is stored beside the file and
//! sent with every answer about the capture, so the groups are capped too:
//! more than [`CORRELATION_MAX_GROUPS`] distinct (`env`, `service`) groups
//! is refused with `result_too_large`. Each group is at most a 32-byte env,
//! a 128-byte service and two timestamps. The field list is at most
//! [`ATTRIBUTION_MAX_FIELDS`] names whatever the catalog holds, `fields` at
//! most [`MAX_FIELDS`] and the follow-ups at most two queries; at the caps
//! the worst-case facts document is about 1.6 MB. The capture bounds the
//! encoded document whatever its names ([`service::CORRELATION_MAX_BYTES`])
//! and counts it in the capture's bytes.
//!
//! Every refusal of the request itself (the literal's caps and NUL, the
//! `fields` cap, lengths and names, the window) comes before
//! [`service::run`], so it takes nothing, and none quotes more than
//! [`ERROR_EXCERPT_BYTES`] of what the caller sent.

use std::collections::BTreeMap;

use fleet_auth::VerifiedKey;
use trawl_api::results::{
    CaptureKind, Correlation, CorrelationResponse, FieldSightings, SightingGroup,
    SightingsDenominators, SightingsFacts, SightingsRequest,
};
use trawl_api::value::{QueryResult, Value};
use trawl_core::correlate::{
    self, MAX_LITERAL_BYTES, SIGHTING_COUNT, SIGHTING_FIRST, SIGHTING_LAST,
};
use trawl_core::parser::suggest::quote_dsl_field;
use trawl_core::sanitize::{ERROR_EXCERPT_BYTES, excerpt};
use trawl_core::schema::{MAX_FIELD_NAME_BYTES, RAW, catalog_key};
use trawl_engine::cancel::CancelLatch;
use trawl_engine::timing::PhaseClock;

use super::bounds::{Shape, WindowInput};
use super::service::{self, CORRELATION_MAX_GROUPS, CaptureOverflow, Selection, Spec};
use crate::error::ServerError;
use crate::state::AppState;

/// The most fields a `sightings` request restricts attribution to.
pub const MAX_FIELDS: usize = 16;

/// The most attributed fields the facts list; the rest are counted in
/// `fields_omitted` (module docs, "Bounds on the answer").
pub const ATTRIBUTION_MAX_FIELDS: usize = 256;

/// The window a `sightings` read covers when the request names none.
const DEFAULT_WINDOW: &str = "24h";

/// Count where a literal appears (module docs).
///
/// # Errors
/// 403 without the `query` permission; 400 for the literal, `fields` or
/// the window; then every refusal a capture makes, and `result_too_large`
/// over the group cap.
pub async fn sightings(
    state: &AppState,
    verified: &VerifiedKey,
    request_id: String,
    req: SightingsRequest,
) -> Result<CorrelationResponse, ServerError> {
    service::require_query(verified)?;
    check_literal(&req.literal)?;
    let fields = check_fields(req.fields)?;
    let composed = correlate::sightings_query(&req.literal)?;
    let window = req
        .window
        .map(|window| WindowInput {
            earliest: window.earliest,
            latest: window.latest,
            last: window.last,
        })
        .filter(|window| {
            window.earliest.is_some() || window.latest.is_some() || window.last.is_some()
        })
        .unwrap_or_else(|| WindowInput {
            earliest: None,
            latest: None,
            last: Some(DEFAULT_WINDOW.to_owned()),
        });
    let spec = Spec {
        kind: CaptureKind::Sightings,
        query: composed.text().to_owned(),
        window: Some(window),
        shape: Some(Shape::Events),
        selection: Box::new(Sightings {
            literal: req.literal,
            fields,
            matches_total: 0,
            groups: Vec::new(),
        }),
    };
    let captured = service::run(state, verified, request_id, spec).await?;
    Ok(CorrelationResponse {
        result: captured.result,
        execution: captured.execution,
    })
}

/// The literal's caps, each named with the value sent.
fn check_literal(literal: &str) -> Result<(), ServerError> {
    if literal.is_empty() {
        return Err(ServerError::BadRequest(
            "the literal is empty; give the text to look for".into(),
        ));
    }
    if literal.contains('\0') {
        return Err(ServerError::BadRequest(
            "the literal contains a NUL character".into(),
        ));
    }
    if literal.len() > MAX_LITERAL_BYTES {
        return Err(ServerError::BadRequest(format!(
            "the literal is {} bytes; at most {MAX_LITERAL_BYTES} are accepted",
            literal.len()
        )));
    }
    Ok(())
}

/// The columns attribution never counts, under any case: `_raw`, which
/// the search reads and raw-only stands for, and the statement's own
/// `sighting_*` aggregates.
const RESERVED: [&str; 4] = [RAW, SIGHTING_COUNT, SIGHTING_FIRST, SIGHTING_LAST];

/// Whether `name` folds to a [`RESERVED`] column.
fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&catalog_key(name).as_str())
}

/// The `fields` cap and names: at most [`MAX_FIELDS`], each at most
/// [`MAX_FIELD_NAME_BYTES`] and a name the DSL can write, none
/// [`RESERVED`], none twice under the field fold. Kept in request order.
/// A name over the length limit is refused by its length alone; the
/// others quote at most [`ERROR_EXCERPT_BYTES`] of it.
fn check_fields(fields: Vec<String>) -> Result<Vec<String>, ServerError> {
    if fields.len() > MAX_FIELDS {
        return Err(ServerError::BadRequest(format!(
            "{} fields given; attribution takes at most {MAX_FIELDS}",
            fields.len()
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    for name in &fields {
        if name.len() > MAX_FIELD_NAME_BYTES {
            return Err(ServerError::BadRequest(format!(
                "a field name is {} bytes; at most {MAX_FIELD_NAME_BYTES} are accepted",
                name.len()
            )));
        }
        let quoted = excerpt(name, ERROR_EXCERPT_BYTES);
        if quote_dsl_field(name).is_none() {
            return Err(ServerError::BadRequest(format!(
                "field {quoted} is not a field name"
            )));
        }
        if is_reserved(name) {
            return Err(ServerError::BadRequest(format!(
                "field {quoted} is reserved: attribution never counts `_raw` \
                 or the `sighting_count`, `sighting_first` and \
                 `sighting_last` columns the read writes"
            )));
        }
        if !seen.insert(catalog_key(name)) {
            return Err(ServerError::BadRequest(format!(
                "field {quoted} is given twice"
            )));
        }
    }
    Ok(fields)
}

/// The sightings read's selection (module docs, "Two populations").
struct Sightings {
    literal: String,
    /// The validated `fields`; empty attributes to every column.
    fields: Vec<String>,
    /// Every match, counted before the cut.
    matches_total: u64,
    /// The groups, read before the cut, ranked.
    groups: Vec<SightingGroup>,
}

/// The position of the first column named `name`.
fn position(result: &QueryResult, name: &str) -> Option<usize> {
    result.columns.iter().position(|column| column.name == name)
}

/// A group key cell: its text, or none for a null or a non-text cell.
fn text(row: &[Value], at: Option<usize>) -> Option<String> {
    match at.map(|at| &row[at]) {
        Some(Value::String(text)) => Some(text.clone()),
        _ => None,
    }
}

/// A count the statement wrote.
fn count(row: &[Value], at: usize) -> Result<u64, ServerError> {
    match &row[at] {
        Value::Integer(n) => u64::try_from(*n).map_err(|_| counted_wrong()),
        Value::UInt(n) => Ok(*n),
        _ => Err(counted_wrong()),
    }
}

fn counted_wrong() -> ServerError {
    ServerError::Internal("a sightings read's statement wrote a group without its counts".into())
}

/// Text after any value: a group without an env or a service ranks after
/// the ones with one.
fn null_last(value: Option<&String>) -> (bool, Option<&String>) {
    (value.is_none(), value)
}

/// The cell's text the literal is looked for in: a non-null scalar's
/// displayed text ([`Value`]'s `Display`), none for a null or a list
/// (module docs).
fn haystack(value: &Value) -> Option<std::borrow::Cow<'_, str>> {
    match value {
        Value::String(text) => Some(std::borrow::Cow::Borrowed(text)),
        Value::Integer(_) | Value::UInt(_) | Value::Float(_) | Value::Boolean(_) => {
            Some(std::borrow::Cow::Owned(value.to_string()))
        }
        Value::Null | Value::Array(_) => None,
    }
}

impl Selection for Sightings {
    /// Count every match and read each (`env`, `service`) group once.
    fn before_cut(&mut self, result: &mut QueryResult, _: &PhaseClock) -> Result<(), ServerError> {
        self.matches_total = u64::try_from(result.rows.len()).unwrap_or(u64::MAX);
        if result.rows.is_empty() {
            return Ok(());
        }
        let missing =
            || ServerError::Internal("a sightings read's result lacks its group columns".into());
        let count_at = position(result, SIGHTING_COUNT).ok_or_else(missing)?;
        let first_at = position(result, SIGHTING_FIRST).ok_or_else(missing)?;
        let last_at = position(result, SIGHTING_LAST).ok_or_else(missing)?;
        let env_at = position(result, "env");
        let service_at = position(result, "service");

        let mut groups: BTreeMap<(Option<String>, Option<String>), SightingGroup> = BTreeMap::new();
        for row in &result.rows {
            let key = (text(row, env_at), text(row, service_at));
            if groups.contains_key(&key) {
                continue;
            }
            let (Value::String(first), Value::String(last)) = (&row[first_at], &row[last_at])
            else {
                return Err(counted_wrong());
            };
            let group = SightingGroup {
                env: key.0.clone(),
                service: key.1.clone(),
                count: count(row, count_at)?,
                first_seen: first.clone(),
                last_seen: last.clone(),
            };
            groups.insert(key, group);
            let found = u64::try_from(groups.len()).unwrap_or(u64::MAX);
            if found > CORRELATION_MAX_GROUPS {
                return Err(ServerError::CaptureTooLarge(CaptureOverflow::Groups {
                    read: "sightings",
                    groups: found,
                    max_groups: CORRELATION_MAX_GROUPS,
                    hint: "narrow the window or the literal",
                }));
            }
        }
        let mut groups: Vec<SightingGroup> = groups.into_values().collect();
        groups.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| null_last(a.service.as_ref()).cmp(&null_last(b.service.as_ref())))
                .then_with(|| null_last(a.env.as_ref()).cmp(&null_last(b.env.as_ref())))
        });
        self.groups = groups;
        Ok(())
    }

    /// Attribute the literal to fields over the kept matches.
    fn after_cut(
        &mut self,
        kept: &QueryResult,
        _: &CancelLatch,
    ) -> Result<Option<Correlation>, ServerError> {
        let restricted: Vec<String> = self.fields.iter().map(|f| catalog_key(f)).collect();
        // Every eligible column, whatever `fields` says: raw-only is over
        // all of them. The exclusion comes before the membership test.
        let eligible: Vec<(usize, bool)> = kept
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| !is_reserved(&column.name))
            .map(|(at, column)| {
                let attributed =
                    restricted.is_empty() || restricted.contains(&catalog_key(&column.name));
                (at, attributed)
            })
            .collect();

        let mut per_field = vec![0_u64; eligible.len()];
        let mut raw_only_rows: u64 = 0;
        for row in &kept.rows {
            let mut anywhere = false;
            for (slot, &(at, _)) in eligible.iter().enumerate() {
                if haystack(&row[at])
                    .is_some_and(|text| correlate::contains_ci(&text, &self.literal))
                {
                    per_field[slot] += 1;
                    anywhere = true;
                }
            }
            if !anywhere {
                raw_only_rows += 1;
            }
        }
        let mut fields: Vec<FieldSightings> = eligible
            .iter()
            .zip(per_field)
            .filter(|((_, attributed), rows)| *attributed && *rows > 0)
            .map(|(&(at, _), rows)| FieldSightings {
                field: kept.columns[at].name.clone(),
                rows,
            })
            .collect();
        fields.sort_by(|a, b| b.rows.cmp(&a.rows).then_with(|| a.field.cmp(&b.field)));
        let fields_omitted = fields.len().saturating_sub(ATTRIBUTION_MAX_FIELDS);
        fields.truncate(ATTRIBUTION_MAX_FIELDS);

        let mut per_service: BTreeMap<&str, u64> = BTreeMap::new();
        for group in &self.groups {
            if let Some(service) = group.service.as_deref() {
                *per_service.entry(service).or_default() += group.count;
            }
        }
        // The first of the busiest by name, from the ordered map.
        let busiest = per_service
            .iter()
            .fold(None::<(&str, u64)>, |best, (&service, &count)| match best {
                Some((_, top)) if top >= count => best,
                _ => Some((service, count)),
            })
            .map(|(service, _)| service);
        let follow_ups = correlate::follow_ups(&self.literal, busiest)
            .iter()
            .map(|composed| composed.text().to_owned())
            .collect();

        let captured = u64::try_from(kept.rows.len()).unwrap_or(u64::MAX);
        Ok(Some(Correlation::Sightings(SightingsFacts {
            literal: self.literal.clone(),
            matches_total: self.matches_total,
            captured,
            attribution_fields: self.fields.clone(),
            groups: std::mem::take(&mut self.groups),
            fields,
            fields_omitted: u64::try_from(fields_omitted).unwrap_or(u64::MAX),
            raw_only_rows,
            denominators: SightingsDenominators {
                field_counts_over: captured,
                service_counts_over: self.matches_total,
            },
            follow_ups,
        })))
    }
}

#[cfg(test)]
mod tests {
    use trawl_api::value::{Column, ColumnType};

    use super::*;

    fn result(rows: &[(&str, &str, &str, &str, i64, &str, &str)]) -> QueryResult {
        QueryResult {
            columns: vec![
                Column::new("_raw", ColumnType::String),
                Column::new("env", ColumnType::String),
                Column::new("service", ColumnType::String),
                Column::new("message", ColumnType::String),
                Column::new(SIGHTING_COUNT, ColumnType::Integer),
                Column::new(SIGHTING_FIRST, ColumnType::Timestamp),
                Column::new(SIGHTING_LAST, ColumnType::Timestamp),
            ],
            rows: rows
                .iter()
                .map(|&(raw, env, service, message, count, first, last)| {
                    vec![
                        Value::String(raw.into()),
                        Value::String(env.into()),
                        Value::String(service.into()),
                        Value::String(message.into()),
                        Value::Integer(count),
                        Value::String(first.into()),
                        Value::String(last.into()),
                    ]
                })
                .collect(),
        }
    }

    fn selection(fields: &[&str]) -> Sightings {
        Sightings {
            literal: "10.0.4.17".into(),
            fields: fields.iter().map(|&f| f.to_owned()).collect(),
            matches_total: 0,
            groups: Vec::new(),
        }
    }

    fn facts(selection: &mut Sightings, uncut: &mut QueryResult, kept: usize) -> SightingsFacts {
        selection.before_cut(uncut, &PhaseClock::new()).unwrap();
        uncut.rows.truncate(kept);
        match selection.after_cut(uncut, &CancelLatch::never()).unwrap() {
            Some(Correlation::Sightings(facts)) => facts,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn groups_come_from_every_match_and_fields_from_the_kept_ones() {
        let mut uncut = result(&[
            (
                "from 10.0.4.17",
                "prod",
                "api",
                "from 10.0.4.17",
                2,
                "t1",
                "t3",
            ),
            ("ip=10.0.4.17", "prod", "api", "login", 2, "t1", "t3"),
            ("10.0.4.17", "prod", "db", "x", 1, "t0", "t0"),
        ]);
        let facts = facts(&mut selection(&[]), &mut uncut, 2);
        assert_eq!(facts.matches_total, 3);
        assert_eq!(facts.captured, 2);
        assert_eq!(facts.groups.len(), 2);
        assert_eq!(facts.groups[0].service.as_deref(), Some("api"));
        assert_eq!(facts.groups[0].count, 2);
        assert_eq!(facts.groups[1].service.as_deref(), Some("db"));
        assert_eq!(
            facts.fields,
            vec![FieldSightings {
                field: "message".into(),
                rows: 1
            }]
        );
        assert_eq!(facts.raw_only_rows, 1);
        assert_eq!(facts.denominators.field_counts_over, 2);
        assert_eq!(facts.denominators.service_counts_over, 3);
        assert!(facts.attribution_fields.is_empty());
        assert_eq!(facts.follow_ups.len(), 2);
        assert!(
            facts.follow_ups[0].contains("service=api"),
            "{:?}",
            facts.follow_ups
        );
    }

    #[test]
    fn fields_restrict_attribution_and_nothing_else() {
        let rows = [
            (
                "from 10.0.4.17",
                "prod",
                "api",
                "from 10.0.4.17",
                2,
                "t1",
                "t3",
            ),
            ("ip=10.0.4.17", "prod", "api", "login", 2, "t1", "t3"),
        ];
        let open = facts(&mut selection(&[]), &mut result(&rows), 2);
        let restricted = facts(&mut selection(&["Service"]), &mut result(&rows), 2);
        assert_eq!(open.matches_total, restricted.matches_total);
        assert_eq!(open.captured, restricted.captured);
        assert_eq!(open.groups, restricted.groups);
        assert!(restricted.fields.is_empty());
        assert_eq!(
            restricted.raw_only_rows, open.raw_only_rows,
            "raw-only is over every field: the message match is not raw-only"
        );
        assert_eq!(restricted.raw_only_rows, 1);
        assert_eq!(restricted.attribution_fields, vec!["Service".to_owned()]);
    }

    /// A reserved name that got past `check_fields` still attributes
    /// nothing: the exclusion comes before the membership test.
    #[test]
    fn reserved_columns_are_never_attributed_even_when_named() {
        let rows = [("ip=10.0.4.17", "prod", "api", "login", 1, "10.0.4.17", "t")];
        for name in [
            "_raw",
            "_RAW",
            SIGHTING_COUNT,
            SIGHTING_FIRST,
            "Sighting_Last",
        ] {
            let facts = facts(&mut selection(&[name]), &mut result(&rows), 1);
            assert!(facts.fields.is_empty(), "{name}: {:?}", facts.fields);
            assert_eq!(facts.raw_only_rows, 1, "{name}");
        }
    }

    /// A non-null scalar's haystack is its displayed text.
    #[test]
    fn every_scalar_is_looked_for_in_its_displayed_text() {
        let found = |value: Value| haystack(&value).map(std::borrow::Cow::into_owned);
        assert_eq!(found(Value::String("a b".into())).as_deref(), Some("a b"));
        assert_eq!(found(Value::Integer(-42)).as_deref(), Some("-42"));
        assert_eq!(
            found(Value::UInt(u64::MAX)).as_deref(),
            Some("18446744073709551615")
        );
        assert_eq!(found(Value::Boolean(true)).as_deref(), Some("true"));
        assert_eq!(found(Value::Boolean(false)).as_deref(), Some("false"));
        assert_eq!(found(Value::Float(12.5)).as_deref(), Some("12.5"));
        assert_eq!(found(Value::Float(0.1)).as_deref(), Some("0.1"));
        assert_eq!(found(Value::Float(3.0)).as_deref(), Some("3"));
        assert_eq!(
            found(Value::Float(1e20)).as_deref(),
            Some("100000000000000000000")
        );
        assert_eq!(found(Value::Null), None);
        assert_eq!(found(Value::Array(vec![Value::Integer(1)])), None);
    }

    /// The widest attribution a valid ingest can make: 50 matches, each
    /// with `message` and 160 of 8,000 distinct 255-byte fields holding
    /// the literal, so every one of the 8,001 eligible columns attributes
    /// and `fields` is omitted. The list keeps the 256 busiest, `message`
    /// first (50 rows), then 255 one-row fields by name; the other 7,745
    /// are counted, and the document fits the bound beside the widest
    /// [`CORRELATION_MAX_GROUPS`] groups.
    #[test]
    fn a_wide_read_lists_the_busiest_fields_and_fits_the_bound() {
        use trawl_core::schema::MAX_FIELD_NAME_BYTES;

        let names: Vec<String> = (0..8000)
            .map(|n| format!("f{n:0>width$}", width = MAX_FIELD_NAME_BYTES - 1))
            .collect();
        let mut columns = vec![
            Column::new("_raw", ColumnType::String),
            Column::new("env", ColumnType::String),
            Column::new("service", ColumnType::String),
            Column::new("host", ColumnType::String),
            Column::new("message", ColumnType::String),
        ];
        columns.extend(names.iter().map(|n| Column::new(n, ColumnType::String)));
        columns.push(Column::new(SIGHTING_COUNT, ColumnType::Integer));
        columns.push(Column::new(SIGHTING_FIRST, ColumnType::Timestamp));
        columns.push(Column::new(SIGHTING_LAST, ColumnType::Timestamp));
        let literal = Value::String("needle".into());
        let rows = (0..50)
            .map(|n| {
                let mut row = vec![
                    literal.clone(),
                    Value::String("prod".into()),
                    Value::String("api".into()),
                    Value::String("h".into()),
                    literal.clone(),
                ];
                row.extend((0..8000).map(|f| {
                    if f / 160 == n {
                        literal.clone()
                    } else {
                        Value::Null
                    }
                }));
                row.extend([
                    Value::Integer(50),
                    Value::String("t0".into()),
                    Value::String("t1".into()),
                ]);
                row
            })
            .collect();
        let mut uncut = QueryResult { columns, rows };
        let mut wide = Sightings {
            literal: "needle".into(),
            fields: Vec::new(),
            matches_total: 0,
            groups: Vec::new(),
        };
        let mut facts = facts(&mut wide, &mut uncut, 50);

        assert_eq!(facts.fields.len(), ATTRIBUTION_MAX_FIELDS);
        assert_eq!(
            facts.fields[0],
            FieldSightings {
                field: "message".into(),
                rows: 50
            }
        );
        let listed: Vec<&str> = facts.fields[1..].iter().map(|f| f.field.as_str()).collect();
        let first: Vec<&str> = names[..ATTRIBUTION_MAX_FIELDS - 1]
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(listed, first, "one-row fields by name");
        assert_eq!(facts.fields_omitted, 7745);
        assert_eq!(facts.raw_only_rows, 0);

        facts.groups = (0..CORRELATION_MAX_GROUPS)
            .map(|n| SightingGroup {
                env: Some(format!("{n:e>32}")),
                service: Some(format!(
                    "{n:s>width$}",
                    width = crate::ingest::pipeline::MAX_SERVICE_NAME_LEN
                )),
                count: u64::MAX,
                first_seen: "2026-01-03T00:05:00.000000Z".into(),
                last_seen: "2026-01-03T00:05:59.999999Z".into(),
            })
            .collect();
        let document = service::encode_facts(Correlation::Sightings(facts))
            .expect("the widest attribution fits beside the widest groups");
        assert!(
            document.bytes() <= service::CORRELATION_MAX_BYTES,
            "{}",
            document.bytes()
        );
    }

    #[test]
    fn more_groups_than_the_cap_are_refused() {
        let services: Vec<String> = (0..=CORRELATION_MAX_GROUPS)
            .map(|n| format!("s{n}"))
            .collect();
        let rows: Vec<_> = services
            .iter()
            .map(|s| ("10.0.4.17", "prod", s.as_str(), "m", 1, "t", "t"))
            .collect();
        let mut uncut = result(&rows);
        let refused = selection(&[]).before_cut(&mut uncut, &PhaseClock::new());
        let Err(ServerError::CaptureTooLarge(overflow)) = refused else {
            panic!("{refused:?}");
        };
        assert!(
            overflow
                .to_string()
                .contains("narrow the window or the literal"),
            "{overflow}"
        );
    }

    #[test]
    fn the_caps_name_the_value_sent() {
        assert!(check_literal("").is_err());
        assert!(check_literal("a\0b").is_err());
        let long = "a".repeat(MAX_LITERAL_BYTES + 1);
        let message = check_literal(&long).unwrap_err().to_string();
        assert!(
            message.contains("1025") && message.contains("1024"),
            "{message}"
        );
        assert!(check_literal(&"a".repeat(MAX_LITERAL_BYTES)).is_ok());

        let seventeen: Vec<String> = (0..=MAX_FIELDS).map(|n| format!("f{n}")).collect();
        let message = check_fields(seventeen).unwrap_err().to_string();
        assert!(
            message.contains("17") && message.contains("16"),
            "{message}"
        );
        assert!(check_fields(vec!["Host".into(), "host".into()]).is_err());
        assert!(check_fields(vec![String::new()]).is_err());

        let widest = "f".repeat(MAX_FIELD_NAME_BYTES);
        let message = check_fields(vec![widest.clone(), widest.to_uppercase()])
            .unwrap_err()
            .to_string();
        assert!(
            message.contains(&format!(
                "field {:?}… (255 bytes) is given twice",
                "F".repeat(64)
            )),
            "{message}"
        );
        let message = check_fields(vec!["f".repeat(MAX_FIELD_NAME_BYTES + 1)])
            .unwrap_err()
            .to_string();
        assert!(
            message.ends_with("a field name is 256 bytes; at most 255 are accepted"),
            "{message}"
        );
    }

    #[test]
    fn reserved_names_are_refused_under_the_fold() {
        for name in [
            "_raw",
            "_Raw",
            "sighting_count",
            "SIGHTING_COUNT",
            "sighting_first",
            "sighting_last",
        ] {
            let message = check_fields(vec!["host".into(), name.into()])
                .unwrap_err()
                .to_string();
            assert!(message.contains(name), "{message}");
            assert!(message.contains("reserved"), "{message}");
        }
    }
}
