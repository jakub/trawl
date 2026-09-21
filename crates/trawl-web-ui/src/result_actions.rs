// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Safe actions and ordering for displayed query results.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
use std::{cmp::Ordering, collections::HashSet};
use trawl_api::value::Value;
use trawl_core::{ast::PipeStage, schema::catalog_key};

#[derive(Clone, Debug, Default)]
pub(crate) struct Capabilities {
    pub raw: bool,
    groups: Option<HashSet<String>>,
    changed: HashSet<String>,
    unknown: bool,
}
impl Capabilities {
    pub fn for_query(query: &str) -> Self {
        let Ok(parsed) = trawl_core::parser::parse(query) else {
            return Self {
                unknown: true,
                ..Self::default()
            };
        };
        let mut out = Self {
            raw: true,
            ..Self::default()
        };
        for stage in parsed.pipeline {
            match stage.node {
                PipeStage::Stats(s) => {
                    out.raw = false;
                    out.changed.extend(
                        s.aggregations
                            .iter()
                            .map(|a| catalog_key(&trawl_core::projection::agg_output_name(a))),
                    );
                    out.groups = Some(
                        s.group_by
                            .into_iter()
                            .map(|f| catalog_key(&f))
                            .filter(|f| {
                                !out.changed.contains(f)
                                    && out.groups.as_ref().is_none_or(|g| g.contains(f))
                            })
                            .collect(),
                    );
                }
                PipeStage::Timechart(s) => {
                    out.raw = false;
                    out.changed.insert(catalog_key("_time"));
                    out.changed.extend(
                        s.aggregations
                            .iter()
                            .map(|a| catalog_key(&trawl_core::projection::agg_output_name(a))),
                    );
                    out.groups = Some(
                        s.group_by
                            .into_iter()
                            .map(|f| catalog_key(&f))
                            .filter(|f| {
                                !out.changed.contains(f)
                                    && out.groups.as_ref().is_none_or(|g| g.contains(f))
                            })
                            .collect(),
                    );
                }
                PipeStage::Let(s) => out
                    .changed
                    .extend(s.assignments.into_iter().map(|(f, _)| catalog_key(&f))),
                PipeStage::Rename(s) => {
                    for (a, b) in s.renames {
                        out.changed.insert(catalog_key(&a));
                        out.changed.insert(catalog_key(&b));
                    }
                }
                PipeStage::Where(_)
                | PipeStage::Sort(_)
                | PipeStage::Limit(_)
                | PipeStage::Table(_)
                | PipeStage::Drop(_)
                | PipeStage::Dedup(_)
                | PipeStage::Tail(_)
                | PipeStage::Sample(_) => (),
                // Extraction can overwrite unknown fields; saved input and other
                // shaping stages do not prove input-field provenance.
                _ => {
                    out.unknown = true;
                    out.raw = false;
                }
            }
        }
        out
    }
    /// Whether this output field still names an unchanged input field.
    pub fn input_field(&self, field: &str) -> bool {
        let field = catalog_key(field);
        !self.unknown
            && !self.changed.contains(&field)
            && self.groups.as_ref().is_none_or(|g| g.contains(&field))
    }
    pub fn include(&self, field: &str, value: &Value) -> bool {
        self.input_field(field) && !matches!(value, Value::Null | Value::Array(_))
    }
    /// Facets summarize original fields of raw rows. Aggregations and
    /// unknown sources do not offer facet groups, even for grouping keys.
    pub fn raw_facets(&self) -> bool {
        self.raw && !self.unknown
    }
    pub fn raw_actions(&self) -> bool {
        self.raw && !self.unknown && self.changed.is_empty()
    }
}

/// Missing cells and null sort first. Numbers compare by value, preserving
/// integer precision even when the other operand is a floating-point value.
pub(crate) fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    use Value::{Array, Boolean, Float, Integer, Null, String, UInt};
    let a = a.unwrap_or(&Null);
    let b = b.unwrap_or(&Null);
    match (a, b) {
        (Null, Null) => Ordering::Equal,
        (Null, _) => Ordering::Less,
        (_, Null) => Ordering::Greater,
        (Integer(a), Integer(b)) => a.cmp(b),
        (UInt(a), UInt(b)) => a.cmp(b),
        // Every integer is a point on one line, so the two integer
        // variants compare as the numbers they are, not by variant rank:
        // `i128` is the narrowest type holding both ranges.
        (Integer(a), UInt(b)) => i128::from(*a).cmp(&i128::from(*b)),
        (UInt(a), Integer(b)) => i128::from(*a).cmp(&i128::from(*b)),
        (Float(a), Float(b)) => a.partial_cmp(b).unwrap_or_else(|| a.total_cmp(b)),
        (Integer(a), Float(b)) => int_float(*a, *b),
        (Float(a), Integer(b)) => int_float(*b, *a).reverse(),
        (UInt(a), Float(b)) => uint_float(*a, *b),
        (Float(a), UInt(b)) => uint_float(*b, *a).reverse(),
        (Boolean(a), Boolean(b)) => a.cmp(b),
        (String(a), String(b)) => a.cmp(b),
        (Array(a), Array(b)) => a
            .iter()
            .zip(b)
            .map(|(a, b)| compare(Some(a), Some(b)))
            .find(|o| *o != Ordering::Equal)
            .unwrap_or_else(|| a.len().cmp(&b.len())),
        _ => rank(a).cmp(&rank(b)),
    }
}
fn rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Integer(_) | Value::UInt(_) | Value::Float(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
    }
}
/// The row indices of one locally-paged window, sorted WHOLE first.
///
/// Sorting happens over every row the response carried, and only then is
/// the page cut from it. The other order — page, then sort — would make
/// page 2 the second 50 rows of the response re-ordered among
/// themselves, which is not the second 50 rows of the sorted result.
///
/// The underlying sort is stable, so the same arguments answer the same
/// list every time. That determinism is the contract: the exact table
/// and the bars drawn beside it each call this for themselves, and they
/// describe the same groups only because both calls agree.
pub fn sorted_page(
    rows: &[Vec<Value>],
    sort: Option<(usize, bool)>,
    page: usize,
    size: usize,
) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..rows.len()).collect();
    if let Some((col, asc)) = sort {
        indices.sort_by(|&a, &b| {
            let ord = compare(rows[a].get(col), rows[b].get(col));
            if asc { ord } else { ord.reverse() }
        });
    }
    let start = page.saturating_mul(size);
    if start >= indices.len() {
        return Vec::new();
    }
    let end = start.saturating_add(size).min(indices.len());
    indices[start..end].to_vec()
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn int_float(i: i64, f: f64) -> Ordering {
    if f.is_nan() {
        return if f.is_sign_negative() {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    if f >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    if f < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let whole = f as i64;
    i.cmp(&whole)
        .then_with(|| (whole as f64).partial_cmp(&f).unwrap_or(Ordering::Equal))
}
/// The unsigned twin of [`int_float`], for the range above `i64::MAX`.
///
/// Same shape, same NaN placement, same truncate-then-refine tie-break;
/// only the bounds move, because a `u64` cannot be negative and reaches
/// twice as far up.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn uint_float(u: u64, f: f64) -> Ordering {
    if f.is_nan() {
        return if f.is_sign_negative() {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    if f >= 18_446_744_073_709_551_616.0 {
        return Ordering::Less;
    }
    if f < 0.0 {
        return Ordering::Greater;
    }
    let whole = f as u64;
    u.cmp(&whole)
        .then_with(|| (whole as f64).partial_cmp(&f).unwrap_or(Ordering::Equal))
}
/// The results table sorts an oversized unsigned by value, not by text.
///
/// This is why the variant exists. Spelled as a string, `10000000000000000000`
/// sorts before `9223372036854775808` — the shorter number last — and a
/// column of ids reads as shuffled. Spelled as a number, both compare on
/// one line with every other integer.
#[cfg(test)]
#[test]
fn uint_sorts_numerically() {
    let lo = Value::UInt(9_223_372_036_854_775_808);
    let hi = Value::UInt(10_000_000_000_000_000_000);
    assert_eq!(compare(Some(&lo), Some(&hi)), Ordering::Less);
    assert_eq!(compare(Some(&hi), Some(&lo)), Ordering::Greater);
    assert_eq!(compare(Some(&hi), Some(&hi)), Ordering::Equal);

    // Across the variant boundary: one number line, not two ranks.
    assert_eq!(
        compare(Some(&Value::Integer(5)), Some(&Value::UInt(u64::MAX))),
        Ordering::Less
    );
    assert_eq!(
        compare(Some(&Value::UInt(u64::MAX)), Some(&Value::Integer(5))),
        Ordering::Greater
    );
    assert_eq!(
        compare(Some(&Value::Integer(-1)), Some(&lo)),
        Ordering::Less
    );

    // …and against floats, the way the signed variant already does.
    assert_eq!(
        compare(Some(&lo), Some(&Value::Float(1.0))),
        Ordering::Greater
    );
    assert_eq!(
        compare(
            Some(&Value::Float(f64::INFINITY)),
            Some(&Value::UInt(u64::MAX))
        ),
        Ordering::Greater
    );
    assert_eq!(
        compare(Some(&Value::Float(-1.0)), Some(&lo)),
        Ordering::Less
    );

    // Null still sorts first, whatever the number's width.
    assert_eq!(compare(None, Some(&hi)), Ordering::Less);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_mutations_keep_only_unchanged_field_facets() {
        for (query, changed) in [
            ("* | let status = 0", vec!["status"]),
            ("* | rename message as summary", vec!["message", "summary"]),
        ] {
            assert!(trawl_core::parser::parse(query).is_ok());
            let capability = Capabilities::for_query(query);
            assert!(capability.raw_facets(), "{query}");
            for field in ["host", "service"] {
                assert!(capability.input_field(field), "{query}: {field}");
                assert!(capability.include(field, &Value::String("original".into())));
            }
            for field in changed {
                assert!(!capability.input_field(field), "{query}: {field}");
                assert!(!capability.include(field, &Value::String("changed".into())));
            }
        }
        for query in [
            "* | stats count() by host",
            "| from saved example",
            "* | extract kv",
        ] {
            assert!(trawl_core::parser::parse(query).is_ok(), "{query}");
            assert!(!Capabilities::for_query(query).raw_facets(), "{query}");
        }
    }

    #[test]
    fn provenance_uses_ascii_catalog_identity() {
        for query in [
            "* | stats count() by HOST",
            "* | stats count() by HOST | stats sum(count) by HoSt",
            "* | timechart count() by HOST",
            "* | stats count() by HOST | timechart sum(count) by HoSt",
        ] {
            assert!(trawl_core::parser::parse(query).is_ok(), "{query}");
            let capability = Capabilities::for_query(query);
            for field in ["host", "HOST", "HoSt"] {
                assert!(capability.input_field(field), "{query}: {field}");
            }
            assert!(!capability.input_field("COUNT"), "{query}");
        }
        for query in [
            "* | let HOST = lower(host)",
            "* | rename HOST as Other",
            "* | rename other as HOST",
            "* | let HOST = lower(host) | stats count() by host",
            "* | stats count() by HOST | let host = lower(host)",
            "* | stats count() by HOST | rename host as Other",
            "* | stats count() as HOST by host",
            "* | timechart count() as HOST by host",
            "* | stats count() as HOST by service | stats count() by host",
            "* | stats count() as HOST by service | timechart count() by host",
        ] {
            assert!(trawl_core::parser::parse(query).is_ok(), "{query}");
            let capability = Capabilities::for_query(query);
            for field in ["host", "HOST", "HoSt"] {
                assert!(!capability.input_field(field), "{query}: {field}");
            }
        }
        let renamed = Capabilities::for_query("* | rename HOST as Other");
        assert!(!renamed.input_field("OTHER"));
        assert!(!Capabilities::for_query("* | timechart count()").input_field("_TIME"));
    }

    #[test]
    fn provenance_preserves_non_ascii_distinctions() {
        for query in [
            "* | stats count() by `CAFÉ`",
            "* | stats count() by `CAFÉ` | stats sum(count) by `cafÉ`",
        ] {
            assert!(trawl_core::parser::parse(query).is_ok(), "{query}");
            let capability = Capabilities::for_query(query);
            assert!(capability.input_field("cafÉ"), "{query}");
            assert!(!capability.input_field("café"), "{query}");
        }
        for query in ["* | let `CAFÉ` = 0", "* | rename source as `CAFÉ`"] {
            assert!(trawl_core::parser::parse(query).is_ok(), "{query}");
            let capability = Capabilities::for_query(query);
            assert!(!capability.input_field("cafÉ"), "{query}");
            assert!(capability.input_field("café"), "{query}");
        }
    }

    #[test]
    fn mixed_numbers_define_a_consistent_order_at_precision_boundaries() {
        let ordered = [
            Value::Float(-f64::NAN),
            Value::Float(f64::NEG_INFINITY),
            Value::Integer(i64::MIN),
            Value::Float(-9_007_199_254_740_992.0),
            Value::Integer(-9_007_199_254_740_991),
            Value::Float(-1.5),
            Value::Integer(-1),
            Value::Float(-0.0),
            Value::Integer(0),
            Value::Float(0.0),
            Value::Float(0.5),
            Value::Float(9_007_199_254_740_992.0),
            Value::Integer(9_007_199_254_740_993),
            Value::Integer(i64::MAX),
            Value::Float(9_223_372_036_854_775_808.0),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NAN),
        ];
        for (i, a) in ordered.iter().enumerate() {
            for b in &ordered[i..] {
                let order = compare(Some(a), Some(b));
                assert_ne!(order, Ordering::Greater, "{a:?} > {b:?}");
                assert_eq!(order.reverse(), compare(Some(b), Some(a)));
            }
        }
    }

    #[test]
    fn numeric_extremes() {
        for (a, b) in [
            (Value::Integer(10), Value::Integer(100)),
            (
                Value::Integer(9_007_199_254_740_993),
                Value::Float(9_007_199_254_740_992.0),
            ),
            (
                Value::Integer(i64::MAX),
                Value::Float(9_223_372_036_854_775_808.0),
            ),
            (Value::Float(-1.5), Value::Integer(-1)),
        ] {
            let expected = if matches!(a, Value::Integer(9_007_199_254_740_993)) {
                Ordering::Greater
            } else {
                Ordering::Less
            };
            assert_eq!(compare(Some(&a), Some(&b)), expected);
            assert_eq!(compare(Some(&b), Some(&a)), expected.reverse());
        }
        assert_eq!(compare(None, Some(&Value::Null)), Ordering::Equal);
        assert_eq!(
            compare(
                Some(&Value::Integer(i64::MIN)),
                Some(&Value::Float(-9_223_372_036_854_775_808.0))
            ),
            Ordering::Equal
        );
    }
    #[test]
    fn saved_and_reaggregated_metrics_never_gain_input_provenance() {
        for q in [
            "| from saved example | stats count() by host",
            "* | stats count() by host | stats count() by count",
        ] {
            assert!(trawl_core::parser::parse(q).is_ok(), "{q}");
            let c = Capabilities::for_query(q);
            assert!(!c.include("count", &Value::Integer(1)));
            assert!(!c.raw_actions());
            if q.contains("from saved") {
                assert!(!c.include("host", &Value::String("x".into())));
            }
        }
    }

    #[test]
    fn only_original_groups_allow_include() {
        let v = Value::String("x".into());
        assert!(Capabilities::for_query("* | stats count() by host").include("host", &v));
        for q in [
            "* | stats count() by host",
            "* | let host = lower(host) | stats count() by host",
            "* | stats count() by host | let host = lower(host)",
            "* | stats count() by host | rename host as other",
        ] {
            let c = Capabilities::for_query(q);
            assert!(!c.raw_actions());
            assert!(!c.include("count", &v));
            if q.contains("let") || q.contains("rename") {
                assert!(!c.include("host", &v));
            }
        }
        assert!(!Capabilities::for_query("nonsense | bad").include("host", &v));
        assert!(
            !Capabilities::for_query("* | stats count() by host").include("host", &Value::Null)
        );
    }

    /// 59 rows whose sort column counts DOWN, so ascending order is the
    /// exact reverse of response order and a page cut before the sort
    /// would be visibly wrong.
    fn descending_rows() -> Vec<Vec<Value>> {
        (0..59)
            .map(|i| vec![Value::Integer(59 - i), Value::String(format!("r{i}"))])
            .collect()
    }

    #[test]
    fn an_unsorted_page_is_the_response_order() {
        let rows = descending_rows();
        assert_eq!(sorted_page(&rows, None, 0, 50), (0..50).collect::<Vec<_>>());
        assert_eq!(
            sorted_page(&rows, None, 1, 50),
            (50..59).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_sorted_page_holds_the_rows_that_follow_the_sort() {
        let rows = descending_rows();
        // Ascending on the counting-down column: the whole result in
        // reverse, so page 1 is the first nine rows of the response,
        // largest last.
        let ascending: Vec<usize> = (0..59).rev().collect();
        assert_eq!(sorted_page(&rows, Some((0, true)), 0, 50), ascending[..50]);
        assert_eq!(sorted_page(&rows, Some((0, true)), 1, 50), ascending[50..]);
        // Not the unsorted page 1 re-ordered among itself, which is what
        // sorting a slice would have produced.
        assert_ne!(
            sorted_page(&rows, Some((0, true)), 1, 50),
            vec![58, 57, 56, 55, 54, 53, 52, 51, 50]
        );
    }

    #[test]
    fn a_page_past_the_end_is_empty() {
        let rows = descending_rows();
        for page in [2, 9, usize::MAX] {
            assert!(sorted_page(&rows, None, page, 50).is_empty(), "page {page}");
            assert!(
                sorted_page(&rows, Some((0, false)), page, 50).is_empty(),
                "page {page}"
            );
        }
    }

    /// What lets the exact table and the bars agree without sharing a
    /// computation: two calls, one answer.
    #[test]
    fn the_same_call_answers_the_same_page_twice() {
        // A column with ties, so a comparator that left equal rows to
        // chance would show it here.
        let rows: Vec<Vec<Value>> = (0..59)
            .map(|i| vec![Value::Integer(i % 3), Value::String(format!("r{i}"))])
            .collect();
        for sort in [None, Some((0, true)), Some((0, false))] {
            for page in [0, 1] {
                assert_eq!(
                    sorted_page(&rows, sort, page, 50),
                    sorted_page(&rows, sort, page, 50),
                    "{sort:?} page {page}"
                );
            }
        }
    }
}
