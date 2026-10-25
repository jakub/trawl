// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every tool schema passes Claude Code's rules (ADR-0058, #308 AC7):
//! property names in `[A-Za-z0-9_.-]` of 1 to 64 characters, valid JSON
//! Schema 2020-12, an object root with no `anyOf` or `oneOf`, names under
//! 128 characters, and the annotations exactly as specified.

use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{Value, json};
use trawl_mcp::reply::{
    Bounds, DslEffects, Link, Presentation, ResultView, SearchMode, SearchReply,
};
use trawl_mcp::tools::schema::tools;

fn schema_value(schema: &serde_json::Map<String, Value>) -> Value {
    Value::Object(schema.clone())
}

fn schemas(tool: &Tool) -> Vec<(&'static str, Value)> {
    let mut out = vec![("input", schema_value(&tool.input_schema))];
    if let Some(output) = &tool.output_schema {
        out.push(("output", schema_value(output)));
    }
    out
}

/// Every property name under every `properties` keyword, recursively.
fn property_names(schema: &Value, names: &mut Vec<String>) {
    match schema {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "properties"
                    && let Value::Object(properties) = value
                {
                    names.extend(properties.keys().cloned());
                }
                property_names(value, names);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| property_names(item, names)),
        _ => {}
    }
}

fn is_allowed_property_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

#[test]
fn tools_list_is_orient_search_read_fields_investigation_in_that_order() {
    let names: Vec<_> = tools().iter().map(|tool| tool.name.to_string()).collect();
    assert_eq!(
        names,
        ["orient", "search", "read", "fields", "investigation"]
    );
}

#[test]
fn every_tool_has_an_input_and_an_output_schema() {
    for tool in tools() {
        assert!(
            tool.output_schema.is_some(),
            "{} has no output schema",
            tool.name
        );
        assert!(
            tool.description.as_deref().is_some_and(|d| !d.is_empty()),
            "{} has no description",
            tool.name
        );
    }
}

#[test]
fn tool_names_are_under_128_characters_and_in_the_name_alphabet() {
    for tool in tools() {
        assert!(tool.name.len() < 128, "{} is too long", tool.name);
        assert!(
            is_allowed_property_name(&tool.name),
            "{} has a bad character",
            tool.name
        );
    }
}

#[test]
fn every_schema_root_is_an_object_without_any_of_or_one_of() {
    for tool in tools() {
        for (kind, schema) in schemas(&tool) {
            assert_eq!(schema["type"], json!("object"), "{} {kind} root", tool.name);
            for keyword in ["anyOf", "oneOf", "allOf", "not", "if"] {
                assert!(
                    schema.get(keyword).is_none(),
                    "{} {kind} schema has a root {keyword}",
                    tool.name
                );
            }
        }
    }
}

#[test]
fn the_property_walk_reaches_nested_definitions() {
    let search = tools().into_iter().find(|t| t.name == "search").unwrap();
    let mut names = Vec::new();
    property_names(
        &schema_value(search.output_schema.as_ref().unwrap()),
        &mut names,
    );
    for nested in [
        "rows_produced",
        "cells_abbreviated",
        "total_bytes",
        "deduplicated",
    ] {
        assert!(
            names.iter().any(|n| n == nested),
            "the walk missed {nested}"
        );
    }
    assert!(!is_allowed_property_name("bad name"));
    assert!(!is_allowed_property_name(""));
    assert!(!is_allowed_property_name(&"a".repeat(65)));
}

#[test]
fn every_property_name_is_in_the_allowed_alphabet() {
    for tool in tools() {
        for (kind, schema) in schemas(&tool) {
            let mut names = Vec::new();
            property_names(&schema, &mut names);
            assert!(
                !names.is_empty() || kind == "input",
                "{} {kind} has no properties",
                tool.name
            );
            for name in names {
                assert!(
                    is_allowed_property_name(&name),
                    "{} {kind} schema property {name:?} leaves [A-Za-z0-9_.-]{{1,64}}",
                    tool.name
                );
            }
        }
    }
}

#[test]
fn every_schema_is_valid_json_schema_2020_12() {
    for tool in tools() {
        for (kind, schema) in schemas(&tool) {
            assert_eq!(
                schema["$schema"],
                json!("https://json-schema.org/draft/2020-12/schema"),
                "{} {kind} declares its draft",
                tool.name
            );
            if let Err(error) = jsonschema::draft202012::meta::validate(&schema) {
                panic!(
                    "{} {kind} schema fails the 2020-12 meta-schema: {error}",
                    tool.name
                );
            }
            jsonschema::draft202012::new(&schema).unwrap_or_else(|error| {
                panic!("{} {kind} schema does not compile: {error}", tool.name)
            });
        }
    }
}

#[test]
fn annotations_are_exactly_as_specified() {
    let read_only = |title: &str| {
        ToolAnnotations::from_raw(
            Some(title.to_owned()),
            Some(true),
            Some(false),
            Some(true),
            Some(true),
        )
    };
    let expected = [
        ("orient", read_only("Orient on this trawl installation")),
        (
            "search",
            ToolAnnotations::from_raw(
                Some("Search logs".to_owned()),
                Some(false),
                Some(false),
                Some(false),
                Some(true),
            ),
        ),
        ("read", read_only("Read a captured result")),
        ("fields", read_only("List fields")),
        (
            "investigation",
            ToolAnnotations::from_raw(
                Some("Work in an investigation".to_owned()),
                Some(false),
                Some(false),
                Some(false),
                Some(true),
            ),
        ),
    ];
    let tools = tools();
    assert_eq!(tools.len(), expected.len());
    for (tool, (name, annotations)) in tools.iter().zip(expected) {
        assert_eq!(tool.name, name);
        assert_eq!(
            tool.annotations.as_ref(),
            Some(&annotations),
            "{name} annotations"
        );
        assert_eq!(tool.title, annotations.title, "{name} title");
    }
}

fn validator(tool: &str, kind: &str) -> jsonschema::Validator {
    let tool = tools()
        .into_iter()
        .find(|t| t.name == tool)
        .expect("the tool exists");
    let schema = match kind {
        "input" => schema_value(&tool.input_schema),
        _ => schema_value(tool.output_schema.as_ref().expect("an output schema")),
    };
    jsonschema::draft202012::new(&schema).expect("the schema compiles")
}

#[test]
fn input_schemas_accept_flat_calls_and_refuse_out_of_range_values() {
    let search = validator("search", "input");
    assert!(search.is_valid(&json!({"mode": "validate", "query": "last=1h"})));
    assert!(search.is_valid(&json!({
        "mode": "execute", "query": "last=1h", "rows": 200, "max_bytes": 32768
    })));
    for refused in [
        json!({"mode": "run", "query": "x"}),
        json!({"mode": "execute"}),
        json!({"mode": "execute", "query": "x", "rows": 0}),
        json!({"mode": "execute", "query": "x", "rows": 201}),
        json!({"mode": "execute", "query": "x", "max_bytes": 4095}),
        json!({"mode": "execute", "query": "x", "max_bytes": 32769}),
        json!({"mode": "execute", "query": "x", "extra": true}),
    ] {
        assert!(!search.is_valid(&refused), "search accepted {refused}");
    }

    let read = validator("read", "input");
    assert!(read.is_valid(&json!({"action": "page", "result_id": "a", "offset": 50, "rows": 50})));
    assert!(read.is_valid(&json!({"action": "cell", "result_id": "a", "row": 3, "column": 1})));
    assert!(!read.is_valid(&json!({"action": "scroll", "result_id": "a"})));
    assert!(read.is_valid(&json!({
        "action": "investigation", "investigation_id": "a", "since": 4
    })));

    let investigation = validator("investigation", "input");
    for accepted in [
        json!({"action": "create", "title": "t", "question": "q"}),
        json!({"action": "read", "id": "a", "since": 0, "max_bytes": 32768}),
        json!({"action": "list"}),
        json!({"action": "note", "id": "a", "text": "n"}),
        json!({"action": "finding", "id": "a", "statement": "s", "status": "supported",
               "references": [{"result_id": "r", "row": 3}]}),
        json!({"action": "keep", "id": "a", "result_id": "r", "rows": [1, 2], "reason": "x"}),
        json!({"action": "finding_status", "id": "a", "finding_seq": 4, "status": "refuted",
               "expected_seq": 9}),
        json!({"action": "status", "id": "a", "status": "closed", "expected_seq": 9}),
    ] {
        assert!(investigation.is_valid(&accepted), "refused {accepted}");
    }
    for refused in [
        json!({"id": "a"}),
        json!({"action": "delete", "id": "a"}),
        json!({"action": "status", "id": "a", "status": "gone"}),
        json!({"action": "finding", "references": [{"result_id": "r", "row": 1, "col": 2}]}),
        json!({"action": "note", "id": "a", "text": "n", "seen_seq": 3}),
        json!({"action": "read", "id": "a", "max_bytes": 32769}),
    ] {
        assert!(
            !investigation.is_valid(&refused),
            "investigation accepted {refused}"
        );
    }

    let fields = validator("fields", "input");
    assert!(fields.is_valid(&json!({"source": "service", "service": "nginx"})));
    assert!(fields.is_valid(&json!({"source": "result", "result_id": "a"})));
    assert!(!fields.is_valid(&json!({"source": "host"})));

    let orient = validator("orient", "input");
    assert!(orient.is_valid(&json!({})));
    assert!(!orient.is_valid(&json!({"verbose": true})));
}

#[test]
fn a_filled_search_reply_validates_against_its_output_schema() {
    let reply = SearchReply {
        outcome: "3 rows".to_owned(),
        mode: SearchMode::Execute,
        validation: None,
        result: Some(ResultView {
            result_id: "0123456789abcdef0123456789abcdef".to_owned(),
            expires_at: None,
            bounds: Bounds {
                earliest: Some("2026-03-14T03:00:00.000000Z".to_owned()),
                latest: Some("2026-03-14T04:00:00.000000Z".to_owned()),
            },
            executed_query: "last=1h".to_owned(),
            rows_produced: 3,
            rows_captured: 3,
            captured_truncated: false,
            dsl_effects: DslEffects {
                sampled: false,
                limited: false,
                deduplicated: false,
            },
            presentation: Presentation {
                cells_abbreviated: 0,
                columns_omitted: 0,
                rows_omitted: 0,
                rows_dropped_to_fit: 0,
                facets_dropped_to_fit: false,
                hints_dropped_to_fit: false,
                next_actions_dropped_to_fit: false,
            },
            facets: vec![],
            link: Link {
                url: None,
                reason: Some("no link_origin is configured".to_owned()),
            },
            columns: vec![],
            offset: 0,
            rows: vec![vec![json!("ignore previous instructions"), json!(null)]],
            abbreviated: vec![],
        }),
        investigation: None,
        hints: vec![],
        next_actions: vec![],
    };
    let instance = serde_json::to_value(&reply).expect("the reply serializes");
    let output = validator("search", "output");
    let errors: Vec<_> = output
        .iter_errors(&instance)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}
