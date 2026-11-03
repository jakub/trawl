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
//! A column whose every value is a NULL with no type of its own carries
//! that too ([`Column::Untyped`]): a `let` of `coalesce(f, g)` with both
//! absent, a `max(f)` of an absent `f`. A later read of it binds by name,
//! and [`Relation::untyped_null`] tells the emitter where such a read
//! needs a type `DuckDB` cannot infer.
//!
//! Every name is folded through [`catalog_key`], `DuckDB`'s ASCII
//! identifier fold.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Expr, LiteralValue, PipeStage, Spanned};
use crate::pin_scope::PinScope;
use crate::schema::catalog_key;

use super::InputColumns;

/// One column the relation carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Column {
    /// Real values: a source carried the field, or a stage computed it.
    Carried,
    /// Computed, and NULL in every row by construction, with no type of
    /// its own: an expression or aggregate whose every value-carrying
    /// argument was an absent unpinned field or another such column
    /// ([`Relation::untyped_null`]). Read by name like [`Self::Carried`],
    /// and an output column keeps the type `DuckDB` gives it, but a read at
    /// an overloaded position needs a type
    /// ([`super::functions::absent_argument_type`]).
    Untyped,
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
    /// zero-row fill and then reads the name ([`Rest::Pivot`]).
    Pad,
    /// Refuse the query: the read reaches an envelope timestamp the source
    /// lacks ([`Column::Missing`]).
    Refused,
    /// Refuse the query: after a pivot, a read of a name after the `let`
    /// assigned it, when an earlier read of the same `let` padded it. The
    /// pad makes the name an input column whether or not the data made
    /// one, and `DuckDB` binds an input column before the alias, so the
    /// later read could mean either. Split in two `let`s, it cannot.
    Ambiguous,
}

/// The columns at one point of a pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Relation {
    /// Nobody described the input ([`InputColumns::unprobed`]), or it has
    /// no source to describe ([`Relation::root`]), and no stage since has
    /// set or removed a column: every name binds, and the emitter writes
    /// the SQL it wrote before it could know.
    Open,
    /// Exactly these columns, keyed by folded name.
    Known(BTreeMap<String, Column>),
    /// Some names decided by the stages, the rest unknown: an undescribed
    /// input that a stage removed a name from or wrote a direct NULL
    /// into, or the output of a dynamic `pivot`, until a stage replaces
    /// the column set.
    Partial(Partial),
}

/// What is known of a relation whose other columns nobody can name: every
/// name a stage wrote or removed, and, after a pivot, its group-by keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Partial {
    known: BTreeMap<String, Column>,
    removed: BTreeSet<String>,
    rest: Rest,
}

/// Who decides the names a [`Partial`] relation does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rest {
    /// The undescribed input: a read of one binds by name, as at an
    /// [`Relation::Open`] root.
    Input,
    /// A dynamic `pivot`, which makes its columns from the data: a read of
    /// one is padded ([`Binding::Pad`]). `untyped` when its aggregate is
    /// NULL with no type by construction (`max(f)` of an absent `f`), so
    /// every value column it makes is [`Column::Untyped`].
    Pivot { untyped: bool },
}

impl Partial {
    fn open() -> Self {
        Self {
            known: BTreeMap::new(),
            removed: BTreeSet::new(),
            rest: Rest::Input,
        }
    }
}

/// The `let` being emitted (`stream::apply_let`'s order): the
/// assignments it has emitted so far, in order, which a read in a later
/// assignment binds when the input lacks the name, every name its targets
/// assign, and which of those an earlier read padded onto a pivot's fill.
/// [`Lateral::NONE`] outside a `let`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Lateral {
    assigned: Vec<(String, Column)>,
    targets: BTreeSet<String>,
    padded: BTreeSet<String>,
}

impl Lateral {
    /// Outside a `let`: no alias and no target.
    pub(crate) const NONE: Self = Self {
        assigned: Vec::new(),
        targets: BTreeSet::new(),
        padded: BTreeSet::new(),
    };

    /// A `let` about to emit its assignments to `targets`.
    pub(crate) fn new<'a>(targets: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            targets: targets.into_iter().map(catalog_key).collect(),
            ..Self::NONE
        }
    }

    /// A read of `name` was padded onto a pivot's fill. Only a target
    /// counts: a read after its assignment then has two meanings
    /// ([`Binding::Ambiguous`]).
    pub(crate) fn padded(&mut self, name: &str) {
        let key = catalog_key(name);
        if self.targets.contains(&key) {
            self.padded.insert(key);
        }
    }

    /// One assignment was emitted: a later one can read it.
    pub(crate) fn push(&mut self, target: &str, column: Column) {
        self.assigned.push((catalog_key(target), column));
    }

    /// The column the latest assignment to `key` wrote.
    fn alias(&self, key: &str) -> Option<&Column> {
        self.assigned
            .iter()
            .rev()
            .find(|(target, _)| target == key)
            .map(|(_, column)| column)
    }

    /// The columns the assignments wrote, in order.
    fn into_assigned(self) -> impl Iterator<Item = Column> {
        self.assigned.into_iter().map(|(_, column)| column)
    }
}

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
    pub(crate) fn bind(&self, name: &str, lateral: &Lateral) -> Binding {
        let key = catalog_key(name);
        let alias = lateral.alias(&key);
        match self {
            Self::Open => Binding::Name,
            Self::Known(columns) => match columns.get(&key) {
                Some(Column::Carried | Column::Untyped) => Binding::Name,
                Some(Column::Null { .. }) => Binding::Null,
                // The input lacks the name, so an earlier alias binds, and
                // only then NULL or, for the source's missing envelope
                // timestamp, the refusal.
                lacking => match alias {
                    Some(Column::Carried | Column::Untyped) => Binding::Name,
                    Some(Column::Missing) => Binding::Refused,
                    None if lacking.is_some() => Binding::Refused,
                    Some(Column::Null { .. }) | None => Binding::Null,
                },
            },
            Self::Partial(partial) => match partial.known.get(&key) {
                Some(Column::Carried | Column::Untyped) => Binding::Name,
                Some(Column::Null { .. }) => Binding::Null,
                Some(Column::Missing) => Binding::Refused,
                // The input or the data decides whether the column exists,
                // and `DuckDB` binds an input column before the alias,
                // which is the order this lookup promises. So an alias is
                // read by name and never padded: a fill would manufacture
                // an input column that steals the alias. When an earlier
                // read of this `let` padded the name, that column exists
                // already, and the read is refused.
                None if alias.is_some() && lateral.padded.contains(&key) => Binding::Ambiguous,
                None if alias.is_some() => Binding::Name,
                None if partial.removed.contains(&key) => Binding::Null,
                // A read before the `let` assigns the name, or in its own
                // assignment, is an input read: it pads, and reads the
                // pivot's column when the data made one.
                None => match partial.rest {
                    Rest::Input => Binding::Name,
                    Rest::Pivot { .. } => Binding::Pad,
                },
            },
        }
    }

    /// Whether `expr` is a NULL with no type of its own by construction
    /// (ADR-0062): what `DuckDB` cannot pick an overload for when it is an
    /// argument ([`super::functions::absent_argument_type`]).
    ///
    /// The leaves are a `null` literal, a read that resolves to an
    /// untyped NULL literal (an absent field the scope does not pin), and
    /// a read of a [`Column::Untyped`] column. A call is one when each
    /// argument it can return as its own value is one
    /// ([`super::functions::passthrough_arguments`]): `coalesce`, the
    /// branches of `if` and `case`, `min`, `max`, `first` and `last`, at
    /// any depth. Every operator types its result whatever its operands
    /// are (arithmetic a number, comparison, logic and `IN` a boolean),
    /// and so does every other function, so neither is one.
    ///
    /// `pins` is the scope the read resolves in: a pinned absent field is
    /// a NULL of its pin's type, which `DuckDB` binds like a column of it.
    pub(crate) fn untyped_null(&self, expr: &Expr, lateral: &Lateral, pins: &PinScope) -> bool {
        match expr {
            Expr::Literal(LiteralValue::Null) => true,
            Expr::FieldRef(name) => match self.bind(name, lateral) {
                Binding::Null => pins.pin_for(name).is_none(),
                Binding::Name | Binding::Pad => self.untyped_column(name, lateral),
                Binding::Refused | Binding::Ambiguous => false,
            },
            Expr::FunctionCall { name, args } => self.untyped_call(name, args, lateral, pins),
            Expr::Literal(_) | Expr::Binary { .. } | Expr::Unary { .. } | Expr::InList { .. } => {
                false
            }
        }
    }

    /// Whether a call to `name` over `args` is a NULL with no type of its
    /// own ([`Self::untyped_null`]): the same rule for a scalar call and
    /// for an aggregation stage's call.
    pub(crate) fn untyped_call(
        &self,
        name: &str,
        args: &[Spanned<Expr>],
        lateral: &Lateral,
        pins: &PinScope,
    ) -> bool {
        super::functions::passthrough_arguments(name, args).is_some_and(|values| {
            !values.is_empty()
                && values
                    .iter()
                    .all(|arg| self.untyped_null(&arg.node, lateral, pins))
        })
    }

    /// Whether a read of `name` that binds by name, or pads, reads a
    /// [`Column::Untyped`] column. Where the input or the data decides
    /// whether the column exists, it may hold real values, so it is not.
    fn untyped_column(&self, name: &str, lateral: &Lateral) -> bool {
        let key = catalog_key(name);
        let alias = lateral.alias(&key);
        match self {
            Self::Open => false,
            Self::Known(columns) => columns.get(&key).or(alias) == Some(&Column::Untyped),
            Self::Partial(partial) => match partial.known.get(&key) {
                Some(column) => *column == Column::Untyped,
                None if alias.is_some() => false,
                None => partial.rest == Rest::Pivot { untyped: true },
            },
        }
    }

    /// The column a direct projection of `name` under `spelling` writes:
    /// `table`, a group key, a rename target, `let y = name`.
    fn projected(&self, name: &str, spelling: &str, lateral: &Lateral) -> Column {
        match self.bind(name, lateral) {
            // A padded name exists once the fill runs: a real column or
            // the fill's NULL of the same type. Either way it is the
            // pivot's column, not a direct NULL of this read.
            Binding::Name | Binding::Pad if self.untyped_column(name, lateral) => Column::Untyped,
            Binding::Name | Binding::Pad => Column::Carried,
            Binding::Null => Column::Null {
                spelling: spelling.to_owned(),
            },
            // The emission refused the read before the walk got here.
            Binding::Refused | Binding::Ambiguous => Column::Missing,
        }
    }

    /// The column one `let` assignment writes: a bare reference keeps its
    /// source's lineage, an untyped NULL by construction is
    /// [`Column::Untyped`], and any other expression computes real values.
    pub(crate) fn let_target(
        &self,
        target: &str,
        expr: &Expr,
        lateral: &Lateral,
        pins: &PinScope,
    ) -> Column {
        match expr {
            Expr::FieldRef(source) => self.projected(source, target, lateral),
            _ if self.untyped_null(expr, lateral, pins) => Column::Untyped,
            _ => Column::Carried,
        }
    }

    /// The columns a `let` writes, in order. Every assignment reads the
    /// pre-stage relation and the assignments before it, exactly as the
    /// emission did.
    fn let_written(&self, l: &crate::ast::LetStage, pins: &PinScope) -> Vec<(String, Column)> {
        let mut lateral = Lateral::new(l.assignments.iter().map(|(t, _)| t.as_str()));
        for (target, expr) in &l.assignments {
            let column = self.let_target(target, &expr.node, &lateral, pins);
            lateral.push(target, column);
        }
        l.assignments
            .iter()
            .map(|(target, _)| target.clone())
            .zip(lateral.into_assigned())
            .collect()
    }

    /// The columns an aggregation stage writes, in order: each
    /// [`Column::Untyped`] when its call is an untyped NULL by construction
    /// (`max(f)` of an absent `f`).
    fn aggregations(&self, aggs: &[crate::ast::AggExpr], pins: &PinScope) -> Vec<(String, Column)> {
        aggs.iter()
            .map(|agg| {
                let column = if self.untyped_call(&agg.function, &agg.args, &Lateral::NONE, pins) {
                    Column::Untyped
                } else {
                    Column::Carried
                };
                (crate::projection::agg_output_name(agg), column)
            })
            .collect()
    }

    /// The direct-null columns the relation still carries, as written.
    pub(crate) fn nulls(&self) -> Vec<&str> {
        let columns = match self {
            Self::Open => return Vec::new(),
            Self::Known(columns) => columns,
            Self::Partial(partial) => &partial.known,
        };
        columns
            .values()
            .filter_map(|column| match column {
                Column::Null { spelling } => Some(spelling.as_str()),
                Column::Carried | Column::Untyped | Column::Missing => None,
            })
            .collect()
    }

    fn insert(&mut self, name: &str, column: Column) {
        let key = catalog_key(name);
        match self {
            Self::Open => {
                let mut partial = Partial::open();
                partial.known.insert(key, column);
                *self = Self::Partial(partial);
            }
            Self::Known(columns) => {
                columns.insert(key, column);
            }
            Self::Partial(partial) => {
                partial.removed.remove(&key);
                partial.known.insert(key, column);
            }
        }
    }

    fn remove(&mut self, name: &str) {
        let key = catalog_key(name);
        match self {
            Self::Open => {
                let mut partial = Partial::open();
                partial.removed.insert(key);
                *self = Self::Partial(partial);
            }
            Self::Known(columns) => {
                columns.remove(&key);
            }
            Self::Partial(partial) => {
                partial.known.remove(&key);
                partial.removed.insert(key);
            }
        }
    }

    /// An undescribed input that the stages left binding every name as
    /// written is [`Self::Open`] again: a column they wrote with real
    /// values binds by name either way.
    fn settle(&mut self) {
        if let Self::Partial(partial) = self
            && partial.rest == Rest::Input
            && partial.removed.is_empty()
            && partial
                .known
                .values()
                .all(|column| *column == Column::Carried)
        {
            *self = Self::Open;
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
    ///
    /// An [`Self::Open`] input advances too. What it carries is unknown,
    /// but what a stage sets or removes is not, so the walk gives what a
    /// described input's walk gives for every name a stage decided: the
    /// probe is skipped for a query that reads only the envelope
    /// timestamps, and `table _time | where isnull(_ingested)` still reads
    /// a NULL where the probed path does.
    ///
    /// `pins` is the scope the stage's own reads resolve in, before the
    /// stage advances it.
    pub(crate) fn advance(&mut self, stage: &PipeStage, pins: &PinScope) {
        self.apply(stage, pins);
        self.settle();
    }

    fn apply(&mut self, stage: &PipeStage, pins: &PinScope) {
        match stage {
            PipeStage::Where(_)
            | PipeStage::Sort(_)
            | PipeStage::Limit(_)
            | PipeStage::Tail(_)
            | PipeStage::Sample(_)
            | PipeStage::Dedup(_) => {}
            PipeStage::Table(t) => {
                let columns: Vec<(String, Column)> =
                    super::fields::first_of_each(t.fields.iter().map(String::as_str))
                        .into_iter()
                        .map(|f| (f.to_owned(), self.projected(f, f, &Lateral::NONE)))
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
                    .map(|(old, new)| (new.clone(), self.projected(old, new, &Lateral::NONE)))
                    .collect();
                for (old, _) in &r.renames {
                    self.remove(old);
                }
                for (new, column) in targets {
                    self.insert(&new, column);
                }
            }
            PipeStage::Let(l) => {
                for (target, column) in self.let_written(l, pins) {
                    self.insert(&target, column);
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
                for (name, column) in self.aggregations(&es.aggregations, pins) {
                    self.insert(&name, column);
                }
            }
            PipeStage::Stats(s) => {
                let keys = s
                    .group_by
                    .iter()
                    .map(|k| (k.clone(), self.projected(k, k, &Lateral::NONE)));
                let columns: Vec<_> = keys
                    .chain(self.aggregations(&s.aggregations, pins))
                    .collect();
                *self = Self::replaced(columns);
            }
            PipeStage::Timechart(t) => {
                let bucket = std::iter::once((crate::schema::TIME.to_owned(), Column::Carried));
                let keys = t
                    .group_by
                    .iter()
                    .map(|k| (k.clone(), self.projected(k, k, &Lateral::NONE)));
                let columns: Vec<_> = bucket
                    .chain(keys)
                    .chain(self.aggregations(&t.aggregations, pins))
                    .collect();
                *self = Self::replaced(columns);
            }
            PipeStage::Top(t) => *self = self.frequency(&t.field, &t.by),
            PipeStage::Rare(r) => *self = self.frequency(&r.field, &r.by),
            PipeStage::Pivot(p) => {
                let known =
                    p.by.iter()
                        .map(|k| (catalog_key(k), self.projected(k, k, &Lateral::NONE)))
                        .collect();
                let untyped = self.untyped_call(
                    &p.aggregation.function,
                    &p.aggregation.args,
                    &Lateral::NONE,
                    pins,
                );
                *self = Self::Partial(Partial {
                    known,
                    removed: BTreeSet::new(),
                    rest: Rest::Pivot { untyped },
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
            .map(|k| (k.to_owned(), self.projected(k, k, &Lateral::NONE)));
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

    /// A `let` that has assigned `entries`, in order, and nothing else.
    fn aliases(entries: &[(&str, Column)]) -> Lateral {
        let mut lateral = Lateral::NONE;
        for (target, column) in entries {
            lateral.push(target, column.clone());
        }
        lateral
    }

    fn walk(dsl: &str, root: Relation) -> Relation {
        let query = parser::parse(dsl).expect("parses");
        let mut relation = root;
        for stage in &query.pipeline {
            relation.advance(&stage.node, &PinScope::unpinned());
        }
        relation
    }

    /// An undescribed root binds every name, and a stage that only filters
    /// or adds columns keeps it that way.
    #[test]
    fn an_open_relation_binds_every_name_until_a_stage_sets_the_columns() {
        let relation = walk(
            r#"* | where host == "x" | let a = f | sort host | dedup host"#,
            Relation::Open,
        );
        assert_eq!(relation, Relation::Open);
        assert_eq!(relation.bind("nosuch", &Lateral::NONE), Binding::Name);
        assert!(relation.nulls().is_empty());
    }

    /// An undescribed root is unknown, but what a stage sets or removes is
    /// not: the walk from it gives what the walk from a described root
    /// gives for every name a stage decided (ADR-0062).
    #[test]
    fn an_open_relation_advances_through_stages_that_set_or_remove_columns() {
        let table = walk("* | table _time", Relation::Open);
        assert_eq!(table.bind("_time", &Lateral::NONE), Binding::Name);
        assert_eq!(table.bind("_ingested", &Lateral::NONE), Binding::Null);

        let dropped = walk("* | drop _ingested", Relation::Open);
        assert_eq!(dropped.bind("_ingested", &Lateral::NONE), Binding::Null);
        assert_eq!(dropped.bind("_time", &Lateral::NONE), Binding::Name);
        assert_eq!(dropped.bind("anything", &Lateral::NONE), Binding::Name);
        assert_eq!(
            dropped.bind("_ingested", &aliases(&[("_ingested", Column::Carried)])),
            Binding::Name,
            "a lateral alias binds a dropped name"
        );
        let restored = walk("* | drop host | let host = 1", Relation::Open);
        assert_eq!(restored, Relation::Open, "a rewritten name binds again");

        let renamed = walk("* | rename host as h", Relation::Open);
        assert_eq!(renamed.bind("host", &Lateral::NONE), Binding::Null);
        assert_eq!(renamed.bind("h", &Lateral::NONE), Binding::Name);

        let stats = walk("* | stats count() | table _time", Relation::Open);
        assert_eq!(stats.bind("_time", &Lateral::NONE), Binding::Null);
        assert_eq!(stats.nulls(), ["_time"]);
        for stage in [
            "stats count() as n by host",
            "top 3 host",
            "rare 3 host",
            "timechart span=1h count()",
            "table host",
        ] {
            let described = walk(&format!("* | {stage}"), known(&["host", "_time"]));
            let open = walk(&format!("* | {stage}"), Relation::Open);
            assert_eq!(open, described, "{stage}");
        }

        let pivoted = walk("* | pivot count() on host by service", Relation::Open);
        assert_eq!(pivoted.bind("service", &Lateral::NONE), Binding::Name);
        assert_eq!(pivoted.bind("web3", &Lateral::NONE), Binding::Pad);
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
        assert_eq!(root.bind("_time", &Lateral::NONE), Binding::Refused);
        assert_eq!(root.bind("_INGESTED", &Lateral::NONE), Binding::Refused);
        assert!(root.nulls().is_empty());
        assert_eq!(
            walk(r#"* | where host == "x" | let t = 1"#, root.clone())
                .bind("_time", &Lateral::NONE),
            Binding::Refused,
            "a stage that leaves the name alone still reads the source"
        );

        let charted = walk("* | timechart on t count()", root.clone());
        assert_eq!(charted.bind("_time", &Lateral::NONE), Binding::Name);
        assert_eq!(charted.bind("_ingested", &Lateral::NONE), Binding::Null);

        let projected = walk("* | table host", root);
        assert_eq!(projected.bind("_time", &Lateral::NONE), Binding::Null);
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
            assert_eq!(root.bind(name, &Lateral::NONE), Binding::Name, "{name}");
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
        assert_eq!(hot_only.bind("_time", &Lateral::NONE), Binding::Name);
        assert_eq!(hot_only.bind("f", &Lateral::NONE), Binding::Null);
    }

    #[test]
    fn reads_fold_and_miss_to_null() {
        let relation = known(&["host", "Status"]);
        assert_eq!(relation.bind("HOST", &Lateral::NONE), Binding::Name);
        assert_eq!(relation.bind("status", &Lateral::NONE), Binding::Name);
        assert_eq!(relation.bind("f", &Lateral::NONE), Binding::Null);
    }

    #[test]
    fn projections_replace_and_remove() {
        let root = known(&["host", "service", "n"]);
        let table = walk("* | table host, f", root.clone());
        assert_eq!(table.bind("host", &Lateral::NONE), Binding::Name);
        assert_eq!(table.bind("f", &Lateral::NONE), Binding::Null);
        assert_eq!(table.bind("service", &Lateral::NONE), Binding::Null);
        assert_eq!(table.nulls(), ["f"]);

        let dropped = walk("* | drop service", root.clone());
        assert_eq!(dropped.bind("service", &Lateral::NONE), Binding::Null);
        assert_eq!(dropped.bind("n", &Lateral::NONE), Binding::Name);

        let renamed = walk("* | rename host as h, f as q", root.clone());
        assert_eq!(renamed.bind("host", &Lateral::NONE), Binding::Null);
        assert_eq!(renamed.bind("h", &Lateral::NONE), Binding::Name);
        assert_eq!(renamed.bind("q", &Lateral::NONE), Binding::Null);
        assert_eq!(renamed.nulls(), ["q"]);

        let stats = walk("* | stats count() as c by host, f", root.clone());
        assert_eq!(stats.bind("c", &Lateral::NONE), Binding::Name);
        assert_eq!(stats.bind("n", &Lateral::NONE), Binding::Null);
        assert_eq!(stats.nulls(), ["f"]);

        let chart = walk("* | timechart span=1h count() by f", root.clone());
        assert_eq!(chart.bind("_time", &Lateral::NONE), Binding::Name);
        assert_eq!(chart.nulls(), ["f"]);

        let top = walk("* | top 3 f by host", root);
        assert_eq!(top.bind("count", &Lateral::NONE), Binding::Name);
        assert_eq!(top.bind("host", &Lateral::NONE), Binding::Name);
        assert_eq!(top.nulls(), ["f"]);
    }

    /// A name `table` repeats is one column, under the spelling its first
    /// position wrote, as the emitted projection keeps it.
    #[test]
    fn a_repeated_table_name_keeps_its_first_spelling() {
        let relation = walk("* | table f, host, F, HOST", known(&["host"]));
        assert_eq!(relation.nulls(), ["f"]);
        assert_eq!(relation.bind("host", &Lateral::NONE), Binding::Name);
        assert_eq!(relation.bind("host_1", &Lateral::NONE), Binding::Null);
    }

    #[test]
    fn writes_add_their_targets() {
        let root = known(&["host", "message"]);
        let relation = walk(
            r#"* | let y = f, z = 1 | extract "(?P<w>x)" | eventstats count() as c"#,
            root,
        );
        assert_eq!(
            relation.bind("y", &Lateral::NONE),
            Binding::Null,
            "a bare absent source"
        );
        assert_eq!(relation.bind("z", &Lateral::NONE), Binding::Name);
        assert_eq!(relation.bind("w", &Lateral::NONE), Binding::Name);
        assert_eq!(relation.bind("c", &Lateral::NONE), Binding::Name);
        assert_eq!(relation.nulls(), ["y"]);
    }

    #[test]
    fn a_computed_expression_drops_the_lineage() {
        let relation = walk("* | table f | let f = coalesce(f, 0)", known(&["host"]));
        assert_eq!(relation.bind("f", &Lateral::NONE), Binding::Name);
        assert!(relation.nulls().is_empty());
    }

    #[test]
    fn a_lateral_alias_binds_only_when_the_input_lacks_the_name() {
        let relation = known(&["n"]);
        let lateral = aliases(&[
            ("a", Column::Carried),
            ("n", Column::Carried),
            (
                "y",
                Column::Null {
                    spelling: "y".to_owned(),
                },
            ),
        ]);
        assert_eq!(relation.bind("a", &lateral), Binding::Name);
        assert_eq!(relation.bind("n", &lateral), Binding::Name);
        assert_eq!(relation.bind("y", &lateral), Binding::Null);
        assert_eq!(relation.bind("nosuch", &lateral), Binding::Null);

        let chained = walk("* | let a = f, b = a, c = 1, d = c", relation);
        assert_eq!(chained.nulls(), ["a", "b"]);
        assert_eq!(chained.bind("d", &Lateral::NONE), Binding::Name);
    }

    #[test]
    fn after_a_pivot_unknown_names_pad_until_a_stage_resets() {
        let root = known(&["host", "service"]);
        let pivoted = walk("* | pivot count() on host by service", root.clone());
        assert_eq!(pivoted.bind("service", &Lateral::NONE), Binding::Name);
        assert_eq!(pivoted.bind("web3", &Lateral::NONE), Binding::Pad);
        assert_eq!(
            pivoted.bind("a", &aliases(&[("a", Column::Carried)])),
            Binding::Name,
            "a lateral alias is never padded"
        );
        let mut lateral = Lateral::new(["x", "a", "b"]);
        assert_eq!(
            pivoted.bind("a", &lateral),
            Binding::Pad,
            "a read before the let assigns the name pads"
        );
        lateral.padded("a");
        lateral.push("a", Column::Carried);
        assert_eq!(
            pivoted.bind("a", &lateral),
            Binding::Ambiguous,
            "and a read after the assignment then has two meanings"
        );
        let before = walk(
            "* | pivot count() on host by service | let x = a, a = 1",
            root.clone(),
        );
        assert!(
            before.nulls().is_empty(),
            "a read before the let assigns the name pads: the pivot's column"
        );
        assert_eq!(before.bind("x", &Lateral::NONE), Binding::Name);
        assert_eq!(before.bind("a", &Lateral::NONE), Binding::Name);

        let written = walk(
            "* | pivot count() on host by service | let a = 1",
            root.clone(),
        );
        assert_eq!(written.bind("a", &Lateral::NONE), Binding::Name);

        let dropped = walk(
            "* | pivot count() on host by service | drop web1",
            root.clone(),
        );
        assert_eq!(dropped.bind("web1", &Lateral::NONE), Binding::Null);

        let reset = walk(
            "* | pivot count() on host by service | table service",
            root.clone(),
        );
        assert_eq!(reset.bind("web3", &Lateral::NONE), Binding::Null);

        let absent_key = walk("* | pivot count() on host by g", root);
        assert_eq!(absent_key.nulls(), ["g"]);
    }

    /// The expression of `* | let y = <text>`.
    fn expr(text: &str) -> Expr {
        let query = parser::parse(&format!("* | let y = {text}")).expect("parses");
        match &query.pipeline[0].node {
            PipeStage::Let(l) => l.assignments[0].1.node.clone(),
            other => panic!("not a let: {other:?}"),
        }
    }

    /// A NULL with no type of its own by construction: an unpinned absent
    /// read, the `null` literal, and the calls that return an argument as
    /// their value when every such argument is one, at any depth.
    /// Anything that can carry data, or that `DuckDB` types itself, is not.
    #[test]
    fn untyped_null_follows_the_passthrough_calls() {
        let relation = known(&["host", "n"]);
        let unpinned = PinScope::unpinned();
        let untyped =
            |text: &str, pins: &PinScope| relation.untyped_null(&expr(text), &Lateral::NONE, pins);
        for text in [
            "f",
            "null",
            "coalesce(f, g)",
            "coalesce(f, null)",
            "if(host == \"x\", f, g)",
            "case(true, f, false, g, f)",
            "case(true, f)",
            "coalesce(if(true, f, g), case(true, g))",
            "max(f)",
            "first(coalesce(f, g))",
        ] {
            assert!(untyped(text, &unpinned), "{text}");
        }
        for text in [
            "host",
            "coalesce(f, host)",
            "if(true, f, n)",
            "case(true, f, n)",
            "f + g",
            "-f",
            "lower(f)",
            "abs(f)",
            "sum(f)",
            "values(f)",
            "f == g",
            "\"x\"",
            "1",
        ] {
            assert!(!untyped(text, &unpinned), "{text}");
        }

        let mut pins = crate::schema::FieldTypes::new();
        pins.insert("f", crate::schema::CanonicalType::BigInt);
        let pinned = PinScope::root(&pins);
        assert!(
            !untyped("f", &pinned),
            "a pinned absent field has its pin's type"
        );
        assert!(!untyped("coalesce(f, g)", &pinned));
        assert!(untyped("coalesce(g, null)", &pinned));
    }

    /// A column whose every value is an untyped NULL keeps that through
    /// the stages that write it or carry it, and a later read of it binds
    /// by name.
    #[test]
    fn untyped_columns_flow_through_the_stages() {
        let root = known(&["host", "service"]);
        let untyped = |relation: &Relation, name: &str| {
            relation.untyped_null(
                &Expr::FieldRef(name.to_owned()),
                &Lateral::NONE,
                &PinScope::unpinned(),
            )
        };
        let lets = walk(
            "* | let a = coalesce(f, g), b = a, c = lower(a)",
            root.clone(),
        );
        assert_eq!(lets.bind("a", &Lateral::NONE), Binding::Name);
        assert!(untyped(&lets, "a") && untyped(&lets, "b"));
        assert!(!untyped(&lets, "c"));
        assert!(lets.nulls().is_empty(), "an expression keeps its own type");

        let stats = walk("* | stats max(f) as m, count() as n by host", root.clone());
        assert!(untyped(&stats, "m"));
        assert!(!untyped(&stats, "n") && !untyped(&stats, "host"));
        let renamed = walk(
            "* | stats max(f) as m | rename m as q | table q",
            root.clone(),
        );
        assert!(untyped(&renamed, "q"));
        let events = walk("* | eventstats first(f) as m", root.clone());
        assert!(untyped(&events, "m") && !untyped(&events, "host"));

        let pivoted = walk("* | pivot max(f) on host by service", root.clone());
        assert!(untyped(&pivoted, "web1"), "every value column of the pivot");
        assert!(!untyped(&pivoted, "service"));
        let counted = walk("* | pivot count() on host by service", root);
        assert!(!untyped(&counted, "web1"));
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
