// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A facet on a field whose first dot segment is a field-start keyword
//! keeps its predicate through the merge.
//!
//! Rendered bare, `OR.a="x"` is the search stage's OR separator followed
//! by text, so the filter would vanish from the merged query. The merge
//! renders facet fields through `quote_dsl_field`, which must backtick
//! the name.

use trawl_core::ast::SearchToken;
use trawl_search_url::query_merge::{Filter, FilterOp, RangeSpec, effective_query};

#[test]
fn keyword_prefixed_facet_keeps_its_predicate() {
    for field in ["OR.a", "or.a.b", "NOT.a", "true.a", "null.a"] {
        let filter = Filter {
            field: field.into(),
            value: "x".into(),
            op: FilterOp::Include,
        };
        let merged = effective_query("*", &[filter], &RangeSpec::Quick("15m"));
        let parsed = trawl_core::parser::parse(&merged)
            .unwrap_or_else(|e| panic!("{field:?}: {merged:?} must parse: {e:?}"));
        assert!(
            parsed.search.all_tokens().any(|t| matches!(
                &t.node,
                SearchToken::FieldFilter(f) if f.field == field
            )),
            "{field:?}: {merged:?} lost the field filter"
        );
    }
}
