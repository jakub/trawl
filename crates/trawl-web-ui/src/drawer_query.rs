// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! This file is also compiled into `tests/e2e_wire_fixture_contract.rs`
//! through a `#[path]` module, so it names `trawl_api` and `trawl_core`
//! directly and never says `crate::`.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

pub struct TopValues {
    pub dsl: String,
    pub count_column: &'static str,
}

const TOP_VALUES_ALIAS: &str = "hits";

/// `| top 10 <field>` projects the field beside a `count` column it mints
/// itself, so a field whose name folds to `count` would name one output
/// column twice, which the projection-collision check refuses (ADR-0013
/// ruling 8).
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

pub struct Cardinality {
    pub dsl: String,
    pub fields: Vec<String>,
}

fn alias(i: usize) -> String {
    format!("c{i}")
}

/// The alias is the field's POSITION, never its name. Every service
/// schema leads with `_time`, and the parser refuses a reserved name as
/// an aggregation alias (`parser::pipe::assignment_target`).
///
/// `i` counts EMITTED expressions, not input fields: a name
/// `quote_dsl_field` cannot render is skipped here and left out of
/// [`Cardinality::fields`], so that list's i-th entry names the i-th
/// column of the response.
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

/// A response whose column count, column names or first-row width do not
/// match this field list yields an empty map rather than a partial one,
/// because zipping it would key counts onto the wrong fields.
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
pub fn value_as_u64(v: &trawl_api::value::Value) -> Option<u64> {
    use trawl_api::value::Value;
    match v {
        #[allow(clippy::cast_sign_loss)]
        Value::Integer(i) if *i >= 0 => Some(*i as u64),
        Value::UInt(u) => Some(*u),
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

    #[test]
    fn a_field_named_count_gets_a_non_colliding_query() {
        for name in ["count", "COUNT", "Count"] {
            let top = top_values_query("nginx", name).expect("a nameable field");
            assert_eq!(top.count_column, "hits", "{name}");
            assert!(top.dsl.contains("stats count() as hits by"), "{}", top.dsl);
            assert_accepted(&top.dsl);
        }

        let top = top_values_query("nginx", "host").expect("a nameable field");
        assert_eq!(top.count_column, "count");
        assert_eq!(top.dsl, r#"service="nginx" last=7d | top 10 host"#);
        assert_accepted(&top.dsl);
    }

    #[test]
    fn composed_queries_are_accepted_for_hostile_names() {
        for name in ["request id", "http-status", "a#b", "a`b", "count", "top"] {
            let top = top_values_query("nginx", name).expect("a nameable field");
            assert_accepted(&top.dsl);
            let card = cardinality_query("nginx", &[name.to_string()]).expect("nameable");
            assert_accepted(&card.dsl);
        }
    }

    fn response(
        columns: &[&str],
        rows: Vec<Vec<trawl_api::value::Value>>,
    ) -> trawl_api::QueryResponse {
        trawl_api::QueryResponse {
            execution: None,
            result: trawl_api::value::QueryResult {
                columns: columns
                    .iter()
                    .enumerate()
                    .map(|(idx, name)| {
                        let mut fold = trawl_api::value::TypeFold::new();
                        for row in &rows {
                            if let Some(cell) = row.get(idx) {
                                fold.observe_value(cell);
                            }
                        }
                        trawl_api::value::Column::new(
                            *name,
                            fold.finish()
                                .unwrap_or(trawl_api::value::ColumnType::String),
                        )
                    })
                    .collect(),
                rows,
            },
            pagination: trawl_api::PaginationMeta {
                limit: 50,
                offset: 0,
                returned: 1,
                total: 1,
            },
            degraded_fields: Vec::new(),
            severity_columns: Vec::new(),
            histogram: None,
        }
    }

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
        assert!(query.contains("dc(host) as c0"), "{query}");
        assert!(query.contains("dc(`request id`) as c1"), "{query}");
        assert!(!query.contains('\u{202e}'), "{query}");
    }
}
