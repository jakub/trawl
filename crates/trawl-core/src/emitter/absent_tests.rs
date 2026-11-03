// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The emitter's half of ADR-0062: a field the input does not carry reads
//! as NULL, checked by running the emitted SQL on a real `DuckDB`.
//!
//! Every fixture is one parquet file of three events, none of which
//! carries `f` or `g`. The descriptor is built the way the executor builds
//! it: `DESCRIBE SELECT * FROM <the emitter's own reader>`.

use duckdb::Connection;

use super::{
    ColdColumns, EmitError, EmittedQuery, HotColumns, InputColumns, SqlValue, emit_hot_only,
    emit_with_hot_source, emit_with_pins, hot_needs_input_columns, needs_input_columns,
    source_reader,
};
use crate::parser;
use crate::schema::{CanonicalType as CT, FieldTypes};

fn anchor() -> crate::context::EvalContext {
    crate::context::EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .expect("a valid RFC 3339 instant")
            .into(),
    )
}

fn pins(entries: &[(&str, CT)]) -> FieldTypes {
    let mut ft = FieldTypes::new();
    for (field, ty) in entries {
        ft.insert(field, *ty);
    }
    ft
}

/// One parquet file, and the connection that reads it.
struct Fixture {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    source: String,
    conn: Connection,
}

/// The three events every test reads. `n` is NULL on event 3.
const EVENTS: &str = "SELECT CAST(id AS BIGINT) AS id, \
     _time, _time AS _ingested, host, service, CAST(n AS BIGINT) AS n, message, _raw \
     FROM (VALUES \
       (1, TIMESTAMP '2026-01-01 00:00:01', 'web1', 'api', 10, 'boom one', '{\"m\":\"boom one\"}'), \
       (2, TIMESTAMP '2026-01-01 00:00:02', 'web2', 'db', 20, 'calm two', '{\"m\":\"calm two\"}'), \
       (3, TIMESTAMP '2026-01-01 00:00:03', 'web1', 'db', NULL, 'boom three', '{\"m\":\"boom three\"}')) \
     AS t(id, _time, host, service, n, message, _raw)";

fn fixture_from(select: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cold.parquet");
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    conn.execute_batch(&format!(
        "COPY ({select}) TO '{}' (FORMAT PARQUET)",
        path.display()
    ))
    .expect("parquet write succeeds");
    Fixture {
        dir: dir.path().to_path_buf(),
        source: path.to_str().expect("utf-8 temp path").to_owned(),
        _dir: dir,
        conn,
    }
}

fn fixture() -> Fixture {
    fixture_from(EVENTS)
}

/// `DESCRIBE` as (name, type) pairs.
fn describe(conn: &Connection, relation: &str) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(&format!("DESCRIBE {relation}"))
        .unwrap_or_else(|e| panic!("DESCRIBE {relation}: {e}"));
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("describe runs")
        .collect::<Result<_, _>>()
        .expect("describe rows")
}

/// The descriptor the executor's probe builds for `source`.
fn described(conn: &Connection, source: &str) -> InputColumns {
    let reader = source_reader(source).expect("a valid source");
    InputColumns {
        cold: ColdColumns::described(describe(conn, &format!("SELECT * FROM {reader}"))),
        hot: None,
    }
}

fn emit_probed(dsl: &str, fx: &Fixture, pin_set: &FieldTypes) -> Result<EmittedQuery, EmitError> {
    let query = parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl} parses: {e:?}"));
    emit_with_pins(
        &query,
        &fx.source,
        pin_set,
        &described(&fx.conn, &fx.source),
        anchor(),
    )
}

fn emit_ok(dsl: &str, fx: &Fixture) -> EmittedQuery {
    emit_probed(dsl, fx, &FieldTypes::new()).unwrap_or_else(|e| panic!("{dsl} emits: {e}"))
}

fn bind(params: &[SqlValue]) -> Vec<Box<dyn duckdb::ToSql>> {
    params
        .iter()
        .map(|val| -> Box<dyn duckdb::ToSql> {
            match val {
                SqlValue::String(s) => Box::new(s.clone()),
                SqlValue::Int(i) => Box::new(*i),
                SqlValue::Float(f) => Box::new(*f),
                SqlValue::Bool(b) => Box::new(*b),
                SqlValue::Timestamp(at) => Box::new(duckdb::types::Value::Timestamp(
                    duckdb::types::TimeUnit::Microsecond,
                    at.and_utc().timestamp_micros(),
                )),
            }
        })
        .collect()
}

/// A statement's answer: its columns with their stored types, and its
/// rows as text, in result order.
#[derive(Debug)]
struct Answer {
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Option<String>>>,
}

impl Answer {
    fn names(&self) -> Vec<&str> {
        self.columns.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn type_of(&self, name: &str) -> &str {
        &self
            .columns
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no column {name} in {:?}", self.columns))
            .1
    }

    fn column(&self, name: &str) -> Vec<Option<&str>> {
        let at = self
            .columns
            .iter()
            .position(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no column {name} in {:?}", self.columns));
        self.rows.iter().map(|row| row[at].as_deref()).collect()
    }

    /// The `id` column, sorted.
    fn ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .column("id")
            .into_iter()
            .map(|id| id.expect("every event has an id").parse().expect("an id"))
            .collect();
        ids.sort_unstable();
        ids
    }
}

/// Run the statement as the query lane does, and read its result columns
/// with their logical types, and its rows as `DuckDB`'s own text, in
/// result order.
fn run(conn: &Connection, emitted: &EmittedQuery) -> Result<Answer, duckdb::Error> {
    let params = bind(&emitted.params);
    let refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&emitted.sql)?;
    let rows = stmt.query(refs.as_slice())?;
    let bound = rows.as_ref().expect("a statement that ran");
    let columns: Vec<(String, String)> = (0..bound.column_count())
        .map(|i| {
            let name = bound.column_name(i).expect("a column name").clone();
            let ty = format!("{:?}", bound.column_logical_type(i).id()).to_ascii_uppercase();
            (name, ty)
        })
        .collect();
    drop(rows);

    let mut text = conn.prepare(&format!(
        "SELECT CAST(COLUMNS(*) AS VARCHAR) FROM (\n{}\n)",
        emitted.sql
    ))?;
    let width = columns.len();
    let rows = text
        .query_map(refs.as_slice(), |row| {
            (0..width)
                .map(|i| row.get::<_, Option<String>>(i))
                .collect::<Result<Vec<_>, _>>()
        })?
        .collect::<Result<_, _>>()?;
    Ok(Answer { columns, rows })
}

fn answer(dsl: &str, fx: &Fixture) -> Answer {
    let emitted = emit_ok(dsl, fx);
    run(&fx.conn, &emitted).unwrap_or_else(|e| panic!("{dsl} runs: {e}\n{}", emitted.sql))
}

fn ids(dsl: &str, fx: &Fixture) -> Vec<i64> {
    answer(dsl, fx).ids()
}

/// No absent read leaves an identifier for `DuckDB` to bind: the SQL
/// never names `f` or `g`.
fn assert_never_names_f(dsl: &str, fx: &Fixture) {
    let sql = emit_ok(dsl, fx).sql;
    for name in ["\"f\"", "\"g\""] {
        assert!(
            !sql.contains(name),
            "{dsl} must not read the absent {name} by name:\n{sql}"
        );
    }
}

// ---------------------------------------------------------------------------
// absent operands
// ---------------------------------------------------------------------------

/// Every search operator over an absent field answers as over a NULL:
/// `!=` and the negated patterns and lists match (ADR-0011, ADR-0060),
/// everything positive is unknown, `NOT` keeps three-valued logic, and an
/// absent field never removes a sibling branch's matches.
#[test]
fn absent_search_operands_read_null() {
    let fx = fixture();
    let all = vec![1, 2, 3];
    let web1 = vec![1, 3];
    let cases: &[(&str, &[i64])] = &[
        ("f=x", &[]),
        ("f!=x", &all),
        ("f!=*", &all),
        ("f=*", &[]),
        ("f!=a*", &all),
        ("f!=/re/", &all),
        ("f=a,b*", &[]),
        ("f!=a,b*", &all),
        ("f>1", &[]),
        ("NOT f=x", &[]),
        ("NOT f!=x", &[]),
        ("host=web1 OR f=x", &web1),
        ("f=x OR host=web1", &web1),
        ("host=web1 f!=x", &web1),
        ("boom f!=x", &web1),
        ("f=x OR g!=y", &all),
    ];
    for (dsl, expect) in cases {
        assert_eq!(ids(dsl, &fx), *expect, "{dsl}");
        assert_never_names_f(dsl, &fx);
    }
}

/// `where` stays strict over an absent field, and functions take it as
/// NULL.
#[test]
fn absent_expression_operands_read_null() {
    let fx = fixture();
    let cases: &[(&str, &[i64])] = &[
        (r#"* | where f == "x""#, &[]),
        (r#"* | where f != "x""#, &[]),
        ("* | where isnull(f)", &[1, 2, 3]),
        (r#"* | where coalesce(f, "d") == "d""#, &[1, 2, 3]),
        ("* | where f > 1 or host == \"web2\"", &[2]),
        (r#"* | where f in ("a", "b")"#, &[]),
        (r#"* | where lower(f) == "x""#, &[]),
        (r#"* | where f matches "x""#, &[]),
    ];
    for (dsl, expect) in cases {
        assert_eq!(ids(dsl, &fx), *expect, "{dsl}");
        assert_never_names_f(dsl, &fx);
    }
}

/// Aggregate arguments over an absent field: `count` and `dc` are 0, the
/// numeric reductions are NULL.
#[test]
fn absent_aggregate_arguments_read_null() {
    let fx = fixture();
    let got = answer(
        "* | stats count(f) as c, dc(f) as d, sum(f) as s, avg(f) as a, min(f) as lo, max(f) as hi",
        &fx,
    );
    assert_eq!(got.rows.len(), 1);
    assert_eq!(got.column("c"), [Some("0")]);
    assert_eq!(got.column("d"), [Some("0")]);
    for name in ["s", "a", "lo", "hi"] {
        assert_eq!(got.column(name), [None], "{name}");
    }

    let by_host = answer("* | stats sum(f) as s by host", &fx);
    assert_eq!(by_host.column("s"), [None, None]);
}

// ---------------------------------------------------------------------------
// absent output columns
// ---------------------------------------------------------------------------

/// `table` and `fields` materialize an absent requested name, in order,
/// as an all-null column that a result stores as VARCHAR.
#[test]
fn table_materializes_absent_names_in_order() {
    let fx = fixture();
    for stage in ["table", "fields"] {
        let got = answer(&format!("* | {stage} id, f, host"), &fx);
        assert_eq!(got.names(), ["id", "f", "host"], "{stage}");
        assert_eq!(got.type_of("f"), "VARCHAR", "{stage}");
        assert_eq!(got.column("f"), [None, None, None], "{stage}");
        assert_eq!(got.ids(), [1, 2, 3], "{stage}");
    }
}

/// `let y = f`, `rename f as q` and `stats … by f` write their target as
/// NULL; a direct NULL of an unpinned absent field is VARCHAR.
#[test]
fn absent_sources_write_null_targets() {
    let fx = fixture();

    let let_alias = answer("* | let y = f", &fx);
    assert_eq!(let_alias.column("y"), [None, None, None]);
    assert_eq!(let_alias.type_of("y"), "VARCHAR");
    assert!(!let_alias.names().contains(&"f"), "{:?}", let_alias.names());

    let renamed = answer("* | rename f as q", &fx);
    assert_eq!(renamed.column("q"), [None, None, None]);
    assert_eq!(renamed.type_of("q"), "VARCHAR");
    assert!(!renamed.names().contains(&"f"), "{:?}", renamed.names());

    let grouped = answer("* | stats count() as c by f", &fx);
    assert_eq!(grouped.names(), ["f", "c"]);
    assert_eq!(grouped.rows, [vec![None, Some("3".to_owned())]]);
    assert_eq!(grouped.type_of("f"), "VARCHAR");

    let mixed = answer("* | stats count() as c by host, f | sort host", &fx);
    assert_eq!(mixed.column("host"), [Some("web1"), Some("web2")]);
    assert_eq!(mixed.column("f"), [None, None]);
    assert_eq!(mixed.column("c"), [Some("2"), Some("1")]);

    let top = answer("* | top 5 f", &fx);
    assert_eq!(top.names(), ["f", "count"]);
    assert_eq!(top.rows, [vec![None, Some("3".to_owned())]]);
    assert_eq!(top.type_of("f"), "VARCHAR");

    let rare = answer("* | rare 5 f by host | sort host", &fx);
    assert_eq!(rare.column("host"), [Some("web1"), Some("web2")]);
    assert_eq!(rare.column("f"), [None, None]);

    let charted = answer("* | timechart span=1h count() as c by f", &fx);
    assert_eq!(charted.column("f"), [None]);
    assert_eq!(charted.column("c"), [Some("3")]);
    assert_eq!(charted.type_of("f"), "VARCHAR");
}

/// A grouping over an absent key keeps SQL's empty-input answer: no
/// input, no group.
#[test]
fn an_absent_group_key_over_no_rows_is_no_group() {
    let fx = fixture();
    let got = answer("host=nosuch | stats count() as c by f", &fx);
    assert!(got.rows.is_empty(), "{:?}", got.rows);
}

/// `dedup`, `eventstats` and `sort` over an absent field: one null group,
/// a constant key that changes no order.
#[test]
fn absent_partitions_and_sort_keys_read_null() {
    let fx = fixture();
    let deduped = answer("* | dedup f", &fx);
    assert_eq!(deduped.ids(), [3], "one null group keeps its latest event");

    let windowed = answer("* | eventstats count() as c by f", &fx);
    assert_eq!(windowed.column("c"), [Some("3"), Some("3"), Some("3")]);

    let sorted = answer("* | sort f, -id", &fx);
    assert_eq!(sorted.column("id"), [Some("3"), Some("2"), Some("1")]);
    let trailing = answer("* | sort id, f", &fx);
    assert_eq!(trailing.column("id"), [Some("1"), Some("2"), Some("3")]);
}

// ---------------------------------------------------------------------------
// pins
// ---------------------------------------------------------------------------

/// A pinned absent field reads as a NULL of its pin's storage type, as an
/// operand and as an output column.
#[test]
fn a_pinned_absent_field_reads_as_its_pin_type() {
    let fx = fixture();
    let pinned = pins(&[("f", CT::BigInt), ("g", CT::Timestamp)]);

    let filter = emit_probed("* | where f > 1", &fx, &pinned).expect("emits");
    assert!(
        filter.sql.contains("CAST(NULL AS BIGINT)"),
        "{}",
        filter.sql
    );
    assert!(run(&fx.conn, &filter).expect("runs").rows.is_empty());

    let search = emit_probed("f!=5", &fx, &pinned).expect("emits");
    assert!(
        search.sql.contains("CAST(NULL AS BIGINT)"),
        "{}",
        search.sql
    );
    assert_eq!(run(&fx.conn, &search).expect("runs").ids(), [1, 2, 3]);

    let table = emit_probed("* | table f, g", &fx, &pinned).expect("emits");
    let got = run(&fx.conn, &table).expect("runs");
    assert_eq!(got.type_of("f"), "BIGINT");
    assert_eq!(got.type_of("g"), "TIMESTAMP");

    let renamed = emit_probed("* | table id, f | rename f as q", &fx, &pinned).expect("emits");
    assert_eq!(
        run(&fx.conn, &renamed).expect("runs").type_of("q"),
        "BIGINT",
        "the pin follows the rename, and so does its type"
    );
}

// ---------------------------------------------------------------------------
// lateral `let`
// ---------------------------------------------------------------------------

/// `let` binds an input column first, then an earlier assignment of the
/// same stage, then NULL — `stream::apply_let`'s order (plan R5).
#[test]
fn let_reads_the_input_then_a_lateral_alias_then_null() {
    let fx = fixture();

    let lateral = answer("* | let a = 1, b = a", &fx);
    assert_eq!(lateral.column("b"), [Some("1"), Some("1"), Some("1")]);

    let shadowed = answer("* | where isnull(ms) | let ms = 1000, total = ms * 2", &fx);
    assert_eq!(shadowed.ids(), [1, 2, 3]);
    assert_eq!(
        shadowed.column("total"),
        [Some("2000"), Some("2000"), Some("2000")]
    );

    let input_first = answer("* | let n = 1, b = n", &fx);
    assert_eq!(
        input_first.column("b"),
        [Some("10"), Some("20"), None],
        "an input column wins over the earlier alias, as in the live lane"
    );

    let none = answer("* | let a = g, b = a", &fx);
    assert_eq!(none.column("a"), [None, None, None]);
    assert_eq!(none.column("b"), [None, None, None]);
    assert_eq!(none.type_of("b"), "VARCHAR");
}

/// The documented exception to "absent equals explicit null" (plan R5): a
/// read that binds a same-stage alias. `let a = 1, b = a` is 1 when the
/// input lacks `a`, and NULL when the input carries an all-null `a`.
#[test]
fn a_lateral_read_is_the_absent_equals_null_exception() {
    let carrying = fixture_from(&format!(
        "SELECT *, CAST(NULL AS BIGINT) AS a FROM ({EVENTS})"
    ));
    let got = answer("* | let a = 1, b = a", &carrying);
    assert_eq!(got.column("b"), [None, None, None]);

    let lacking = fixture();
    assert_eq!(
        answer("* | let a = 1, b = a", &lacking).column("b"),
        [Some("1"), Some("1"), Some("1")]
    );
}

// ---------------------------------------------------------------------------
// reads after a projection
// ---------------------------------------------------------------------------

/// A stage that removed a field leaves it absent: later reads are NULL,
/// and the output gains no column.
#[test]
fn post_projection_reads_are_null() {
    let fx = fixture();
    let cases: &[(&str, &[i64], &[&str])] = &[
        (
            "* | table id, host | where isnull(f)",
            &[1, 2, 3],
            &["id", "host"],
        ),
        (
            r#"* | table id, host | where f == "x""#,
            &[],
            &["id", "host"],
        ),
        ("* | drop host | where isnull(host)", &[1, 2, 3], &[]),
        ("* | rename host as h | where isnull(host)", &[1, 2, 3], &[]),
    ];
    for (dsl, expect, names) in cases {
        let got = answer(dsl, &fx);
        assert_eq!(got.ids(), *expect, "{dsl}");
        if !names.is_empty() {
            assert_eq!(got.names(), *names, "{dsl}");
        }
        assert!(
            !got.names().contains(&"f"),
            "{dsl} must not gain an f column"
        );
    }
    let stats = answer("* | stats count() as c by host | where isnull(f)", &fx);
    assert_eq!(stats.names(), ["host", "c"]);
    assert_eq!(stats.rows.len(), 2);
}

/// A column that is a direct NULL for an absent field reads as the NULL
/// literal downstream, never by name: an INTEGER-typed NULL column would
/// throw against a VARCHAR parameter and refuse `sum` over VARCHAR.
#[test]
fn a_direct_null_column_is_read_as_the_literal_downstream() {
    let fx = fixture();
    let cases: &[(&str, &[i64])] = &[
        (r#"* | table id, f | where f == "x""#, &[]),
        (r#"* | table id, f | where f != "x""#, &[]),
        ("* | table id, f | where isnull(f)", &[1, 2, 3]),
        (r#"* | let y = f | where y == "x""#, &[]),
        (r#"* | rename f as q | where q == "x""#, &[]),
    ];
    for (dsl, expect) in cases {
        assert_eq!(ids(dsl, &fx), *expect, "{dsl}");
    }
    for dsl in [
        "* | table f | stats sum(f) as s",
        "* | let y = f | stats sum(y) as s",
        "* | stats count() by f | stats sum(f) as s",
    ] {
        let got = answer(dsl, &fx);
        assert_eq!(got.column("s"), [None], "{dsl}");
    }
}

/// The VARCHAR cast is for the final result only. A computed expression
/// over an absent field keeps the type `DuckDB` gives it.
#[test]
fn only_surviving_direct_nulls_are_cast_to_varchar() {
    let fx = fixture();
    let computed = answer("* | let y = coalesce(f, 0)", &fx);
    assert_eq!(computed.type_of("y"), "BIGINT", "the bound literal's type");
    assert_eq!(computed.column("y"), [Some("0"), Some("0"), Some("0")]);

    let summed = answer("* | stats sum(f) as s", &fx);
    assert_eq!(summed.type_of("s"), "HUGEINT");

    let carried = answer("* | table id, f | where id > 1 | sort -id", &fx);
    assert_eq!(carried.type_of("f"), "VARCHAR");
    assert_eq!(carried.column("id"), [Some("3"), Some("2")]);

    let present = answer("* | table id, host", &fx);
    assert_eq!(present.type_of("host"), "VARCHAR");
    assert_eq!(present.type_of("id"), "BIGINT");
}

// ---------------------------------------------------------------------------
// targets and alias capture
// ---------------------------------------------------------------------------

/// `rename`, regex `extract` and `eventstats` write their targets, NULL
/// when the source is absent, overwriting a stale target.
#[test]
fn targets_overwrite_a_stale_column_with_null() {
    let fx = fixture();
    let renamed = answer("* | rename f as host", &fx);
    assert_eq!(renamed.column("host"), [None, None, None]);

    let extracted = answer(r#"* | extract "(?P<host>web[0-9])" from f"#, &fx);
    assert_eq!(extracted.column("host"), [None, None, None]);
    assert_eq!(extracted.type_of("host"), "VARCHAR");

    let windowed = answer("* | eventstats sum(f) as host", &fx);
    assert_eq!(windowed.column("host"), [None, None, None]);
}

/// The alias-capture defect (ADR-0062): with `f` absent, a search `!=`
/// must not bind a same-SELECT alias named `f`.
#[test]
fn alias_capture_cannot_happen() {
    let fx = fixture();
    assert_eq!(ids("f!=web1 | rename host as f", &fx), [1, 2, 3]);
    assert_eq!(ids("f!=x | eventstats count() as f", &fx), [1, 2, 3]);
    let stats = answer("f!=x | stats count() as f", &fx);
    assert_eq!(stats.column("f"), [Some("3")]);
}

// ---------------------------------------------------------------------------
// text search
// ---------------------------------------------------------------------------

/// Text search reads `message` and `_raw` through the resolver: a source
/// without either answers positive, negated and `OR` searches.
#[test]
fn text_search_over_a_source_without_message_or_raw() {
    let bare = fixture_from(&format!(
        "SELECT id, _time, _ingested, host FROM ({EVENTS})"
    ));
    assert_eq!(ids("boom", &bare), Vec::<i64>::new());
    assert_eq!(ids("-boom", &bare), [1, 2, 3]);
    assert_eq!(ids("boom OR host=web2", &bare), [2]);
    assert_eq!(ids(r#""boom one" f!=x"#, &bare), Vec::<i64>::new());

    let no_raw = fixture_from(&format!("SELECT * EXCLUDE (_raw) FROM ({EVENTS})"));
    assert_eq!(ids("boom", &no_raw), [1, 3]);
    assert_eq!(ids("-boom f!=x", &no_raw), [2]);

    let no_message = fixture_from(&format!("SELECT * EXCLUDE (message) FROM ({EVENTS})"));
    assert_eq!(ids("boom", &no_message), [1, 3], "_raw alone still matches");
}

// ---------------------------------------------------------------------------
// the envelope timestamps
// ---------------------------------------------------------------------------

/// A source without `_time` or `_ingested` is foreign or broken: a read
/// of either is refused loudly, never answered as NULL.
#[test]
fn an_envelope_timestamp_absent_from_the_source_is_refused() {
    let no_time = fixture_from(&format!("SELECT * EXCLUDE (_time) FROM ({EVENTS})"));
    for dsl in [
        "last=1h host=web1",
        "* | sort _time",
        "* | tail 2",
        "* | where _time > now()",
        "* | timechart count()",
        "* | dedup host",
        "f=x | stats max(_time) as t",
    ] {
        let err = emit_probed(dsl, &no_time, &FieldTypes::new())
            .expect_err(&format!("{dsl} must be refused"));
        assert!(err.to_string().contains("_time"), "{dsl}: {err}");
    }

    let no_ingested = fixture_from(&format!("SELECT * EXCLUDE (_ingested) FROM ({EVENTS})"));
    let err = emit_probed(
        "* | table _ingested, host",
        &no_ingested,
        &FieldTypes::new(),
    )
    .expect_err("a read of _ingested is refused");
    assert!(err.to_string().contains("_ingested"), "{err}");

    // A query that never reads the timestamp is answered.
    assert_eq!(
        answer("* | stats count() as c", &no_time).column("c"),
        [Some("3")]
    );
}

/// A stage that removed `_time` leaves it absent like any other field;
/// only the source's lack of it is refused.
#[test]
fn a_timestamp_a_stage_removed_reads_null() {
    let fx = fixture();
    let got = answer("* | stats count() as c by host | tail 5", &fx);
    assert_eq!(got.rows.len(), 2);
}

// ---------------------------------------------------------------------------
// pivot
// ---------------------------------------------------------------------------

/// A read after a dynamic pivot that misses the group-by keys is padded
/// onto the pivot with a zero-row fill: it answers as an all-null column
/// when no event produced it, and keeps a produced column's values.
#[test]
fn a_pivot_miss_is_padded_with_a_null_fill() {
    let fx = fixture();
    let absent = emit_ok("* | pivot count() on host by service | where web3 > 0", &fx);
    assert!(
        absent.sql.contains("UNION ALL BY NAME"),
        "a miss pads the pivot:\n{}",
        absent.sql
    );
    assert!(run(&fx.conn, &absent).expect("runs").rows.is_empty());

    let null_read = answer(
        "* | pivot count() on host by service | where isnull(web3) | sort service",
        &fx,
    );
    assert_eq!(null_read.column("service"), [Some("api"), Some("db")]);
    assert_eq!(null_read.column("web3"), [None, None]);

    let present = answer(
        "* | pivot count() on host by service | where web1 > 0 | sort service",
        &fx,
    );
    assert_eq!(present.column("service"), [Some("api"), Some("db")]);
    assert_eq!(present.column("web1"), [Some("1"), Some("1")]);
    assert_eq!(present.type_of("web1"), "BIGINT");

    let consumed = answer(
        "* | pivot count() on host by service | where isnull(host) | sort service",
        &fx,
    );
    assert_eq!(
        consumed.column("host"),
        [None, None],
        "the pivot consumed host"
    );
}

/// No read misses, no fill: the SQL is today's.
#[test]
fn a_pivot_without_a_miss_takes_no_fill() {
    let fx = fixture();
    for dsl in [
        "* | pivot count() on host by service",
        r#"* | pivot count() on host by service | where service == "db""#,
        "* | pivot count() on host by service | let a = 1, b = a",
        "* | pivot count() on host by service | let a = 1 | where a > 0",
    ] {
        let sql = emit_ok(dsl, &fx).sql;
        assert!(!sql.contains("UNION ALL BY NAME"), "{dsl}:\n{sql}");
    }
}

/// A name a `let` in the post-pivot scope assigns is never padded, so the
/// lateral read binds the alias.
#[test]
fn a_post_pivot_let_reads_its_own_alias() {
    let fx = fixture();
    let got = answer(
        "* | pivot count() on host by service | let a = 1, b = a | sort service",
        &fx,
    );
    assert_eq!(got.column("b"), [Some("1"), Some("1")]);
}

/// Once a stage resets the relation, a miss is a plain NULL again.
#[test]
fn a_reset_after_a_pivot_reads_plain_null() {
    let fx = fixture();
    let got = answer(
        "* | pivot count() on host by service | table service | where isnull(web1) | sort service",
        &fx,
    );
    assert_eq!(got.names(), ["service"]);
    assert_eq!(got.rows.len(), 2);
}

/// Absent pivot inputs read as NULL columns in the narrowing.
#[test]
fn a_pivot_over_absent_inputs_binds() {
    let fx = fixture();
    let got = answer("* | pivot sum(f) on host by g", &fx);
    assert_eq!(got.rows.len(), 1);
    assert_eq!(got.type_of("g"), "VARCHAR");
}

// ---------------------------------------------------------------------------
// timechart on
// ---------------------------------------------------------------------------

/// `timechart on` an absent unpinned field keeps the refusal: its probe
/// reports an untyped NULL, not a timestamp. A TIMESTAMP pin passes and
/// buckets everything as NULL.
#[test]
fn timechart_on_an_absent_field_probes_its_null() {
    let fx = fixture();
    let unpinned = emit_ok("* | timechart on f span=1h count() as c", &fx);
    let check = &unpinned.timechart_input_checks[0];
    assert!(check.sql.starts_with("SELECT NULL FROM"), "{}", check.sql);
    let probe_type = |sql: &str, params: &[SqlValue]| {
        let params = bind(params);
        let refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let mut stmt = fx.conn.prepare(sql).expect("the probe binds");
        let rows = stmt.query(refs.as_slice()).expect("the probe runs");
        rows.as_ref().expect("ran").column_logical_type(0).id()
    };
    assert_ne!(
        probe_type(&check.sql, &check.params),
        duckdb::core::LogicalTypeId::Timestamp,
        "an untyped NULL is no timestamp, so the executor refuses it"
    );

    let pinned = emit_probed(
        "* | timechart on f span=1h count() as c",
        &fx,
        &pins(&[("f", CT::Timestamp)]),
    )
    .expect("emits");
    let check = &pinned.timechart_input_checks[0];
    assert!(
        check.sql.starts_with("SELECT CAST(NULL AS TIMESTAMP) FROM"),
        "{}",
        check.sql
    );
    assert_eq!(
        probe_type(&check.sql, &check.params),
        duckdb::core::LogicalTypeId::Timestamp,
    );
    let got = run(&fx.conn, &pinned).expect("runs");
    assert_eq!(got.column("_time"), [None]);
    assert_eq!(got.column("c"), [Some("3")]);
}

// ---------------------------------------------------------------------------
// saved sources
// ---------------------------------------------------------------------------

/// A saved run that recorded no schema reads through a dummy-column
/// source. The dummy column is hidden: `table n` is zero rows and one
/// VARCHAR column `n`.
#[test]
fn the_zero_column_source_hides_its_dummy_column() {
    let conn = Connection::open_in_memory().expect("duckdb");
    let source = super::ZERO_COLUMN_SOURCE;
    let columns = InputColumns {
        cold: ColdColumns::described(describe(&conn, &format!("SELECT * FROM {source}"))),
        hot: None,
    };
    let query = parser::parse("* | table n").expect("parses");
    let emitted =
        emit_with_pins(&query, source, &FieldTypes::new(), &columns, anchor()).expect("emits");
    let got = run(&conn, &emitted).expect("runs");
    assert_eq!(got.columns, [("n".to_owned(), "VARCHAR".to_owned())]);
    assert!(got.rows.is_empty());

    let query = parser::parse("* | stats count() as c").expect("parses");
    let emitted =
        emit_with_pins(&query, source, &FieldTypes::new(), &columns, anchor()).expect("emits");
    assert_eq!(run(&conn, &emitted).expect("runs").column("c"), [Some("0")]);
}

// ---------------------------------------------------------------------------
// hot null-only keys
// ---------------------------------------------------------------------------

/// A hot snapshot beside the fixture: two events, `k` only ever JSON
/// `null`, `host` real.
fn hot_file(fx: &Fixture) -> String {
    let path = fx.dir.join("hot.ndjson");
    std::fs::write(
        &path,
        "{\"id\":4,\"_time\":\"2026-01-01T00:00:04Z\",\"_ingested\":\"2026-01-01T00:00:04Z\",\"host\":\"web4\",\"k\":null}\n\
         {\"id\":5,\"_time\":\"2026-01-01T00:00:05Z\",\"_ingested\":\"2026-01-01T00:00:05Z\",\"host\":\"web5\",\"k\":null}\n",
    )
    .expect("hot file written");
    path.to_str().expect("utf-8").to_owned()
}

fn hot_columns() -> HotColumns {
    HotColumns::new(["id", "_time", "_ingested", "host"], ["k"])
}

/// R7: an unpinned null-only hot key with a cold carrier of type T reads
/// as `CAST(NULL AS T)`; with no carrier it is excluded and reads absent;
/// a pinned one keeps the guarded conform.
#[test]
fn hot_null_only_keys_conform_to_a_typed_null_or_vanish() {
    let carrier = fixture_from(&format!("SELECT *, CAST(7 AS BIGINT) AS k FROM ({EVENTS})"));
    let hot = hot_file(&carrier);
    let columns = InputColumns {
        hot: Some(hot_columns()),
        ..described(&carrier.conn, &carrier.source)
    };
    let query = parser::parse("k=7").expect("parses");
    let emitted = emit_with_hot_source(
        &query,
        &carrier.source,
        &hot,
        &FieldTypes::new(),
        &FieldTypes::new(),
        &columns,
        anchor(),
    )
    .expect("emits");
    assert!(
        emitted.sql.contains("CAST(NULL AS BIGINT) AS \"k\""),
        "{}",
        emitted.sql
    );
    let got = run(&carrier.conn, &emitted).expect("runs");
    assert_eq!(got.ids(), [1, 2, 3]);
    assert_eq!(
        got.type_of("k"),
        "BIGINT",
        "the hot JSON null no longer widens k"
    );

    let plain = fixture();
    let hot = hot_file(&plain);
    let columns = InputColumns {
        hot: Some(hot_columns()),
        ..described(&plain.conn, &plain.source)
    };
    let query = parser::parse("k!=x").expect("parses");
    let emitted = emit_with_hot_source(
        &query,
        &plain.source,
        &hot,
        &FieldTypes::new(),
        &FieldTypes::new(),
        &columns,
        anchor(),
    )
    .expect("emits");
    assert!(emitted.sql.contains("EXCLUDE (\"k\")"), "{}", emitted.sql);
    let got = run(&plain.conn, &emitted).expect("runs");
    assert_eq!(got.ids(), [1, 2, 3, 4, 5]);
    assert!(
        !got.names().contains(&"k"),
        "an unpinned null-only key reads absent"
    );

    let hot_only = emit_hot_only(
        &parser::parse("k=x").expect("parses"),
        &hot,
        &FieldTypes::new(),
        &FieldTypes::new(),
        &columns.hot_only(),
        anchor(),
    )
    .expect("emits");
    assert!(hot_only.sql.contains("EXCLUDE (\"k\")"), "{}", hot_only.sql);
    assert!(run(&plain.conn, &hot_only).expect("runs").rows.is_empty());

    let pinned = pins(&[("k", CT::BigInt)]);
    let emitted = emit_with_hot_source(
        &parser::parse("k!=7").expect("parses"),
        &plain.source,
        &hot,
        &pinned,
        &pinned,
        &columns,
        anchor(),
    )
    .expect("emits");
    assert!(!emitted.sql.contains("EXCLUDE"), "{}", emitted.sql);
    let got = run(&plain.conn, &emitted).expect("runs");
    assert_eq!(got.ids(), [1, 2, 3, 4, 5]);
    assert_eq!(got.type_of("k"), "BIGINT");
}

// ---------------------------------------------------------------------------
// present fields: no change
// ---------------------------------------------------------------------------

/// A query whose every read binds emits exactly the SQL it emitted before
/// the emitter could know what the input carries.
#[test]
fn present_field_sql_is_byte_identical_to_unprobed() {
    let fx = fixture();
    let probed = described(&fx.conn, &fx.source);
    for dsl in [
        "host=web1",
        "host!=web1 n>5",
        "boom",
        "-boom host=web1",
        r#""boom one""#,
        "last=1h host=web1 OR service=db",
        "* | where n > 1 | table host, n",
        "* | stats count() as c, sum(n) as s by host | sort -c",
        "* | let y = n * 2, z = y + 1 | where z > 3",
        "* | rename host as h | where h == \"web1\"",
        r#"* | extract "(?P<w>[a-z]+)" from message | where w == "boom""#,
        "* | dedup host | tail 2",
        "* | eventstats avg(n) as a by service",
        "* | top 3 host by service",
        "* | timechart span=1h count() by host",
        "* | pivot count() on host by service",
        "* | table host, n | sort n | head 2",
        "* | drop message | fields host, id",
    ] {
        let query = parser::parse(dsl).expect("parses");
        let unprobed = emit_with_pins(
            &query,
            &fx.source,
            &FieldTypes::new(),
            &InputColumns::unprobed(),
            anchor(),
        )
        .expect("emits unprobed");
        let resolved = emit_with_pins(&query, &fx.source, &FieldTypes::new(), &probed, anchor())
            .expect("emits probed");
        assert_eq!(resolved.sql, unprobed.sql, "{dsl}");
        assert_eq!(resolved.params, unprobed.params, "{dsl}");
        run(&fx.conn, &resolved).unwrap_or_else(|e| panic!("{dsl}: {e}"));
    }
}

// ---------------------------------------------------------------------------
// the probe's skip predicate
// ---------------------------------------------------------------------------

/// The probe is skipped only for a query that reads nothing beyond the
/// envelope timestamps: no field, no text search, no default extract
/// (plan R2).
#[test]
fn the_probe_is_skipped_only_for_envelope_reads() {
    let needs = |dsl: &str| needs_input_columns(&parser::parse(dsl).expect("parses"));
    for dsl in [
        "*",
        "last=1h",
        "* | stats count()",
        "* | timechart count()",
        "* | sort -_time | tail 5",
        "* | where _time > now() | dedup",
        "* | timechart on _ingested count()",
        "* | let a = 1",
    ] {
        assert!(!needs(dsl), "{dsl}");
    }
    for dsl in [
        "host=web1",
        "boom",
        "\"boom one\"",
        "-boom",
        "* | where f == 1",
        "* | stats count() by service",
        "* | table host",
        "* | rename f as g",
        r#"* | extract "(?P<w>x)""#,
        "* | extract kv",
        "* | sort host",
        "* | dedup host",
    ] {
        assert!(needs(dsl), "{dsl}");
    }
}

/// A hot key only ever `null` needs the cold description unless a pin
/// conforms it.
#[test]
fn an_unpinned_null_only_hot_key_needs_the_probe() {
    let hot = HotColumns::new(["host", "_time"], ["k"]);
    assert!(hot_needs_input_columns(&hot, &FieldTypes::new()));
    assert!(!hot_needs_input_columns(&hot, &pins(&[("k", CT::BigInt)])));
    assert!(!hot_needs_input_columns(
        &HotColumns::keys_only(["host", "k"]),
        &FieldTypes::new()
    ));
    assert!(!hot_needs_input_columns(
        &HotColumns::new(["host"], ["_time"]),
        &FieldTypes::new()
    ));
}
