// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parse arbitrary DSL text, invent a catalog pin for every name
//! `trawl_core::field_refs::referenced_fields` reports out of the parsed
//! query, and emit SQL twice: once pin-blind and once under those pins.
//!
//! Not "every field the query binds". `referenced_fields` skips bare-word
//! search terms and the time bounds, so `error last=1h` runs with an
//! empty pin map even though it reads `message`, `_raw` and `_time` —
//! envelope fields whose production types are fixed, so there is no pin
//! for the selector to vary and nothing is lost here.
//!
//! The wire encoding and the pin derivation live in
//! [`trawl_core::fuzz_input`], not here, because the PREPARE fixture in
//! trawl-engine replays the same committed cases and the two would
//! otherwise decode them differently the first time either changed.

#![no_main]

use libfuzzer_sys::fuzz_target;

use trawl_core::compare::CompareError;
use trawl_core::context::EvalContext;
use trawl_core::emitter::{self, EmitError, EmittedQuery, JsonPathFault};
use trawl_core::fuzz_input;
use trawl_core::json_path::{PathError, PathErrorKind};
use trawl_core::parser::ParseError;
use trawl_core::schema::{CanonicalType, FieldTypes};

/// Fixed: varying it would spend fuzzer time inside `validate_source_path`
/// instead of inside the pin rule table.
const SRC: &str = "test.parquet";

/// The exhaustive destructuring is the guard. With no `..`, a field added
/// to `ParseError` stops compiling right here and whoever adds it has to
/// decide whether the fuzzer should check the new information.
fn classify_parse_failure(errors: &[ParseError]) {
    for err in errors {
        let ParseError {
            message: _,
            span: _,
            label: _,
            hint: _,
        } = err;

        assert!(
            !err.to_string().is_empty(),
            "a ParseError rendered as empty text: {err:?}"
        );
    }
}

/// Same guard as [`classify_parse_failure`]: no `_` arm and no `..`, so a
/// new variant or a new field on an existing one is a compile error.
fn classify_emit_failure(err: &EmitError, query: &str) {
    match err {
        EmitError::UnknownFunction {
            name: _,
            suggestion: _,
        }
        | EmitError::InvalidAggregation { message: _ }
        | EmitError::UnsupportedOperation { message: _ }
        | EmitError::InvalidFormat {
            func_name: _,
            format: _,
        }
        | EmitError::ComputedPattern
        | EmitError::RegexParameters { count: _, bytes: _ } => {}
        EmitError::InvalidJsonPath {
            func_name: _,
            fault,
        } => classify_json_path_fault(fault),
        EmitError::Comparison(cause) => classify_compare_failure(cause),
    }

    assert!(
        !err.to_string().is_empty(),
        "an EmitError rendered as empty text: {err:?}"
    );

    // `to_parse_errors` is how an emit refusal reaches the renderers that
    // only speak parse errors — the validate route and the TUI's error
    // underline both call it. Those renderers slice the query text by the
    // span, so a span reaching past the end of input panics them.
    for rendered in err.to_parse_errors(query.len()) {
        assert!(
            rendered.span.end <= query.len(),
            "EmitError::to_parse_errors returned span {:?} past the end of a \
             {}-byte query {query:?}: {err:?}",
            rendered.span,
            query.len()
        );
        assert!(
            !rendered.message.is_empty(),
            "EmitError::to_parse_errors returned an empty message: {err:?}"
        );
    }
}

/// Same reason as [`classify_emit_failure`]: a new kind of path refusal is
/// a compile error here.
fn classify_json_path_fault(fault: &JsonPathFault) {
    match fault {
        JsonPathFault::NotLiteral => {}
        JsonPathFault::Invalid {
            path: _,
            error:
                PathError {
                    kind: PathErrorKind::Malformed | PathErrorKind::Unsupported,
                    reason,
                    fix: _,
                },
        } => assert!(!reason.is_empty(), "a JSON path refusal with no reason"),
    }
}

/// One variant today. The differential oracle below leans on
/// `CompareError`'s doc comment claiming SEVERITY is the only pin with a
/// closed vocabulary, so a second variant means the oracle needs rereading.
fn classify_compare_failure(err: &CompareError) {
    match err {
        CompareError::UnknownSeverityToken { token: _ } => {}
    }
}

fn describe(result: &Result<EmittedQuery, EmitError>) -> String {
    match result {
        Ok(emitted) => format!("ok, sql {:?}", emitted.sql),
        Err(err) => format!("error: {err}"),
    }
}

fn report(
    query: &str,
    pins: &FieldTypes,
    blind: &Result<EmittedQuery, EmitError>,
    pinned: &Result<EmittedQuery, EmitError>,
) -> String {
    format!(
        "query {query:?}\n  pins: {pins:?}\n  pin-blind: {}\n  pinned:    {}",
        describe(blind),
        describe(pinned)
    )
}

fuzz_target!(|input: &str| {
    let case = fuzz_input::decode_case(input);

    let query = match trawl_core::parser::parse(case.query) {
        Ok(query) => query,
        Err(errors) => {
            classify_parse_failure(&errors);
            return;
        }
    };

    let pins = fuzz_input::derive_field_types(&query, case.selector);

    // One anchor for both lanes. `now()` binds as a parameter (ADR-0017),
    // so two `capture()` calls would put two different instants in the two
    // parameter lists and every differential comparison over a query using
    // `now()` would be reading clock skew rather than pin behaviour.
    let anchor = EvalContext::capture();
    let blind = emitter::emit(&query, SRC, anchor);
    let pinned = emitter::emit_with_pins(
        &query,
        SRC,
        &pins,
        &emitter::InputColumns::unprobed(),
        anchor,
    );

    if let Err(err) = &blind {
        classify_emit_failure(err, case.query);
    }
    if let Err(err) = &pinned {
        classify_emit_failure(err, case.query);
    }

    // A pin decides how a comparison is rendered, never whether the query
    // can be emitted at all. With a SEVERITY pin the lanes may disagree in
    // one direction only: `_severity=banana` names no ladder point and is a
    // refusal under the pin.
    let severity_pinned = pins.iter().any(|(_, ty)| ty == CanonicalType::Severity);
    if !severity_pinned {
        assert_eq!(
            blind.is_ok(),
            pinned.is_ok(),
            "pins changed whether a query is emittable, with no SEVERITY pin \
             in the map — every other rule table entry is documented total \
             over literals\n  {}",
            report(case.query, &pins, &blind, &pinned)
        );
    }

    if blind.is_err() {
        assert!(
            pinned.is_err(),
            "a pin map made an unemittable query emittable\n  {}",
            report(case.query, &pins, &blind, &pinned)
        );
    }
});
