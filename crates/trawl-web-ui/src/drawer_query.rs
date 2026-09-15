// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Testable DSL builders used by the service drawer, and the readers
//! for the answers they provoke.
//!
//! A builder that mints its own output names returns them beside the
//! DSL, because the reader cannot guess them: `top_values_query` carries
//! its count column, and `cardinality_query` carries the field list its
//! positional aliases stand for.
//!
//! This file is also compiled into `tests/e2e_wire_fixture_contract.rs`
//! through a `#[path]` module, so it names `trawl_api` and `trawl_core`
//! directly and never says `crate::`.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// A field's top-values query, and the column its counts arrive under.
///
/// The two travel together because the second is not always `count`: see
/// [`top_values_query`]. A reader that assumed `count` would find no
/// column and render an empty list.
pub struct TopValues {
    pub dsl: String,
    pub count_column: &'static str,
}

/// The alias the collision-avoiding `stats` form counts into. Only
/// reached when the field itself folds to `count`, so any other spelling
/// would serve; [`TopValues::count_column`] is what carries it to the
/// reader.
const TOP_VALUES_ALIAS: &str = "hits";

/// `| top 10 <field>` projects the field beside a `count` column it mints
/// itself, so a field whose name folds to `count` would name one output
/// column twice, which the projection-collision check refuses (ADR-0013
/// ruling 8). The stats form asks the same question under an alias that
/// cannot collide, and is used only for the colliding name so every other
/// field keeps the cheaper stage.
pub fn top_values_query(svc: &str, field: &str) -> Option<TopValues> {
    let rendered = trawl_core::parser::suggest::quote_dsl_field(field)?;
    let svc = svc.replace('"', "");
    if trawl_core::schema::catalog_key(field) == "count" {
        return Some(TopValues {
            dsl: format!(
                r#"service="{svc}" last=7d | stats count() as {TOP_VALUES_ALIAS} by {rendered} | sort -{TOP_VALUES_ALIAS} | head 10"#
            ),
            count_column: TOP_VALUES_ALIAS,
        });
    }
    Some(TopValues {
        dsl: format!(r#"service="{svc}" last=7d | top 10 {rendered}"#),
        count_column: "count",
    })
}

/// A service's cardinality query, and the fields its columns answer for,
/// in the order they were emitted.
///
/// The two travel together because the aliases are positional: see
/// [`cardinality_query`]. Only [`decode_cardinality`] can read the
/// answer, and only with this list.
pub struct Cardinality {
    pub dsl: String,
    pub fields: Vec<String>,
}

/// The alias of the i-th emitted expression. [`cardinality_query`]
/// mints it and [`decode_cardinality`] demands it back, so the two read
/// it from here and cannot drift apart.
fn alias(i: usize) -> String {
    format!("c{i}")
}

/// `dc(<field>) as c<i>`, one per field the renderer can spell.
///
/// The alias is the field's POSITION, never its name. Every service
/// schema leads with `_time`, and the parser refuses a reserved name as
/// an aggregation alias (`parser::pipe::assignment_target`), so a
/// name-shaped alias failed the whole statement and cost the service its
/// cardinality for every field. `c<i>` can be neither reserved nor a
/// collision with `count`.
///
/// `i` counts EMITTED expressions, not input fields: a name
/// `quote_dsl_field` cannot render is skipped here and absent from
/// [`Cardinality::fields`], so `fields[i]` is the source name of the
/// i-th column of the response.
pub fn cardinality_query(svc: &str, fields: &[String]) -> Option<Cardinality> {
    let mut emitted: Vec<String> = Vec::with_capacity(fields.len());
    let expressions: Vec<String> = fields
        .iter()
        .filter_map(|field| {
            let rendered = trawl_core::parser::suggest::quote_dsl_field(field)?;
            let alias = alias(emitted.len());
            emitted.push(field.clone());
            Some(format!("dc({rendered}) as {alias}"))
        })
        .collect();
    if expressions.is_empty() {
        return None;
    }
    Some(Cardinality {
        dsl: format!(
            r#"service="{}" last=7d | stats {}"#,
            svc.replace('"', ""),
            expressions.join(", ")
        ),
        fields: emitted,
    })
}

/// Read a cardinality response back onto its field names BY POSITION.
///
/// The column names say nothing about the fields — they are the
/// positional aliases [`cardinality_query`] minted — but they are still
/// checked: the i-th column must be named [`alias`]`(i)`, so a response
/// whose columns were reordered or came from some other query cannot be
/// zipped onto this field list. `fields` must be the list that came back
/// beside the DSL.
///
/// A response that does not answer this field list — a different column
/// count, a column under an unexpected name, or a first row with a
/// different number of cells — yields an empty map rather than a partial
/// one, because zipping a mismatched response would key counts onto the
/// wrong fields. A cell no count can be read from (a null) is skipped,
/// and that field renders as unknown rather than as zero.
pub fn decode_cardinality(
    resp: &trawl_api::QueryResponse,
    fields: &[String],
) -> std::collections::HashMap<String, u64> {
    let mut out = std::collections::HashMap::new();
    if resp.result.columns.len() != fields.len() {
        return out;
    }
    if resp
        .result
        .columns
        .iter()
        .enumerate()
        .any(|(i, column)| column.name != alias(i))
    {
        return out;
    }
    let Some(row) = resp.result.rows.first() else {
        return out;
    };
    if row.len() != fields.len() {
        return out;
    }
    for (field, val) in fields.iter().zip(row.iter()) {
        if let Some(n) = value_as_u64(val) {
            out.insert(field.clone(), n);
        }
    }
    out
}

/// The counts on the wire are whatever JSON the engine emitted for an
/// aggregate, so a count can arrive as an integer, a float, or a string.
/// A negative or unreadable one is no count at all.
pub fn value_as_u64(v: &trawl_api::value::Value) -> Option<u64> {
    use trawl_api::value::Value;
    match v {
        #[allow(clippy::cast_sign_loss)]
        Value::Integer(i) if *i >= 0 => Some(*i as u64),
        Value::Float(f) if *f >= 0.0 =>
        {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(*f as u64)
        }
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top_dsl(svc: &str, field: &str) -> Option<String> {
        top_values_query(svc, field).map(|t| t.dsl)
    }

    /// Every query this module composes has to survive the door it is
    /// posted to: the parser and the pipeline validation `/api/v1/query`
    /// runs before emitting.
    fn assert_accepted(dsl: &str) {
        let query = trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
        trawl_core::emitter::validate_pipeline(&query.pipeline)
            .unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
    }

    #[test]
    fn unnameable_fields_issue_no_query() {
        assert!(top_values_query("nginx", "").is_none());
        assert!(top_values_query("nginx", "a\u{202e}b").is_none());
        assert!(cardinality_query("nginx", &[String::new()]).is_none());
    }

    /// `top` mints its own `count` column, so a field of that name would
    /// project two columns of one name — a 400 from the collision check,
    /// for a drill-in the drawer offers on any field. The fallback says
    /// the same thing with an alias, and the counts still arrive under a
    /// column the reader is told about.
    #[test]
    fn a_field_named_count_gets_a_non_colliding_query() {
        for name in ["count", "COUNT", "Count"] {
            let top = top_values_query("nginx", name).expect("a nameable field");
            assert_eq!(top.count_column, "hits", "{name}");
            assert!(top.dsl.contains("stats count() as hits by"), "{}", top.dsl);
            assert_accepted(&top.dsl);
        }

        // …and every other field keeps the cheaper stage.
        let top = top_values_query("nginx", "host").expect("a nameable field");
        assert_eq!(top.count_column, "count");
        assert_eq!(top.dsl, r#"service="nginx" last=7d | top 10 host"#);
        assert_accepted(&top.dsl);
    }

    /// The names only backticks can spell go through both branches.
    #[test]
    fn composed_queries_are_accepted_for_hostile_names() {
        for name in ["request id", "http-status", "a#b", "a`b", "count", "top"] {
            let top = top_values_query("nginx", name).expect("a nameable field");
            assert_accepted(&top.dsl);
            let card = cardinality_query("nginx", &[name.to_string()]).expect("nameable");
            assert_accepted(&card.dsl);
        }
    }

    /// A response with the builder's field list. Column NAMES are
    /// deliberately not the field names: they are the positional
    /// aliases, and the decoder demands exactly those.
    fn response(
        columns: &[&str],
        rows: Vec<Vec<trawl_api::value::Value>>,
    ) -> trawl_api::QueryResponse {
        trawl_api::QueryResponse {
            execution: None,
            result: trawl_api::value::QueryResult {
                columns: columns
                    .iter()
                    .map(|name| trawl_api::value::Column {
                        name: (*name).to_string(),
                    })
                    .collect(),
                rows,
            },
            truncated: false,
            pagination: trawl_api::PaginationMeta {
                limit: 50,
                offset: 0,
                returned: 1,
            },
            degraded_fields: Vec::new(),
            severity_columns: Vec::new(),
        }
    }

    /// The defect this module was rewritten for: every service schema
    /// leads with `_time`, and the parser refuses a reserved name as an
    /// aggregation alias, so one reserved column used to fail the whole
    /// statement and the service lost cardinality for all of its fields.
    #[test]
    fn reserved_fields_are_aliased_and_accepted() {
        let fields: Vec<String> = [
            "_time",
            "_severity",
            "_producer",
            "host",
            "request id",
            "count",
            "c0",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let q = cardinality_query("nginx", &fields).expect("every name is renderable");
        assert_eq!(q.fields, fields);
        assert_accepted(&q.dsl);
    }

    #[test]
    fn aliases_are_positional_and_never_the_field_name() {
        let fields: Vec<String> = ["_time", "a\u{202e}b", "host"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let q = cardinality_query("nginx", &fields).expect("two names are renderable");
        // The unrenderable name in the middle is skipped, and the
        // aliases stay dense, so `fields[i]` names the i-th expression.
        assert_eq!(q.fields, ["_time".to_string(), "host".to_string()]);
        assert_eq!(
            q.dsl,
            r#"service="nginx" last=7d | stats dc(_time) as c0, dc(host) as c1"#
        );
        for (i, field) in q.fields.iter().enumerate() {
            assert!(q.dsl.contains(&format!("as c{i}")), "{}", q.dsl);
            assert!(!q.dsl.contains(&format!("as {field}")), "{}", q.dsl);
        }
        assert!(!q.dsl.contains('\u{202e}'), "{}", q.dsl);
    }

    #[test]
    fn cardinality_decodes_by_position() {
        use trawl_api::value::Value;

        let fields: Vec<String> = ["_time", "status", "duration"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();

        // The builder's aliases, and a null cell: the null field is
        // absent from the map rather than counted as zero.
        let map = decode_cardinality(
            &response(
                &["c0", "c1", "c2"],
                vec![vec![Value::Integer(1200), Value::Integer(5), Value::Null]],
            ),
            &fields,
        );
        assert_eq!(map.get("_time"), Some(&1200));
        assert_eq!(map.get("status"), Some(&5));
        assert_eq!(map.get("duration"), None);

        // A response that does not answer this field list yields nothing
        // at all: a partial map would key counts onto the wrong fields,
        // and so would a right-width response under other names.
        let reordered = response(
            &["c1", "c0", "c2"],
            vec![vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
            ]],
        );
        let garbage_names = response(
            &["x", "y", "z"],
            vec![vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
            ]],
        );
        let too_many = response(
            &["c0", "c1", "c2", "c3"],
            vec![vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
                Value::Integer(4),
            ]],
        );
        let too_few = response(
            &["c0", "c1"],
            vec![vec![Value::Integer(1), Value::Integer(2)]],
        );
        let short_row = response(
            &["c0", "c1", "c2"],
            vec![vec![Value::Integer(1), Value::Integer(2)]],
        );
        let no_rows = response(&["c0", "c1", "c2"], Vec::new());
        for resp in [
            &reordered,
            &garbage_names,
            &too_many,
            &too_few,
            &short_row,
            &no_rows,
        ] {
            assert!(decode_cardinality(resp, &fields).is_empty());
        }
    }

    #[test]
    fn every_name_position_uses_the_shared_renderer() {
        assert_eq!(
            top_dsl("nginx", "request id").as_deref(),
            Some(r#"service="nginx" last=7d | top 10 `request id`"#)
        );
        let q = cardinality_query(
            "nginx",
            &[
                "host".to_string(),
                "request id".to_string(),
                "a\u{202e}b".to_string(),
            ],
        )
        .expect("two fields are representable");
        let query = q.dsl;
        // The renderer spells the ARGUMENT; the alias is positional.
        assert!(query.contains("dc(host) as c0"), "{query}");
        assert!(query.contains("dc(`request id`) as c1"), "{query}");
        assert!(!query.contains('\u{202e}'), "{query}");
    }
}
