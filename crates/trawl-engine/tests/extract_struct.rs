// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An `extract` stage binds its regex once (ADR-0063, human ruling
//! 2026-11-04): `DuckDB`'s struct form of `regexp_extract` reads every
//! named group from one RE2.
//!
//! The premise tests pin what the bundled `DuckDB` does with that form,
//! straight in SQL, prepared and then bound as the engine runs a query:
//! how it maps names to groups, what a group that takes no part in the
//! match reads, and when the bound pattern becomes a constant. The
//! parity tests run what the emitter writes for an `extract` against the
//! live lane, which reads each group by name.
//!
//! The struct form needs a constant pattern at bind. The engine prepares
//! a statement before it binds the values, and a bound `?` is no constant
//! then, so `regexp_extract(s, ?, ['g1', ...])` is a binder error at
//! prepare. With the names list bound as a parameter too,
//! `regexp_extract(s, ?, ?::VARCHAR[])`, the binder defers the call until
//! the values are bound, and by then the pattern is a constant.

use duckdb::Connection;
use trawl_core::eval::EvalValue;
use trawl_core::schema::FieldTypes;
use trawl_engine::executor::Executor;
use trawl_engine::value::Value;

/// `[g1, ..., gn]`: the struct field names the emitter generates, as the
/// text the names parameter binds.
fn names(n: usize) -> String {
    let list: Vec<String> = (1..=n).map(|i| format!("g{i}")).collect();
    format!("[{}]", list.join(", "))
}

/// Each field of the struct `regexp_extract(subject, pattern, names(n))`
/// returns, read in order, with the pattern and the names bound as
/// parameters, as the emitter binds them.
fn fields(subject: Option<&str>, pattern: &str, n: usize) -> Vec<Option<String>> {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let reads: Vec<String> = (1..=n).map(|i| format!("x.g{i}")).collect();
    let sql = format!(
        "SELECT {} FROM (SELECT regexp_extract(?::VARCHAR, ?, ?::VARCHAR[]) AS x)",
        reads.join(", ")
    );
    let mut stmt = conn.prepare(&sql).expect("the struct form prepares");
    stmt.query_row(duckdb::params![subject, pattern, names(n)], |row| {
        (0..n).map(|i| row.get::<_, Option<String>>(i)).collect()
    })
    .expect("the struct form runs")
}

fn some(texts: &[&str]) -> Vec<Option<String>> {
    texts.iter().map(|t| Some((*t).to_owned())).collect()
}

/// With the names written as a literal list, a bound pattern is refused
/// at prepare: the struct form wants a constant pattern, and a `?` is
/// none before its value is bound.
#[test]
fn a_literal_names_list_refuses_a_bound_pattern() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let error = conn
        .prepare("SELECT regexp_extract('ab', ?, ['g1'])")
        .map(|_| ())
        .expect_err("a literal names list needs a constant pattern");
    assert!(
        error
            .to_string()
            .contains("with LIST of group names requires a constant pattern"),
        "{error}"
    );
}

/// With the names bound too, the pattern is a constant by the time the
/// call binds: an invalid pattern fails at bind over zero rows, before
/// any row could compile it. (A pattern that only rows compile would
/// pass over zero rows.)
#[test]
fn bound_names_make_the_pattern_a_constant() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let error = conn
        .prepare(
            "SELECT regexp_extract(s, ?, ?::VARCHAR[]) \
             FROM (SELECT 'a' AS s WHERE false)",
        )
        .and_then(|mut stmt| {
            stmt.query(duckdb::params!["((?P<y>b)", names(1)])
                .map(|_| ())
        })
        .expect_err("an invalid constant pattern fails at bind");
    assert!(
        error.to_string().contains("Pattern failed to parse"),
        "{error}"
    );
}

/// The struct form takes 40 names, past the scalar form's 0 to 9, and
/// field k is the k-th capture group's text.
#[test]
fn struct_form_reads_forty_groups() {
    let letters: Vec<String> = (0..40)
        .map(|i| {
            char::from_u32(0x4E00 + i)
                .expect("a CJK ideograph")
                .to_string()
        })
        .collect();
    let pattern = letters
        .iter()
        .enumerate()
        .fold(String::new(), |body, (i, l)| {
            body + "(?P<name" + &i.to_string() + ">" + l + ")"
        });
    let subject: String = letters.concat();
    let want: Vec<&str> = letters.iter().map(String::as_str).collect();
    assert_eq!(fields(Some(&subject), &pattern, 40), some(&want));
}

/// `DuckDB` maps the k-th name to the k-th capture group by position
/// among every capture group, named or not, never by the group's own
/// name. So the batch body must make its unnamed groups non-capturing, as
/// `trawl_core::pattern::batch_extract` does, for the k-th name to read
/// the k-th named group.
#[test]
fn names_map_to_groups_by_position() {
    // An unnamed group ahead of the named one takes the first name.
    assert_eq!(fields(Some("ab"), "(a)(?P<y>b)", 1), some(&["a"]));
    // The group's own name plays no part.
    assert_eq!(fields(Some("ab"), "(?P<y>a)(?P<x>b)", 2), some(&["a", "b"]));
    // Made non-capturing, the unnamed group drops out of the numbering.
    assert_eq!(fields(Some("ab"), "(?:a)(?P<y>b)", 1), some(&["b"]));
    // Fewer names than groups reads the first groups only.
    assert_eq!(
        fields(Some("abc"), "(?P<x>a)(?P<y>b)(?P<z>c)", 2),
        some(&["a", "b"])
    );
}

/// More names than capture groups is a bind error, never a silent empty
/// field.
#[test]
fn more_names_than_groups_is_refused() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let error = conn
        .prepare("SELECT regexp_extract('ab', ?, ?::VARCHAR[])")
        .and_then(|mut stmt| stmt.query(duckdb::params!["(a)", names(2)]).map(|_| ()))
        .expect_err("two names, one group");
    assert!(
        error.to_string().contains("Not enough capturing groups"),
        "{error}"
    );
}

/// A group that takes no part in the match reads `''`, and so does every
/// group of a row the pattern does not match, so `nullif(.., '')` reads
/// NULL for both, as the live lane does. A NULL subject gives a NULL
/// struct, whose fields read NULL.
#[test]
fn unmatched_groups_read_empty() {
    let pattern = "(?:a)(?P<x>b)|(?:c)(?P<y>d)";
    assert_eq!(
        fields(Some("cd"), pattern, 2),
        vec![Some(String::new()), Some("d".to_owned())]
    );
    assert_eq!(
        fields(Some("zz"), pattern, 2),
        vec![Some(String::new()), Some(String::new())]
    );
    assert_eq!(fields(None, pattern, 2), vec![None, None]);
}

// ---------------------------------------------------------------------
// Parity with the live lane
// ---------------------------------------------------------------------

/// A parquet file of one row a message, with an `id` column to match
/// rows across lanes by, under `dir`.
fn fixture(dir: &std::path::Path, messages: &[&str]) -> String {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    conn.execute_batch("CREATE TABLE t (_time TIMESTAMP, id BIGINT, message VARCHAR)")
        .expect("create the fixture table");
    {
        let mut app = conn.appender("t").expect("appender");
        for (i, message) in messages.iter().enumerate() {
            let id = i64::try_from(i).expect("a small index");
            let time = duckdb::types::Value::Timestamp(
                duckdb::types::TimeUnit::Microsecond,
                1_767_225_600_000_000 + id,
            );
            app.append_row(duckdb::params![time, id, message])
                .expect("append a row");
        }
    }
    let path = dir.join("extract.parquet");
    conn.execute_batch(&format!("COPY t TO '{}' (FORMAT parquet)", path.display()))
        .expect("write the fixture");
    path.display().to_string()
}

/// What the batch lane answers for `dsl` over `source`: each row's `id`
/// and the text of each of `names`, `None` for NULL.
fn batch_rows(dsl: &str, source: &str, names: &[String]) -> Vec<(i64, Vec<Option<String>>)> {
    let result = Executor::new()
        .expect("an executor")
        .run_query(dsl, source, &FieldTypes::new(), usize::MAX, 0)
        .unwrap_or_else(|e| panic!("{dsl}: {e}"));
    let column = |name: &str| {
        result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("{dsl} has no {name} column: {:?}", result.columns))
    };
    let id = column("id");
    let cells: Vec<usize> = names.iter().map(|n| column(n)).collect();
    let mut rows: Vec<(i64, Vec<Option<String>>)> = result
        .rows
        .iter()
        .map(|row| {
            let Value::Integer(id) = row[id] else {
                panic!("id is an integer, got {:?}", row[id]);
            };
            let texts = cells
                .iter()
                .map(|&c| match &row[c] {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    other => panic!("{dsl}: a capture is text, got {other:?}"),
                })
                .collect();
            (id, texts)
        })
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows
}

/// What the live lane answers for `dsl` over the same messages: each
/// emitted event's `id` and the text of each of `names`.
fn live_rows(dsl: &str, messages: &[&str], names: &[String]) -> Vec<(i64, Vec<Option<String>>)> {
    let query = trawl_core::parser::parse(dsl).expect("the live query parses");
    let filter = trawl_core::filter::CompiledFilter::compile(&query.search, &FieldTypes::new())
        .expect("the live filter compiles");
    let plan = trawl_core::stream::compile_stream_plan(
        &query.pipeline,
        &trawl_core::pin_scope::PinScope::unpinned(),
        None,
    )
    .expect("the live plan compiles");
    let trawl_core::stream::StreamPlan::PassThrough(mut stages) = plan else {
        panic!("{dsl} has no aggregation, so it streams row by row");
    };
    let mut rows = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        let id = i64::try_from(i).expect("a small index");
        let event = serde_json::json!({ "id": id, "message": message });
        let outcome = trawl_core::stream::accept_event(
            &filter,
            &mut stages,
            event.as_object().expect("an event is a JSON object"),
            &trawl_core::context::EvalContext::capture(),
        )
        .expect("the live plan holds every pattern it reads");
        if let trawl_core::stream::LiveOutcome::Emit(row) = outcome {
            let texts = names
                .iter()
                .map(|n| match &row[n.as_str()] {
                    EvalValue::Null => None,
                    cell => Some(trawl_core::row::cell_text(cell)),
                })
                .collect();
            rows.push((id, texts));
        }
    }
    rows
}

/// Both lanes answer `dsl` over `messages` alike, and the batch lane
/// binds one regex for the extract: one `regexp_extract` call in the SQL
/// and one extract site.
fn assert_parity(
    dsl: &str,
    messages: &[&str],
    names: &[String],
) -> Vec<(i64, Vec<Option<String>>)> {
    let dir = tempfile::tempdir().expect("a fixture dir");
    let source = fixture(dir.path(), messages);
    let emitted = trawl_core::emitter::emit(
        &trawl_core::parser::parse(dsl).expect("the query parses"),
        &source,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("the query emits");
    assert_eq!(
        emitted.sql.matches("regexp_extract(").count(),
        1,
        "{}",
        emitted.sql
    );
    assert_eq!(emitted.regex_sites.extract, 1, "{dsl}");
    let batch = batch_rows(dsl, &source, names);
    assert_eq!(batch, live_rows(dsl, messages, names), "{dsl}");
    batch
}

/// A 40-group extract reads every group alike in both lanes: on a row
/// that matches with every group taking part, on rows where one branch
/// of an alternation leaves its group out (NULL), and on a row the
/// pattern does not match (every group NULL). A capture group is one of
/// a body's 64 positions, so 18 groups hold a letter, two sit in an
/// alternation, and 20 are empty, which reads NULL in both lanes.
#[test]
fn forty_groups_agree_with_the_live_lane() {
    let letters: Vec<String> = (1..=18u8)
        .map(|i| format!("(?P<n{i:02}>{})", char::from(b'a' + i - 1)))
        .collect();
    let empty: Vec<String> = (21..=40).map(|i| format!("(?P<n{i:02}>)")).collect();
    let body = format!(
        "{}(?:(?P<n19>x)|(?P<n20>y)){}",
        letters.concat(),
        empty.concat()
    );
    let names: Vec<String> = (1..=40).map(|i| format!("n{i:02}")).collect();
    let run = "abcdefghijklmnopqr";
    let (with_x, with_y) = (format!("{run}x"), format!("--{run}y--"));
    let messages = [with_x.as_str(), with_y.as_str(), "no match here"];
    let rows = assert_parity(&format!(r#"* | extract "{body}""#), &messages, &names);

    let letter = |i: u8| Some(char::from(b'a' + i).to_string());
    let expect = |nineteen: Option<&str>, twenty: Option<&str>| -> Vec<Option<String>> {
        (0..18)
            .map(letter)
            .chain([nineteen.map(str::to_owned), twenty.map(str::to_owned)])
            .chain(std::iter::repeat_n(None, 20))
            .collect()
    };
    assert_eq!(
        rows,
        vec![
            (0, expect(Some("x"), None)),
            (1, expect(None, Some("y"))),
            (2, vec![None; 40]),
        ]
    );
}

/// Unnamed groups between named ones do not shift what a name reads: the
/// batch body makes them non-capturing, so each named group is the
/// struct field at its place among the named ones.
#[test]
fn unnamed_groups_between_named_ones_agree() {
    let names = vec!["a".to_owned(), "b".to_owned()];
    let rows = assert_parity(
        r#"* | extract "(\d+)-(?P<a>[a-z]+)(x|y)((z))?(?P<b>\d)""#,
        &["12-foox3", "7-baryz9", "nothing"],
        &names,
    );
    assert_eq!(
        rows,
        vec![
            (0, vec![Some("foo".to_owned()), Some("3".to_owned())]),
            (1, vec![Some("bar".to_owned()), Some("9".to_owned())]),
            (2, vec![None, None]),
        ]
    );
}

/// A `where` on an extracted field reads the field the struct gave it,
/// in both lanes.
#[test]
fn where_after_extract_agrees() {
    let names = vec!["code".to_owned(), "path".to_owned()];
    let rows = assert_parity(
        r#"* | extract "(?P<code>\d{3}) (?P<path>/\S*)" | where code == "500""#,
        &["500 /a", "404 /b", "500 /c", "garbage"],
        &names,
    );
    assert_eq!(
        rows,
        vec![
            (0, vec![Some("500".to_owned()), Some("/a".to_owned())]),
            (2, vec![Some("500".to_owned()), Some("/c".to_owned())]),
        ]
    );
}

/// The export lane wraps the statement in `CREATE TEMP TABLE … AS`, and
/// the deferred bind of the struct form survives that: a 12-group
/// extract exports every group.
#[test]
fn an_extract_exports() {
    let dir = tempfile::tempdir().expect("a fixture dir");
    let source = fixture(dir.path(), &["abcdefghijkl", "nope"]);
    let body = (0..12u8).fold(String::new(), |body, i| {
        let c = char::from(b'a' + i);
        body + "(?P<n" + &i.to_string() + ">" + &c.to_string() + ")"
    });
    let out = dir.path().join("export.parquet");
    Executor::new()
        .expect("an executor")
        .export_parquet(
            &format!(r#"* | extract "{body}" | table id, n0, n11"#),
            &source,
            &FieldTypes::new(),
            &out,
            100,
        )
        .expect("the extract exports");
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let rows: Vec<(i64, Option<String>, Option<String>)> = conn
        .prepare(&format!(
            "SELECT id, n0, n11 FROM read_parquet('{}') ORDER BY id",
            out.display()
        ))
        .expect("the export reads back")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("the rows read")
        .collect::<Result<_, _>>()
        .expect("every row reads");
    assert_eq!(
        rows,
        vec![
            (0, Some("a".to_owned()), Some("l".to_owned())),
            (1, None, None),
        ]
    );
}

/// A pivot inlines every parameter of the statement as a literal, so the
/// struct form binds with a literal pattern and a literal names list
/// there.
#[test]
fn an_extract_feeds_a_pivot() {
    let dir = tempfile::tempdir().expect("a fixture dir");
    let source = fixture(dir.path(), &["k=a v=1", "k=b v=2", "k=a v=3"]);
    let result = Executor::new()
        .expect("an executor")
        .run_query(
            r#"* | extract "k=(?P<k>\w) v=(?P<v>\d)" | pivot count() on k"#,
            &source,
            &FieldTypes::new(),
            usize::MAX,
            0,
        )
        .expect("the pivot runs");
    let column = |name: &str| {
        result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("no {name} column: {:?}", result.columns))
    };
    assert_eq!(result.rows.len(), 1, "{:?}", result.rows);
    assert_eq!(result.rows[0][column("a")], Value::Integer(2));
    assert_eq!(result.rows[0][column("b")], Value::Integer(1));
}
