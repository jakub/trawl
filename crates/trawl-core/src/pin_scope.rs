// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The compile-time pin scope walk (ADR-0011 slice A′).
//!
//! Which catalog pin applies to a field reference depends on WHERE in the
//! pipeline the reference sits: `rename status as st | where st>400` must
//! stay pin-aware under the new name, `let status=<expr> | where
//! status>400` must NOT apply the original pin to a derived value, and
//! `stats count() by status | where status=…` keeps the group-by key's
//! pin. [`PinScope`] carries the catalog snapshot through the pipeline,
//! advancing per stage; the SQL emitter and the stream compiler consume
//! the SAME walk, so which pin applies at each stage is identical in both
//! lanes by construction.

use crate::ast::{AggExpr, Expr, PipeStage};
use crate::schema::{CanonicalType, FieldTypes, catalog_key};

/// The set of catalog pins in force at one point in a pipeline.
///
/// A thin newtype over [`FieldTypes`] (Arc-shared, copy-on-write): cloning
/// is a refcount bump, and a stage that kills or remaps pins pays one
/// bounded copy — pipeline length × the catalog's size, which is capped
/// install-wide (`MAX_PINNED_FIELDS`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinScope {
    pins: FieldTypes,
}

impl PinScope {
    /// The scope at the pipeline's root: the full catalog snapshot.
    #[must_use]
    pub fn root(pins: &FieldTypes) -> Self {
        Self { pins: pins.clone() }
    }

    /// The explicitly pin-blind scope (embedded mode, plain unit tests).
    /// There is no `Default`-flavoured door on the consuming APIs —
    /// pin-blindness is always spelled out at the call site.
    #[must_use]
    pub fn unpinned() -> Self {
        Self::default()
    }

    /// Whether the scope holds no pins — the pin-blind fast path.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    /// The pin typing a comparison against `dsl_name` at this point in the
    /// pipeline, looked up through [`catalog_key`] (alias resolution +
    /// ASCII fold) — the same lookup the emitter and the live matcher
    /// share.
    #[must_use]
    pub fn pin_for(&self, dsl_name: &str) -> Option<CanonicalType> {
        self.pins.pin_for(dsl_name)
    }

    /// Advance the scope over one pipe stage: the scope BEFORE the call
    /// types that stage's own expressions; after it, the scope describes
    /// the stage's output schema.
    ///
    /// The match is exhaustive on purpose — a new stage variant must state
    /// its scope rule here or fail to compile, never silently keep a wrong
    /// pin. The rules (issue #66 / ADR-0011 slice A′ prep rulings):
    ///
    /// - `rename` remaps the pin from the old name to the new one (and an
    ///   unpinned source scrubs any pin the target name held).
    /// - `table`/`fields` restrict to the named columns; `drop` removes.
    /// - `let` resolves ALL assignments against the PRE-stage scope — the
    ///   SQL desugars to one parallel SELECT (`COLUMNS(c -> c NOT IN …)`),
    ///   so `let a = status, b = a` reads the ORIGINAL `a` column for `b`:
    ///   a sibling reference is a conservative miss in both lanes. A bare
    ///   field-ref alias copies the source's pin to the target; any other
    ///   expression kills the target's pin (the value is derived).
    /// - aggregation stages (`stats`/`timechart`/`top`/`rare`/`pivot`)
    ///   keep their group-by keys and kill every derived output
    ///   (aggregate aliases, the `timechart` `_time` bucket, `count`).
    /// - `eventstats` keeps its inputs (non-reducing) and kills only the
    ///   aggregate output columns.
    /// - `extract <regex>` kills the capture-group names it (re)writes;
    ///   `extract kv` passes the scope through — the acceptance contract
    ///   makes the kv tail pin-aware, and the residual (a kv key
    ///   shadowing a pinned name takes that pin's reading) is accepted
    ///   and documented, identical in both lanes because both consume
    ///   this walk.
    /// - `from saved` reads someone else's output: the scope clears.
    /// - selection/ordering stages pass the scope through.
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
                for (old, new) in &r.renames {
                    let pin = self.pin_for(old);
                    self.pins.remove(&catalog_key(old));
                    match pin {
                        Some(pin) => self.pins.insert(&catalog_key(new), pin),
                        None => self.pins.remove(&catalog_key(new)),
                    }
                }
            }
            PipeStage::Let(l) => {
                // Two passes so every source pin resolves against the
                // PRE-stage scope (parallel SELECT semantics, see above).
                let resolved: Vec<(String, Option<CanonicalType>)> = l
                    .assignments
                    .iter()
                    .map(|(target, expr)| {
                        let pin = match &expr.node {
                            Expr::FieldRef(source) => self.pin_for(source),
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
                // The pivoted value columns are dynamic; only the group-by
                // keys survive with their identity (issue #66 mechanism
                // text: aggregation stages keep group-by keys).
                self.restrict_to_names(p.by.iter().map(String::as_str));
            }
            PipeStage::EventStats(es) => self.remove_agg_outputs(&es.aggregations),
            PipeStage::Extract(e) => match &e.mode {
                crate::ast::ExtractMode::Regex(pattern) => {
                    // The emitter rejects an invalid regex; nothing to
                    // scrub here for one.
                    if let Ok(re) = regex::Regex::new(pattern) {
                        for name in re.capture_names().flatten() {
                            self.pins.remove(&catalog_key(name));
                        }
                    }
                }
                // kv passes through — see the method doc for the accepted
                // shadow residual.
                crate::ast::ExtractMode::KeyValue { .. } => {}
            },
            PipeStage::FromSaved(_) => self.pins = FieldTypes::new(),
        }
    }

    /// Restrict the scope to the named columns (through [`catalog_key`]).
    fn restrict_to_names<'a>(&mut self, names: impl Iterator<Item = &'a str>) {
        let keep: Vec<String> = names.map(catalog_key).collect();
        self.pins.restrict_to(&keep);
    }

    /// Kill the output column of every aggregation: the alias when one is
    /// given, else the default `func_arg`/`func` name the emitter derives.
    fn remove_agg_outputs(&mut self, aggregations: &[AggExpr]) {
        for agg in aggregations {
            let name =
                agg.alias
                    .clone()
                    .unwrap_or_else(|| match agg.args.first().map(|a| &a.node) {
                        Some(Expr::FieldRef(arg)) => format!("{}_{arg}", agg.function),
                        _ => agg.function.clone(),
                    });
            self.pins.remove(&catalog_key(&name));
        }
    }
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

    /// Walk the pipeline of `dsl` from `root`, returning the scope after
    /// every stage has been applied.
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

    // ── pass-through stages ───────────────────────────────────────────

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

    // ── rename ────────────────────────────────────────────────────────

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
    fn rename_folds_case_like_the_catalog() {
        let scope = walk("* | rename Status as St", ROOT);
        assert_eq!(scope.pin_for("st"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("ST"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("status"), None);
    }

    // ── let ───────────────────────────────────────────────────────────

    #[test]
    fn let_bare_alias_copies_the_pin() {
        let scope = walk("* | let s2 = status", ROOT);
        assert_eq!(scope.pin_for("s2"), Some(CT::Varchar));
        // The source keeps its pin — `let` adds a column.
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
        // `b = a` reads the ORIGINAL `a` column (parallel SELECT), which
        // is unpinned — the conservative miss both lanes agree on.
        let scope = walk("* | let a = status, b = a", ROOT);
        assert_eq!(scope.pin_for("a"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("b"), None);
    }

    #[test]
    fn let_overwrite_and_alias_resolve_in_parallel() {
        // `x = status` reads the original status column even though the
        // same stage overwrites `status`.
        let scope = walk("* | let status = 1, x = status", ROOT);
        assert_eq!(scope.pin_for("status"), None);
        assert_eq!(scope.pin_for("x"), Some(CT::Varchar));
    }

    #[test]
    fn let_alias_through_time_alias_resolves() {
        let scope = walk("* | let t = timestamp", ROOT);
        assert_eq!(scope.pin_for("t"), Some(CT::Timestamp));
    }

    // ── selection ─────────────────────────────────────────────────────

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

    // ── aggregations ──────────────────────────────────────────────────

    #[test]
    fn stats_keeps_group_by_keys_and_kills_everything_else() {
        let scope = walk("* | stats count() by status", ROOT);
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("dur"), None);
        assert_eq!(scope.pin_for("count"), None);
    }

    #[test]
    fn stats_alias_matching_a_pinned_name_is_killed() {
        // `avg(x) as dur` groups by status: `dur` in the output is the
        // aggregate, never the pinned column.
        let scope = walk("* | stats avg(x) as dur by status", ROOT);
        assert_eq!(scope.pin_for("dur"), None);
        assert_eq!(scope.pin_for("status"), Some(CT::Varchar));
    }

    #[test]
    fn timechart_kills_the_time_bucket_and_keeps_by_keys() {
        let scope = walk("* | timechart span=5m count() by service", ROOT);
        assert_eq!(scope.pin_for("service"), Some(CT::Varchar));
        // `_time` in the output is the computed bucket, not the column.
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
            "* | eventstats avg(dur) by service",
            &[
                ("dur", CT::BigInt),
                ("service", CT::Varchar),
                ("avg_dur", CT::Double),
            ],
        );
        // Non-reducing: every input column survives.
        assert_eq!(scope.pin_for("dur"), Some(CT::BigInt));
        assert_eq!(scope.pin_for("service"), Some(CT::Varchar));
        // The default output name `avg_dur` is the aggregate now.
        assert_eq!(scope.pin_for("avg_dur"), None);
    }

    #[test]
    fn eventstats_alias_kills_the_alias() {
        let scope = walk("* | eventstats avg(x) as dur", &[("dur", CT::BigInt)]);
        assert_eq!(scope.pin_for("dur"), None);
    }

    // ── extract ───────────────────────────────────────────────────────

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

    // ── from saved ────────────────────────────────────────────────────

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

    // ── composites ────────────────────────────────────────────────────

    #[test]
    fn rename_then_where_carries_the_pin_under_the_new_name() {
        let query = parser::parse("* | rename status as st | where st > 400").expect("parses");
        let mut scope = PinScope::root(&pins(ROOT));
        scope.advance(&query.pipeline[0].node);
        // At the `where` stage the pin lives under `st`.
        assert_eq!(scope.pin_for("st"), Some(CT::Varchar));
        assert_eq!(scope.pin_for("status"), None);
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
