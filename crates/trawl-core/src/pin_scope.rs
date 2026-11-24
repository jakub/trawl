// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The compile-time pin scope walk (ADR-0011).

use crate::ast::{AggExpr, Expr, LiteralValue, PipeStage, Spanned};
use crate::schema::{CanonicalType, FieldTypes, catalog_key};

#[derive(Debug, Clone, Copy)]
pub enum PinnedSubject<'e> {
    Field(&'e str),
    Call(&'e Spanned<Expr>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinScope {
    pins: FieldTypes,
}

impl PinScope {
    #[must_use]
    pub fn root(pins: &FieldTypes) -> Self {
        Self { pins: pins.clone() }
    }

    #[must_use]
    pub fn unpinned() -> Self {
        Self::default()
    }

    pub fn fill_unpinned<'a>(
        &mut self,
        names: impl IntoIterator<Item = &'a str>,
        ty: CanonicalType,
    ) {
        for name in names {
            if self.pin_for(name).is_none() {
                self.pins.insert(&catalog_key(name), ty);
            }
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    #[must_use]
    pub fn pin_for(&self, dsl_name: &str) -> Option<CanonicalType> {
        self.pins.pin_for(dsl_name)
    }

    #[must_use]
    pub fn subject_pin<'e>(
        &self,
        expr: &'e Spanned<Expr>,
    ) -> Option<(PinnedSubject<'e>, CanonicalType)> {
        match &expr.node {
            Expr::FieldRef(name) => self
                .pin_for(name)
                .map(|pin| (PinnedSubject::Field(name.as_str()), pin)),
            Expr::FunctionCall { name, args } => {
                let pin = crate::emitter::function_result_pin(name)?;
                let (first, rest) = args.split_first()?;
                if !matches!(first.node, Expr::FieldRef(_)) {
                    return None;
                }
                if !rest
                    .iter()
                    .all(|a| matches!(a.node, Expr::Literal(LiteralValue::String(_))))
                {
                    return None;
                }
                Some((PinnedSubject::Call(expr), pin))
            }
            _ => None,
        }
    }

    /// The match is exhaustive on purpose — a new stage variant must state
    /// its scope rule here or fail to compile, never silently keep a wrong
    /// pin.
    pub fn advance(&mut self, stage: &PipeStage) {
        match stage {
            PipeStage::Where(_)
            | PipeStage::Sort(_)
            | PipeStage::Limit(_)
            | PipeStage::Tail(_)
            | PipeStage::Sample(_)
            | PipeStage::Dedup(_) => {}
            PipeStage::Table(t) => self.restrict_to_names(t.fields.iter().map(String::as_str)),
            PipeStage::Drop(d) => {
                for field in &d.fields {
                    self.pins.remove(&catalog_key(field));
                }
            }
            PipeStage::Rename(r) => {
                // Parallel, like `let`: every source resolves against the
                // pre-stage scope, because the SQL aliases every source
                // off the pre-stage row.
                let resolved: Vec<(String, Option<CanonicalType>)> = r
                    .renames
                    .iter()
                    .map(|(old, new)| (catalog_key(new), self.pin_for(old)))
                    .collect();
                for (old, _) in &r.renames {
                    self.pins.remove(&catalog_key(old));
                }
                for (target, pin) in resolved {
                    match pin {
                        Some(pin) => self.pins.insert(&target, pin),
                        None => self.pins.remove(&target),
                    }
                }
            }
            PipeStage::Let(l) => {
                // Two passes so every source pin resolves against the
                // pre-stage scope.
                let resolved: Vec<(String, Option<CanonicalType>)> = l
                    .assignments
                    .iter()
                    .map(|(target, expr)| {
                        let pin = match &expr.node {
                            Expr::FieldRef(source) => self.pin_for(source),
                            Expr::FunctionCall { name, .. } => {
                                crate::emitter::function_result_pin(name)
                            }
                            _ => None,
                        };
                        (catalog_key(target), pin)
                    })
                    .collect();
                for (target, pin) in resolved {
                    match pin {
                        Some(pin) => self.pins.insert(&target, pin),
                        None => self.pins.remove(&target),
                    }
                }
            }
            PipeStage::Stats(s) => {
                self.restrict_to_names(s.group_by.iter().map(String::as_str));
                self.remove_agg_outputs(&s.aggregations);
            }
            PipeStage::Timechart(t) => {
                // The `_time` output is the computed bucket, not the
                // stored column — killed by the restriction along with
                // the aggregate aliases.
                self.restrict_to_names(t.group_by.iter().map(String::as_str));
                self.remove_agg_outputs(&t.aggregations);
            }
            PipeStage::Top(t) => {
                self.restrict_to_names(
                    std::iter::once(t.field.as_str()).chain(t.by.iter().map(String::as_str)),
                );
                self.pins.remove("count");
            }
            PipeStage::Rare(r) => {
                self.restrict_to_names(
                    std::iter::once(r.field.as_str()).chain(r.by.iter().map(String::as_str)),
                );
                self.pins.remove("count");
            }
            PipeStage::Pivot(p) => {
                self.restrict_to_names(p.by.iter().map(String::as_str));
            }
            PipeStage::EventStats(es) => self.remove_agg_outputs(&es.aggregations),
            PipeStage::Extract(e) => match &e.mode {
                crate::ast::ExtractMode::Regex(pattern) => {
                    // The emitter rejects a body that does not parse;
                    // nothing to scrub here for one.
                    if let Ok(captures) = crate::pattern::price::captures(pattern) {
                        for capture in captures {
                            self.pins.remove(&catalog_key(&capture.name));
                        }
                    }
                }
                crate::ast::ExtractMode::KeyValue { .. } => {}
            },
            PipeStage::FromSaved(_) => self.pins = FieldTypes::new(),
        }
    }

    /// [`FieldTypes`] iterates a `BTreeMap`, so the list comes out in key
    /// order and the wire order is stable.
    fn severity_columns(&self) -> Vec<String> {
        self.pins
            .iter()
            .filter(|&(field, ty)| {
                ty == CanonicalType::Severity && field != crate::schema::SEVERITY
            })
            .map(|(field, _)| field.to_owned())
            .collect()
    }

    fn restrict_to_names<'a>(&mut self, names: impl Iterator<Item = &'a str>) {
        let keep: Vec<String> = names.map(catalog_key).collect();
        self.pins.restrict_to(&keep);
    }

    fn remove_agg_outputs(&mut self, aggregations: &[AggExpr]) {
        for agg in aggregations {
            let name = crate::projection::agg_output_name(agg);
            self.pins.remove(&catalog_key(&name));
        }
    }
}

#[must_use]
pub fn severity_output_columns(pipeline: &[Spanned<PipeStage>], root: &PinScope) -> Vec<String> {
    let mut scope = root.clone();
    for stage in pipeline {
        scope.advance(&stage.node);
    }
    scope.severity_columns()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{FromSavedStage, SavedRunSelector};
    use crate::parser;
    use crate::schema::CanonicalType as CT;

    fn pins(entries: &[(&str, CT)]) -> FieldTypes {
        let mut ft = FieldTypes::new();
        for (field, ty) in entries {
            ft.insert(field, *ty);
        }
        ft
    }

    fn walk(dsl: &str, root: &[(&str, CT)]) -> PinScope {
        let query = parser::parse(dsl).expect("dsl parses");
        let mut scope = PinScope::root(&pins(root));
        for stage in &query.pipeline {
            scope.advance(&stage.node);
        }
        scope
    }

    const ROOT: &[(&str, CT)] = &[
        ("status", CT::Varchar),
        ("dur", CT::BigInt),
        ("service", CT::Varchar),
        ("_time", CT::Timestamp),
    ];

    #[test]
    fn selection_and_ordering_stages_pass_the_scope_through() {
        for dsl in [
            "* | sort status",
            "* | limit 5",
            "* | head 5",
            "* | tail 5",
            "* | dedup host",
            "* | sample 10%",
            "* | where status > 400",
        ] {
            let scope = walk(dsl, ROOT);
            assert_eq!(scope.pin_for("status"), Some(CT::Varchar), "{dsl}");
            assert_eq!(scope.pin_for("dur"), Some(CT::BigInt), "{dsl}");
        }
    }

    #[test]
    fn rename_remaps_the_pin_to_the_new_name() {
        let scope = walk("* | rename status as st", ROOT);
        assert_eq!(scope.pin_for("st"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn rename_of_unpinned_source_scrubs_the_target_pin() {
        let scope = walk("* | rename unpinned as status", ROOT);
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn rename_onto_pinned_target_takes_the_source_pin() {
        let scope = walk("* | rename dur as status", ROOT);
        assert_eq!(scope.pin_for("status"), Some(CT::BigInt));
        assert_eq!(scope.pin_for("dur"), None);
    }

    #[test]
    fn rename_chain_resolves_against_the_pre_stage_scope() {
        let scope = walk("* | rename status as dur, dur as d2", ROOT);
        assert_eq!(scope.pin_for("dur"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("d2"), Some(CT::BigInt));
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn rename_swap_exchanges_the_pins() {
        let scope = walk("* | rename status as dur, dur as status", ROOT);
        assert_eq!(scope.pin_for("status"), Some(CT::BigInt));
        assert_eq!(scope.pin_for("dur"), Some(CT::Varchar));
    }

    #[test]
    fn rename_onto_one_target_is_refused_before_any_scope_walk() {
        assert!(parser::parse("* | rename status as x, dur as x").is_err());
        assert!(parser::parse("* | rename status as X, dur as x").is_err());

        let scope = walk("* | rename status as x, dur as y", ROOT);
        assert_eq!(scope.pin_for("x"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("y"), Some(CT::BigInt));
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn rename_folds_case_like_the_catalog() {
        let scope = walk("* | rename Status as St", ROOT);
        assert_eq!(scope.pin_for("st"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("ST"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn let_bare_alias_copies_the_pin() {
        let scope = walk("* | let s2 = status", ROOT);
        assert_eq!(scope.pin_for("s2"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
    }

    #[test]
    fn let_computed_expression_kills_the_target_pin() {
        let scope = walk("* | let status = dur * 2", ROOT);
        assert_eq!(scope.pin_for("status"), None);
        assert_eq!(scope.pin_for("dur"), Some(CT::BigInt));
    }

    #[test]
    fn let_function_wrapped_kills_the_target_pin() {
        let scope = walk("* | let status = lower(status)", ROOT);
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn let_sibling_reference_resolves_against_the_pre_stage_scope() {
        let scope = walk("* | let a = status, b = a", ROOT);
        assert_eq!(scope.pin_for("a"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("b"), None);
    }

    #[test]
    fn let_overwrite_and_alias_resolve_in_parallel() {
        let scope = walk("* | let status = 1, x = status", ROOT);
        assert_eq!(scope.pin_for("status"), None);
        assert_eq!(scope.pin_for("x"), Some(CT::Varchar));
    }

    #[test]
    fn let_alias_copies_the_named_columns_pin() {
        let scope = walk("* | let t = _time", ROOT);
        assert_eq!(scope.pin_for("t"), Some(CT::Timestamp));
        let scope = walk("* | let t = timestamp", ROOT);
        assert_eq!(scope.pin_for("t"), None);
    }

    #[test]
    fn table_restricts_the_scope() {
        let scope = walk("* | table status, host", ROOT);
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("dur"), None);
        assert_eq!(scope.pin_for("service"), None);
    }

    #[test]
    fn fields_alias_restricts_like_table() {
        let scope = walk("* | fields dur", ROOT);
        assert_eq!(scope.pin_for("dur"), Some(CT::BigInt));
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn drop_removes_named_pins() {
        let scope = walk("* | drop status", ROOT);
        assert_eq!(scope.pin_for("status"), None);
        assert_eq!(scope.pin_for("dur"), Some(CT::BigInt));
    }

    #[test]
    fn stats_keeps_group_by_keys_and_kills_everything_else() {
        let scope = walk("* | stats count() by status", ROOT);
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("dur"), None);
        assert_eq!(scope.pin_for("count"), None);
    }

    #[test]
    fn stats_alias_matching_a_pinned_name_is_killed() {
        let scope = walk("* | stats avg(x) as dur by status", ROOT);
        assert_eq!(scope.pin_for("dur"), None);
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
    }

    #[test]
    fn timechart_kills_the_time_bucket_and_keeps_by_keys() {
        let scope = walk("* | timechart span=5m count() by service", ROOT);
        assert_eq!(scope.pin_for("service"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("_time"), None);
        assert_eq!(scope.pin_for("timestamp"), None);
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn top_and_rare_keep_field_and_by_keys() {
        for dsl in [
            "* | top 5 status by service",
            "* | rare 5 status by service",
        ] {
            let scope = walk(dsl, ROOT);
            assert_eq!(scope.pin_for("status"), Some(CT::Varchar), "{dsl}");
            assert_eq!(scope.pin_for("service"), Some(CT::Varchar), "{dsl}");
            assert_eq!(scope.pin_for("dur"), None, "{dsl}");
            assert_eq!(scope.pin_for("count"), None, "{dsl}");
        }
    }

    #[test]
    fn top_over_a_pinned_field_named_count_still_kills_count() {
        let scope = walk(
            "* | top 5 status",
            &[("status", CT::Varchar), ("count", CT::BigInt)],
        );
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("count"), None);
    }

    #[test]
    fn pivot_keeps_by_keys_only() {
        let scope = walk("* | pivot count() on status by service", ROOT);
        assert_eq!(scope.pin_for("service"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("status"), None);
        assert_eq!(scope.pin_for("dur"), None);
    }

    #[test]
    fn eventstats_keeps_inputs_and_kills_aggregate_outputs() {
        let scope = walk(
            "* | eventstats avg(dur) as avg_dur by service",
            &[
                ("dur", CT::BigInt),
                ("service", CT::Varchar),
                ("avg_dur", CT::Double),
            ],
        );
        assert_eq!(scope.pin_for("dur"), Some(CT::BigInt));
        assert_eq!(scope.pin_for("service"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("avg_dur"), None);
    }

    #[test]
    fn eventstats_alias_kills_the_alias() {
        let scope = walk("* | eventstats avg(x) as dur", &[("dur", CT::BigInt)]);
        assert_eq!(scope.pin_for("dur"), None);
    }

    #[test]
    fn extract_regex_kills_capture_group_names() {
        let scope = walk(r#"* | extract "(?P<status>\d+)" from message"#, ROOT);
        assert_eq!(scope.pin_for("status"), None);
        assert_eq!(scope.pin_for("dur"), Some(CT::BigInt));
    }

    #[test]
    fn extract_kv_passes_the_scope_through() {
        let scope = walk("* | extract kv", ROOT);
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("dur"), Some(CT::BigInt));
    }

    #[test]
    fn from_saved_clears_the_scope() {
        let mut scope = PinScope::root(&pins(ROOT));
        scope.advance(&PipeStage::FromSaved(FromSavedStage {
            name: "daily".into(),
            run: SavedRunSelector::Latest,
        }));
        assert!(scope.is_empty());
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn rename_then_where_carries_the_pin_under_the_new_name() {
        let query = parser::parse("* | rename status as st | where st > 400").expect("parses");
        let mut scope = PinScope::root(&pins(ROOT));
        scope.advance(&query.pipeline[0].node);
        assert_eq!(scope.pin_for("st"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("status"), None);
    }

    #[test]
    fn let_adopts_a_declared_function_result_pin() {
        let scope = walk("* | let s = sev(level)", ROOT);
        assert_eq!(scope.pin_for("s"), Some(CT::Severity));
        let scope = walk(r#"* | let s = sev(level, "syslog")"#, &[]);
        assert_eq!(scope.pin_for("s"), Some(CT::Severity));
        let scope = walk("* | let s = lower(level)", ROOT);
        assert_eq!(scope.pin_for("s"), None);
    }

    #[test]
    fn a_declared_pin_survives_as_a_group_by_key() {
        let scope = walk("* | let s = sev(level) | stats count() by s", ROOT);
        assert_eq!(scope.pin_for("s"), Some(CT::Severity));
        assert_eq!(scope.pin_for("count"), None);
        let scope = walk("* | let s = sev(level) | stats max(s) as s", ROOT);
        assert_eq!(scope.pin_for("s"), None);
        let scope = walk("* | let s = sev(level) | rename s as sv", ROOT);
        assert_eq!(scope.pin_for("sv"), Some(CT::Severity));
        assert_eq!(scope.pin_for("s"), None);
    }

    #[test]
    fn subject_pin_admits_a_pinned_field_and_a_declaring_call() {
        let scope = PinScope::root(&pins(ROOT));
        let subject = |dsl: &str| {
            let query = parser::parse(dsl).expect("parses");
            let PipeStage::Where(w) = &query.pipeline[0].node else {
                panic!("{dsl} needs a where stage")
            };
            let Expr::Binary { lhs, .. } = &w.condition.node else {
                panic!("{dsl} needs a binary condition")
            };
            let expr: &crate::ast::Spanned<Expr> = lhs;
            scope
                .subject_pin(expr)
                .map(|(subject, pin)| (matches!(subject, PinnedSubject::Call(_)), pin))
        };
        assert_eq!(subject("* | where status == 1"), Some((false, CT::Varchar)));
        assert_eq!(subject("* | where unpinned == 1"), None);
        assert_eq!(
            subject("* | where sev(level) == 1"),
            Some((true, CT::Severity))
        );
        assert_eq!(
            subject(r#"* | where sev(level, "syslog") == 1"#),
            Some((true, CT::Severity))
        );
        assert_eq!(subject("* | where sev(lower(level)) == 1"), None);
        assert_eq!(subject("* | where sev(level, other) == 1"), None);
        assert_eq!(subject("* | where lower(status) == 1"), None);
        assert_eq!(subject("* | where status + 1 == 1"), None);
    }

    #[test]
    fn severity_output_columns_names_declared_columns_only() {
        let names = |dsl: &str, root: &[(&str, CT)]| {
            let query = parser::parse(dsl).expect("dsl parses");
            severity_output_columns(&query.pipeline, &PinScope::root(&pins(root)))
        };
        assert_eq!(names("* | let s = sev(level)", &[]), ["s"]);
        assert_eq!(
            names("* | let s = sev(level) | stats count() by s", &[]),
            ["s"]
        );
        assert!(names("* | let s = sev(level) | table message", &[]).is_empty());
        assert!(names("* | let s = sev(level) | stats count()", &[]).is_empty());
        assert!(names("* | let s = lower(level)", &[]).is_empty());
        assert!(names("*", SEVERITY_ROOT).is_empty());
        assert_eq!(names("* | rename _severity as sv", SEVERITY_ROOT), ["sv"]);
        assert_eq!(names("* | let sv = _severity", SEVERITY_ROOT), ["sv"]);
    }

    const SEVERITY_ROOT: &[(&str, CT)] = &[("_severity", CT::Severity), ("message", CT::Varchar)];

    #[test]
    fn fill_unpinned_pins_a_name_the_scope_leaves_open() {
        let mut scope = PinScope::root(&pins(ROOT));
        scope.fill_unpinned(["bucket"], CT::Timestamp);
        assert_eq!(scope.pin_for("bucket"), Some(CT::Timestamp));
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
    }

    #[test]
    fn fill_unpinned_keeps_an_existing_pin() {
        let mut scope = PinScope::root(&pins(ROOT));
        scope.fill_unpinned(["status", "dur"], CT::Timestamp);
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("dur"), Some(CT::BigInt));
        assert_eq!(scope, PinScope::root(&pins(ROOT)));
    }

    #[test]
    fn fill_unpinned_folds_a_mixed_case_name() {
        let mut scope = PinScope::unpinned();
        scope.fill_unpinned(["Time"], CT::Timestamp);
        assert_eq!(scope.pin_for("Time"), Some(CT::Timestamp));
        assert_eq!(scope.pin_for("time"), Some(CT::Timestamp));
        let mut scope = PinScope::root(&pins(&[("time", CT::Varchar)]));
        scope.fill_unpinned(["TIME"], CT::Timestamp);
        assert_eq!(scope.pin_for("Time"), Some(CT::Varchar));
    }

    #[test]
    fn fill_unpinned_with_no_names_leaves_the_scope_equal() {
        let mut scope = PinScope::root(&pins(ROOT));
        scope.fill_unpinned(std::iter::empty(), CT::Timestamp);
        assert_eq!(scope, PinScope::root(&pins(ROOT)));
    }

    #[test]
    fn fill_unpinned_on_a_clone_leaves_the_source_alone() {
        let source = PinScope::root(&pins(ROOT));
        let mut filled = source.clone();
        filled.fill_unpinned(["bucket"], CT::Timestamp);
        assert_eq!(filled.pin_for("bucket"), Some(CT::Timestamp));
        assert_eq!(source.pin_for("bucket"), None);
        assert_eq!(source, PinScope::root(&pins(ROOT)));
    }

    #[test]
    fn unpinned_scope_stays_empty_and_cheap() {
        let mut scope = PinScope::unpinned();
        assert!(scope.is_empty());
        let query = parser::parse("* | rename a as b | stats count() by b").expect("parses");
        for stage in &query.pipeline {
            scope.advance(&stage.node);
        }
        assert!(scope.is_empty());
    }
}
