// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Testable DSL builders used by the service drawer.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

pub fn top_values_query(svc: &str, field: &str) -> Option<String> {
    let field = trawl_core::parser::suggest::quote_dsl_field(field)?;
    Some(format!(
        r#"service="{}" last=7d | top 10 {field}"#,
        svc.replace('"', "")
    ))
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

    #[test]
    fn unnameable_fields_issue_no_query() {
        assert_eq!(top_values_query("nginx", ""), None);
        assert_eq!(top_values_query("nginx", "a\u{202e}b"), None);
        assert_eq!(cardinality_query("nginx", &[String::new()]), None);
    }

    #[test]
    fn every_name_position_uses_the_shared_renderer() {
        assert_eq!(
            top_values_query("nginx", "request id").as_deref(),
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
