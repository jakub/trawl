// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which columns the relation a stage reads carries (ADR-0062).
//!
//! A field the current stage's input does not carry reads as NULL. The
//! input is the selected sources at the root, and whatever the stages
//! before this one left: `table`, `stats` and their kind replace the
//! column set, `drop` and a `rename` source remove names, `let`, `rename`
//! targets, regex `extract` and `eventstats` add them. [`Relation`] walks
//! that beside [`crate::pin_scope::PinScope`], one stage at a time, and
//! [`Relation::bind`] answers how a read of one name resolves at the
//! current point of emission.
//!
//! A column that is itself a direct NULL standing for an absent field
//! carries that lineage ([`Column::Null`]): `table f`, `let y = f`, a
//! rename of an absent source, an absent group key. A later read of it
//! resolves to the NULL literal again, never to the column's name, because
//! `DuckDB` types a projected untyped NULL as INTEGER, and an INTEGER
//! column throws against a text parameter and refuses `sum` over text.
//!
//! Every name is folded through [`catalog_key`], `DuckDB`'s ASCII
//! identifier fold.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Expr, PipeStage};
use crate::schema::catalog_key;

use super::InputColumns;

/// One column the relation carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Column {
    /// Real values: a source carried the field, or a stage computed it.
    Carried,
    /// A direct NULL standing for a field the input did not carry,
    /// written under `spelling`.
    Null { spelling: String },
    /// `_time` or `_ingested`, which the described sources lack. Every
    /// Trawl event carries both, so such a source is foreign or broken,
    /// and a read that reaches it is refused rather than answered as NULL
    /// (ADR-0062). Only the root holds it: a stage that replaces the
    /// column set (a `timechart` writes its own `_time`) leaves the source
    /// behind.
    Missing,
}

/// How one read of a name resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binding {
    /// Read the column by name: the relation carries it, or nobody
    /// described the input, or it is an earlier alias of the same `let`.
    Name,
    /// Read a NULL literal: the input lacks the name, or carries it only
    /// as a direct NULL.
    Null,
    /// After a pivot, a name the relation may or may not carry: the pivot
    /// makes its columns from the data. The read goes on the pivot's
    /// zero-row fill and then reads the name ([`Relation::AfterPivot`]).
    Pad,
    /// Refuse the query: the read reaches an envelope timestamp the source
    /// lacks ([`Column::Missing`]).
    Refused,
}

/// The columns at one point of a pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Relation {
    /// Nobody described the input ([`InputColumns::unprobed`]), or it has
    /// no source to describe ([`Relation::root`]): every name binds, at
    /// every stage, and the emitter writes the SQL it wrote before it
    /// could know.
    Open,
    /// Exactly these columns, keyed by folded name.
    Known(BTreeMap<String, Column>),
    /// The output of a dynamic `pivot`, and whatever later stages did to
    /// it until one replaces the column set.
    AfterPivot(AfterPivot),
}

/// What is known after a dynamic `pivot`: its group-by keys, and every
/// name a later stage wrote or removed. Any other name is the data's to
/// decide, so a read of it is padded ([`Binding::Pad`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AfterPivot {
    known: BTreeMap<String, Column>,
    removed: BTreeSet<String>,
}

/// The assignments a `let` has emitted so far, in order: what a read in
/// a later assignment of the same stage can bind when the input lacks
/// the name (`stream::apply_let`'s order).
pub(crate) type Lateral = Vec<(String, Column)>;

impl Relation {
    /// The relation at the root: what the selected sources carry.
    ///
    /// `hidden` names a reader column that is no field (the dummy column
    /// of [`super::ZERO_COLUMN_SOURCE`]). `excluded` names the hot keys
    /// the hot branch drops ([`super::state`]'s null-only rule).
    ///
    /// No cold file and no hot snapshot is no source at all, which is not
    /// a source that lacks `_time`. Nothing can be resolved against it, so
    /// it is [`Self::Open`]: the statement reads the empty reader as
    /// written, and that read's "no files" answer is the lane's to give
    /// (an empty result, or cold data unread when the source still reached
    /// files at resolution).
    pub(crate) fn root(columns: &InputColumns, hidden: &[&str], excluded: &[String]) -> Self {
        let no_source = columns.cold == super::ColdColumns::NoFiles && columns.hot.is_none();
        if columns.is_unprobed() || no_source {
            return Self::Open;
        }
        let mut known = BTreeMap::new();
        if let super::ColdColumns::Described(described) = &columns.cold {
            for (name, _) in described {
                if !hidden.iter().any(|h| h.eq_ignore_ascii_case(name)) {
                    known.insert(catalog_key(name), Column::Carried);
                }
            }
        }
        if let Some(hot) = &columns.hot {
            for key in hot.keys() {
                if !excluded.iter().any(|e| e == key) {
                    known.insert(catalog_key(key), Column::Carried);
                }
            }
        }
        for timestamp in crate::schema::TIMESTAMP_COLUMNS {
            known
                .entry(catalog_key(timestamp))
                .or_insert(Column::Missing);
        }
        Self::Known(known)
    }

    /// How a read of `name` resolves here: the relation first, then an
    /// earlier assignment of the `let` being emitted, then NULL.
    pub(crate) fn bind(&self, name: &str, lateral: &[(String, Column)]) -> Binding {
        let key = catalog_key(name);
        let alias = lateral
            .iter()
            .rev()
            .find(|(target, _)| *target == key)
            .map(|(_, column)| column);
        match self {
            Self::Open => Binding::Name,
            Self::Known(columns) => match columns.get(&key) {
                Some(Column::Carried) => Binding::Name,
                Some(Column::Null { .. }) => Binding::Null,
                // The input lacks the name, so an earlier alias binds, and
                // only then NULL or, for the source's missing envelope
                // timestamp, the refusal.
                lacking => match alias {
                    Some(Column::Carried) => Binding::Name,
                    Some(Column::Missing) => Binding::Refused,
                    None if lacking.is_some() => Binding::Refused,
                    Some(Column::Null { .. }) | None => Binding::Null,
                },
            },
            Self::AfterPivot(after) => match after.known.get(&key) {
                Some(Column::Carried) => Binding::Name,
                Some(Column::Null { .. }) => Binding::Null,
                Some(Column::Missing) => Binding::Refused,
                // The data decides whether the pivot made the column, and
                // `DuckDB` binds an input column before the alias, which
                // is the order this lookup promises. So an alias is read
                // by name and never padded: a fill would manufacture an
                // input column that steals the alias.
                None if alias.is_some() => Binding::Name,
                None if after.removed.contains(&key) => Binding::Null,
                None => Binding::Pad,
            },
        }
    }

    /// The column a direct projection of `name` under `spelling` writes:
    /// `table`, a group key, a rename target, `let y = name`.
    fn projected(&self, name: &str, spelling: &str, lateral: &[(String, Column)]) -> Column {
        match self.bind(name, lateral) {
            // A padded name exists once the fill runs: a real column or
            // the fill's own NULL, which `DuckDB` types INTEGER. Either way
            // it is the pivot's column, not a direct NULL of this read.
            Binding::Name | Binding::Pad => Column::Carried,
            Binding::Null => Column::Null {
                spelling: spelling.to_owned(),
            },
            // The emission refused the read before the walk got here.
            Binding::Refused => Column::Missing,
        }
    }

    /// The column one `let` assignment writes: a bare reference keeps its
    /// source's lineage, any other expression computes real values.
    pub(crate) fn let_target(
        &self,
        target: &str,
        expr: &Expr,
        lateral: &[(String, Column)],
    ) -> Column {
        match expr {
            Expr::FieldRef(source) => self.projected(source, target, lateral),
            _ => Column::Carried,
        }
    }

    /// The direct-null columns the relation still carries, as written.
    pub(crate) fn nulls(&self) -> Vec<&str> {
        let columns = match self {
            Self::Open => return Vec::new(),
            Self::Known(columns) => columns,
            Self::AfterPivot(after) => &after.known,
        };
        columns
            .values()
            .filter_map(|column| match column {
                Column::Null { spelling } => Some(spelling.as_str()),
                Column::Carried | Column::Missing => None,
            })
            .collect()
    }

    fn insert(&mut self, name: &str, column: Column) {
        let key = catalog_key(name);
        match self {
            Self::Open => {}
            Self::Known(columns) => {
                columns.insert(key, column);
            }
            Self::AfterPivot(after) => {
                after.removed.remove(&key);
                after.known.insert(key, column);
            }
        }
    }

    fn remove(&mut self, name: &str) {
        let key = catalog_key(name);
        match self {
            Self::Open => {}
            Self::Known(columns) => {
                columns.remove(&key);
            }
            Self::AfterPivot(after) => {
                after.known.remove(&key);
                after.removed.insert(key);
            }
        }
    }

    /// A relation of exactly `columns`, as a stage that replaces the
    /// column set writes it.
    fn replaced(columns: impl IntoIterator<Item = (String, Column)>) -> Self {
        Self::Known(
            columns
                .into_iter()
                .map(|(name, column)| (catalog_key(&name), column))
                .collect(),
        )
    }

    /// Advance over one processed stage: before the call the relation is
    /// the stage's input, after it the stage's output.
    ///
    /// Exhaustive over [`PipeStage`] like
    /// [`crate::pin_scope::PinScope::advance`], and called beside it: a
    /// new stage has to say what it does to the column set.
    pub(crate) fn advance(&mut self, stage: &PipeStage) {
        if *self == Self::Open {
            return;
        }
        match stage {
            PipeStage::Where(_)
            | PipeStage::Sort(_)
            | PipeStage::Limit(_)
            | PipeStage::Tail(_)
            | PipeStage::Sample(_)
            | PipeStage::Dedup(_) => {}
            PipeStage::Table(t) => {
                let columns: Vec<(String, Column)> = t
                    .fields
                    .iter()
                    .map(|f| (f.clone(), self.projected(f, f, &[])))
                    .collect();
                *self = Self::replaced(columns);
            }
            PipeStage::Drop(d) => {
                for field in &d.fields {
                    self.remove(field);
                }
            }
            PipeStage::Rename(r) => {
                // Parallel, as the SQL is: every source reads the
                // pre-stage row, so `rename a as b, b as c` gives `c` the
                // original `b`.
                let targets: Vec<(String, Column)> = r
                    .renames
                    .iter()
                    .map(|(old, new)| (new.clone(), self.projected(old, new, &[])))
                    .collect();
                for (old, _) in &r.renames {
                    self.remove(old);
                }
                for (new, column) in targets {
                    self.insert(&new, column);
                }
            }
            PipeStage::Let(l) => {
                // Every assignment reads the pre-stage relation and the
                // assignments before it, exactly as the emission did.
                let mut lateral = Lateral::new();
                for (target, expr) in &l.assignments {
                    let column = self.let_target(target, &expr.node, &lateral);
                    lateral.push((catalog_key(target), column));
                }
                for ((target, _), (_, column)) in l.assignments.iter().zip(lateral) {
                    self.insert(target, column);
                }
            }
            PipeStage::Extract(e) => match &e.mode {
                crate::ast::ExtractMode::Regex(pattern) => {
                    // The emitter refuses an invalid regex before this.
                    if let Ok(re) = regex::Regex::new(pattern) {
                        for name in re.capture_names().flatten() {
                            self.insert(name, Column::Carried);
                        }
                    }
                }
                // `extract kv` and every stage after it run in Rust over
                // the SQL prefix's rows; the walk stops before it.
                crate::ast::ExtractMode::KeyValue { .. } => {}
            },
            PipeStage::EventStats(es) => {
                for agg in &es.aggregations {
                    self.insert(&crate::projection::agg_output_name(agg), Column::Carried);
                }
            }
            PipeStage::Stats(s) => {
                let keys = s
                    .group_by
                    .iter()
                    .map(|k| (k.clone(), self.projected(k, k, &[])));
                let aggs = s
                    .aggregations
                    .iter()
                    .map(|agg| (crate::projection::agg_output_name(agg), Column::Carried));
                let columns: Vec<_> = keys.chain(aggs).collect();
                *self = Self::replaced(columns);
            }
            PipeStage::Timechart(t) => {
                let bucket = std::iter::once((crate::schema::TIME.to_owned(), Column::Carried));
                let keys = t
                    .group_by
                    .iter()
                    .map(|k| (k.clone(), self.projected(k, k, &[])));
                let aggs = t
                    .aggregations
                    .iter()
                    .map(|agg| (crate::projection::agg_output_name(agg), Column::Carried));
                let columns: Vec<_> = bucket.chain(keys).chain(aggs).collect();
                *self = Self::replaced(columns);
            }
            PipeStage::Top(t) => *self = self.frequency(&t.field, &t.by),
            PipeStage::Rare(r) => *self = self.frequency(&r.field, &r.by),
            PipeStage::Pivot(p) => {
                let known =
                    p.by.iter()
                        .map(|k| (catalog_key(k), self.projected(k, k, &[])))
                        .collect();
                *self = Self::AfterPivot(AfterPivot {
                    known,
                    removed: BTreeSet::new(),
                });
            }
            // Resolved to a source before emission; the emitter refuses
            // one that reaches it.
            PipeStage::FromSaved(_) => *self = Self::Open,
        }
    }

    /// `top`/`rare`'s output: the field, the `by` keys and `count`.
    fn frequency(&self, field: &str, by: &[String]) -> Self {
        let keys = std::iter::once(field)
            .chain(by.iter().map(String::as_str))
            .map(|k| (k.to_owned(), self.projected(k, k, &[])));
        let count = std::iter::once(("count".to_owned(), Column::Carried));
        Self::replaced(keys.chain(count).collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn known(names: &[&str]) -> Relation {
        Relation::Known(
            names
                .iter()
                .map(|n| (catalog_key(n), Column::Carried))
                .collect(),
        )
    }

    fn walk(dsl: &str, root: Relation) -> Relation {
        let query = parser::parse(dsl).expect("parses");
        let mut relation = root;
        for stage in &query.pipeline {
            relation.advance(&stage.node);
        }
        relation
    }

    #[test]
    fn an_open_relation_binds_every_name_through_every_stage() {
        let relation = walk("* | table host | stats count() by host", Relation::Open);
        assert_eq!(relation, Relation::Open);
        assert_eq!(relation.bind("nosuch", &[]), Binding::Name);
    }

    #[test]
    fn a_missing_envelope_timestamp_is_refused_until_a_stage_owns_the_name() {
        let root = Relation::root(
            &InputColumns {
                cold: super::super::ColdColumns::described([("host", "VARCHAR")]),
                hot: None,
            },
            &[],
            &[],
        );
        assert_eq!(root.bind("_time", &[]), Binding::Refused);
        assert_eq!(root.bind("_INGESTED", &[]), Binding::Refused);
        assert!(root.nulls().is_empty());
        assert_eq!(
            walk(r#"* | where host == "x" | let t = 1"#, root.clone()).bind("_time", &[]),
            Binding::Refused,
            "a stage that leaves the name alone still reads the source"
        );

        let charted = walk("* | timechart on t count()", root.clone());
        assert_eq!(charted.bind("_time", &[]), Binding::Name);
        assert_eq!(charted.bind("_ingested", &[]), Binding::Null);

        let projected = walk("* | table host", root);
        assert_eq!(projected.bind("_time", &[]), Binding::Null);
    }

    /// No cold file and no hot snapshot is no source at all, not a source
    /// that lacks `_time`. Its statement's read of the empty reader is
    /// what answers, empty or cold data unread, so every name binds as
    /// written and nothing is refused.
    #[test]
    fn an_input_with_no_source_binds_every_name_as_written() {
        let nothing = InputColumns {
            cold: super::super::ColdColumns::NoFiles,
            hot: None,
        };
        let root = Relation::root(&nothing, &[], &[]);
        assert_eq!(root, Relation::Open);
        for name in ["_time", "_ingested", "service", "f"] {
            assert_eq!(root.bind(name, &[]), Binding::Name, "{name}");
        }

        // The hot-only read keeps its snapshot's description.
        let hot_only = Relation::root(
            &InputColumns {
                cold: super::super::ColdColumns::NoFiles,
                hot: Some(super::super::HotColumns::keys_only([
                    "_time",
                    "_ingested",
                    "host",
                ])),
            },
            &[],
            &[],
        );
        assert_eq!(hot_only.bind("_time", &[]), Binding::Name);
        assert_eq!(hot_only.bind("f", &[]), Binding::Null);
    }

    #[test]
    fn reads_fold_and_miss_to_null() {
        let relation = known(&["host", "Status"]);
        assert_eq!(relation.bind("HOST", &[]), Binding::Name);
        assert_eq!(relation.bind("status", &[]), Binding::Name);
        assert_eq!(relation.bind("f", &[]), Binding::Null);
    }

    #[test]
    fn projections_replace_and_remove() {
        let root = known(&["host", "service", "n"]);
        let table = walk("* | table host, f", root.clone());
        assert_eq!(table.bind("host", &[]), Binding::Name);
        assert_eq!(table.bind("f", &[]), Binding::Null);
        assert_eq!(table.bind("service", &[]), Binding::Null);
        assert_eq!(table.nulls(), ["f"]);

        let dropped = walk("* | drop service", root.clone());
        assert_eq!(dropped.bind("service", &[]), Binding::Null);
        assert_eq!(dropped.bind("n", &[]), Binding::Name);

        let renamed = walk("* | rename host as h, f as q", root.clone());
        assert_eq!(renamed.bind("host", &[]), Binding::Null);
        assert_eq!(renamed.bind("h", &[]), Binding::Name);
        assert_eq!(renamed.bind("q", &[]), Binding::Null);
        assert_eq!(renamed.nulls(), ["q"]);

        let stats = walk("* | stats count() as c by host, f", root.clone());
        assert_eq!(stats.bind("c", &[]), Binding::Name);
        assert_eq!(stats.bind("n", &[]), Binding::Null);
        assert_eq!(stats.nulls(), ["f"]);

        let chart = walk("* | timechart span=1h count() by f", root.clone());
        assert_eq!(chart.bind("_time", &[]), Binding::Name);
        assert_eq!(chart.nulls(), ["f"]);

        let top = walk("* | top 3 f by host", root);
        assert_eq!(top.bind("count", &[]), Binding::Name);
        assert_eq!(top.bind("host", &[]), Binding::Name);
        assert_eq!(top.nulls(), ["f"]);
    }

    #[test]
    fn writes_add_their_targets() {
        let root = known(&["host", "message"]);
        let relation = walk(
            r#"* | let y = f, z = 1 | extract "(?P<w>x)" | eventstats count() as c"#,
            root,
        );
        assert_eq!(
            relation.bind("y", &[]),
            Binding::Null,
            "a bare absent source"
        );
        assert_eq!(relation.bind("z", &[]), Binding::Name);
        assert_eq!(relation.bind("w", &[]), Binding::Name);
        assert_eq!(relation.bind("c", &[]), Binding::Name);
        assert_eq!(relation.nulls(), ["y"]);
    }

    #[test]
    fn a_computed_expression_drops_the_lineage() {
        let relation = walk("* | table f | let f = coalesce(f, 0)", known(&["host"]));
        assert_eq!(relation.bind("f", &[]), Binding::Name);
        assert!(relation.nulls().is_empty());
    }

    #[test]
    fn a_lateral_alias_binds_only_when_the_input_lacks_the_name() {
        let relation = known(&["n"]);
        let lateral = vec![
            ("a".to_owned(), Column::Carried),
            ("n".to_owned(), Column::Carried),
            (
                "y".to_owned(),
                Column::Null {
                    spelling: "y".to_owned(),
                },
            ),
        ];
        assert_eq!(relation.bind("a", &lateral), Binding::Name);
        assert_eq!(relation.bind("n", &lateral), Binding::Name);
        assert_eq!(relation.bind("y", &lateral), Binding::Null);
        assert_eq!(relation.bind("nosuch", &lateral), Binding::Null);

        let chained = walk("* | let a = f, b = a, c = 1, d = c", relation);
        assert_eq!(chained.nulls(), ["a", "b"]);
        assert_eq!(chained.bind("d", &[]), Binding::Name);
    }

    #[test]
    fn after_a_pivot_unknown_names_pad_until_a_stage_resets() {
        let root = known(&["host", "service"]);
        let pivoted = walk("* | pivot count() on host by service", root.clone());
        assert_eq!(pivoted.bind("service", &[]), Binding::Name);
        assert_eq!(pivoted.bind("web3", &[]), Binding::Pad);
        assert_eq!(
            pivoted.bind("a", &[("a".to_owned(), Column::Carried)]),
            Binding::Name,
            "a lateral alias is never padded"
        );

        let written = walk(
            "* | pivot count() on host by service | let a = 1",
            root.clone(),
        );
        assert_eq!(written.bind("a", &[]), Binding::Name);

        let dropped = walk(
            "* | pivot count() on host by service | drop web1",
            root.clone(),
        );
        assert_eq!(dropped.bind("web1", &[]), Binding::Null);

        let reset = walk(
            "* | pivot count() on host by service | table service",
            root.clone(),
        );
        assert_eq!(reset.bind("web3", &[]), Binding::Null);

        let absent_key = walk("* | pivot count() on host by g", root);
        assert_eq!(absent_key.nulls(), ["g"]);
    }

    #[test]
    fn the_root_hides_and_excludes_and_unions() {
        let columns = InputColumns {
            cold: super::super::ColdColumns::described([("1", "INTEGER"), ("Host", "VARCHAR")]),
            hot: Some(super::super::HotColumns::new(["user"], ["k", "j"])),
        };
        let root = Relation::root(&columns, &["1"], &["k".to_owned()]);
        let mut expected = known(&["host", "user", "j"]);
        expected.insert("_time", Column::Missing);
        expected.insert("_ingested", Column::Missing);
        assert_eq!(root, expected);
        assert_eq!(
            Relation::root(&InputColumns::unprobed(), &[], &[]),
            Relation::Open
        );
    }
}
