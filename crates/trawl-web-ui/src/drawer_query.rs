// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The DSL the service drawer asks for, built where it can be tested.
//!
//! These live outside `components/` — which is wasm32-gated — because the
//! rule they carry is the load-bearing part: a field name the DSL cannot
//! express produces NO QUERY. The empty string is a valid UNFILTERED
//! query, so a fallthrough turns "expand one unnameable field" into a
//! corpus scan.

// The only CALLERS are wasm32-gated components; the rule itself is pure
// and its tests run natively. Same arrangement as `service_card_fmt`.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// The `top 10 <field>` query for one field, or `None` when the DSL cannot
/// name it.
///
/// `None` means NO REQUEST — the empty string is a valid UNFILTERED query,
/// so falling through to it would turn "expand one unnameable field" into
/// a full corpus scan.
pub fn top_values_query(svc: &str, field: &str) -> Option<String> {
    let field = trawl_core::parser::quote_dsl_name(field)?;
    Some(format!(
        r#"service="{}" last=7d | top 10 {field}"#,
        svc.replace('"', "")
    ))
}

/// The per-field cardinality query, or `None` when there is nothing to ask.
///
/// Two name positions per field — the aggregate argument and the `as`
/// target — both through the ONE helper; a name it refuses is dropped
/// rather than breaking the stage for every other field. If that leaves NO
/// aggregates, the stage would be a bare `| stats `, which is not a query:
/// the caller answers empty without asking.
pub fn cardinality_query(svc: &str, fields: &[String]) -> Option<String> {
    let dc_exprs: Vec<String> = fields
        .iter()
        .filter_map(|f| {
            let name = trawl_core::parser::quote_dsl_name(f)?;
            Some(format!("dc({name}) as {name}"))
        })
        .collect();
    if dc_exprs.is_empty() {
        return None;
    }
    Some(format!(
        r#"service="{}" last=7d | stats {}"#,
        svc.replace('"', ""),
        dc_exprs.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::{cardinality_query, top_values_query};

    /// A name the DSL cannot express produces NO query. The empty string
    /// is a valid unfiltered query, so a fallthrough would fire a corpus
    /// scan for a field the user merely expanded.
    #[test]
    fn an_unnameable_field_asks_nothing() {
        assert_eq!(top_values_query("nginx", "a\u{202e}b"), None);
        assert_eq!(top_values_query("nginx", ""), None);
        // …while an ordinary and a quoted name both ask properly
        assert_eq!(
            top_values_query("nginx", "host").as_deref(),
            Some(r#"service="nginx" last=7d | top 10 host"#)
        );
        assert_eq!(
            top_values_query("nginx", "request id").as_deref(),
            Some(r#"service="nginx" last=7d | top 10 `request id`"#)
        );
    }

    /// If every field is refused there are no aggregates, and `| stats `
    /// with an empty tail is not a query worth sending.
    #[test]
    fn a_fully_refused_field_list_asks_nothing() {
        assert_eq!(cardinality_query("nginx", &[]), None);
        assert_eq!(
            cardinality_query("nginx", &["a\u{202e}b".to_string(), String::new()]),
            None
        );
        let q = cardinality_query("nginx", &["host".to_string(), "a\u{202e}b".to_string()])
            .expect("the nameable field still asks");
        assert!(q.contains("dc(host) as host"), "{q}");
        assert!(!q.contains('\u{202e}'), "{q}");
    }
}
