// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Testable DSL builders used by the service drawer.

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

/// The alias the collision-avoiding form counts into — the same spelling
/// the projection-collision error suggests as the way out.
const TOP_VALUES_ALIAS: &str = "hits";

/// `| top 10 <field>` projects the field beside a `count` column it mints
/// itself, so a field whose name folds to `count` would name one output
/// column twice — refused since the projection-collision check (ADR-0013
/// ruling 8), which made the drawer's drill-in a 400 for exactly that
/// field. The stats form asks the same question with an alias that cannot
/// collide, and is used ONLY for the colliding name so every other field
/// keeps the cheaper stage.
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

pub fn cardinality_query(svc: &str, fields: &[String]) -> Option<String> {
    let expressions: Vec<String> = fields
        .iter()
        .filter_map(|field| {
            let field = trawl_core::parser::suggest::quote_dsl_field(field)?;
            Some(format!("dc({field}) as {field}"))
        })
        .collect();
    if expressions.is_empty() {
        return None;
    }
    Some(format!(
        r#"service="{}" last=7d | stats {}"#,
        svc.replace('"', ""),
        expressions.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top_dsl(svc: &str, field: &str) -> Option<String> {
        top_values_query(svc, field).map(|t| t.dsl)
    }

    /// Every query this module composes has to survive the door it is
    /// posted to: the parser AND the pipeline validation `/api/v1/query`
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
        assert_eq!(cardinality_query("nginx", &[String::new()]), None);
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
            assert_accepted(&card);
        }
    }

    #[test]
    fn every_name_position_uses_the_shared_renderer() {
        assert_eq!(
            top_dsl("nginx", "request id").as_deref(),
            Some(r#"service="nginx" last=7d | top 10 `request id`"#)
        );
        let query = cardinality_query(
            "nginx",
            &[
                "host".to_string(),
                "request id".to_string(),
                "a\u{202e}b".to_string(),
            ],
        )
        .expect("two fields are representable");
        assert!(query.contains("dc(host) as host"), "{query}");
        assert!(
            query.contains("dc(`request id`) as `request id`"),
            "{query}"
        );
        assert!(!query.contains('\u{202e}'), "{query}");
    }
}
