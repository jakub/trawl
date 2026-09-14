// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Safe actions and ordering for displayed query results.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
use std::{cmp::Ordering, collections::HashSet};
use trawl_api::value::Value;
use trawl_core::ast::PipeStage;

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
                            .map(trawl_core::projection::agg_output_name),
                    );
                    out.groups = Some(
                        s.group_by
                            .into_iter()
                            .filter(|f| {
                                !out.changed.contains(f)
                                    && out.groups.as_ref().is_none_or(|g| g.contains(f))
                            })
                            .collect(),
                    );
                }
                PipeStage::Timechart(s) => {
                    out.raw = false;
                    out.changed.insert("_time".into());
                    out.changed.extend(
                        s.aggregations
                            .iter()
                            .map(trawl_core::projection::agg_output_name),
                    );
                    out.groups = Some(
                        s.group_by
                            .into_iter()
                            .filter(|f| {
                                !out.changed.contains(f)
                                    && out.groups.as_ref().is_none_or(|g| g.contains(f))
                            })
                            .collect(),
                    );
                }
                PipeStage::Let(s) => out
                    .changed
                    .extend(s.assignments.into_iter().map(|(f, _)| f)),
                PipeStage::Rename(s) => {
                    for (a, b) in s.renames {
                        out.changed.insert(a);
                        out.changed.insert(b);
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
    pub fn include(&self, field: &str, value: &Value) -> bool {
        !self.unknown
            && !self.changed.contains(field)
            && self.groups.as_ref().is_none_or(|g| g.contains(field))
            && !matches!(value, Value::Null | Value::Array(_))
    }
    pub fn raw_actions(&self) -> bool {
        self.raw && !self.unknown && self.changed.is_empty()
    }
}

/// Missing cells and null sort first. Numbers compare by value, preserving
/// integer precision even when the other operand is a floating-point value.
pub(crate) fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    use Value::{Array, Boolean, Float, Integer, Null, String};
    let a = a.unwrap_or(&Null);
    let b = b.unwrap_or(&Null);
    match (a, b) {
        (Null, Null) => Ordering::Equal,
        (Null, _) => Ordering::Less,
        (_, Null) => Ordering::Greater,
        (Integer(a), Integer(b)) => a.cmp(b),
        (Float(a), Float(b)) => a.partial_cmp(b).unwrap_or_else(|| a.total_cmp(b)),
        (Integer(a), Float(b)) => int_float(*a, *b),
        (Float(a), Integer(b)) => int_float(*b, *a).reverse(),
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
        Value::Integer(_) | Value::Float(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
    }
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
#[cfg(test)]
mod tests {
    use super::*;
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
}
