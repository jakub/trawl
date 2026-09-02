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
//!
//! Three claims are under test.
//!
//! 1. Neither emitter lane panics, for any pin map, on any input the
//!    parser accepted. libFuzzer owns that check: nothing here catches a
//!    panic, so a panic aborts the process and is reported as a crash.
//! 2. Every typed failure is an outcome someone named. See
//!    [`classify_emit_failure`] — the classifiers exist to fail the build
//!    when a new error variant appears, not to inspect the error.
//! 3. Pins change the SQL, never whether a query is emittable — except
//!    where the crate documents a closed vocabulary. That is the
//!    differential oracle at the bottom of the target.

#![no_main]

use libfuzzer_sys::fuzz_target;

use trawl_core::compare::CompareError;
use trawl_core::context::EvalContext;
use trawl_core::emitter::{self, EmitError, EmittedQuery};
use trawl_core::fuzz_input;
use trawl_core::parser::ParseError;
use trawl_core::schema::{CanonicalType, FieldTypes};

/// The parquet glob every case emits against.
///
/// Fixed, because the source path is not what this oracle is about:
/// varying it would spend fuzzer time inside `validate_source_path`
/// instead of inside the pin rule table.
const SRC: &str = "test.parquet";

/// Accept a parse failure by naming every field of [`ParseError`].
///
/// This function looks like it does nothing, and that is the point: the
/// exhaustive destructuring is the guard. With no `..`, a field added to
/// `ParseError` stops compiling right here and whoever adds it has to
/// decide whether the fuzzer should check the new information, where a
/// `..` would swallow that decision and leave the target reporting green
/// while covering less than it claims.
fn classify_parse_failure(errors: &[ParseError]) {
    for err in errors {
        let ParseError {
            message: _,
            span: _,
            label: _,
            hint: _,
        } = err;

        // Every rejection path renders somewhere: the CLI prints it, the
        // TUI draws it, `/api/v1/validate` puts it on the wire. An empty
        // rendering is a rejection with no reason attached.
        assert!(
            !err.to_string().is_empty(),
            "a ParseError rendered as empty text: {err:?}"
        );
    }
}

/// Accept an emit failure by naming every variant of [`EmitError`].
///
/// Same guard as [`classify_parse_failure`], one level up: no `_` arm and
/// no `..` inside any variant pattern, so a new variant or a new field on
/// an existing one is a compile error rather than an error shape silently
/// accepted as a known outcome.
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
        } => {}
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

/// Accept a pin rule table refusal by naming every variant of
/// [`CompareError`].
///
/// One variant today. The match stays exhaustive anyway for the reason
/// the other two classifiers do: the differential oracle below leans on
/// `CompareError`'s doc comment claiming SEVERITY is the only pin with a
/// closed vocabulary, so a second variant would mean that claim moved and
/// the oracle needs rereading. Failing to compile is how that gets
/// noticed.
fn classify_compare_failure(err: &CompareError) {
    match err {
        CompareError::UnknownSeverityToken { token: _ } => {}
    }
}

/// One lane's outcome, rendered for a crash report.
///
/// The SQL text on success, the message on failure. Whoever reads a
/// libFuzzer artifact has the input bytes and nothing else, so the
/// assertion message has to carry enough to tell which lane went wrong
/// and how.
fn describe(result: &Result<EmittedQuery, EmitError>) -> String {
    match result {
        Ok(emitted) => format!("ok, sql {:?}", emitted.sql),
        Err(err) => format!("error: {err}"),
    }
}

/// The whole bug report for a differential failure.
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
    let pinned = emitter::emit_with_pins(&query, SRC, &pins, anchor);

    if let Err(err) = &blind {
        classify_emit_failure(err, case.query);
    }
    if let Err(err) = &pinned {
        classify_emit_failure(err, case.query);
    }

    // The differential oracle, and the crate invariant it asserts.
    //
    // `CompareError`'s doc comment states that SEVERITY is the only pin
    // with a closed vocabulary and that every other rule table entry is
    // total over literals by construction. Read as a testable claim: a
    // pin decides how a comparison is rendered, never whether the query
    // can be emitted at all. So with no SEVERITY pin anywhere in the map,
    // the two lanes must agree exactly on emittability.
    //
    // With a SEVERITY pin the lanes are allowed to disagree in one
    // direction only: `_severity=banana` names no ladder point and is a
    // refusal under the pin where the pin-blind lane happily emits a text
    // comparison.
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

    // Unconditional, SEVERITY included: pins may narrow what emits, never
    // widen it. A query the pin-blind emitter refuses is malformed on its
    // own terms — an unknown function, a bad aggregation — and no pin map
    // can make it well formed.
    if blind.is_err() {
        assert!(
            pinned.is_err(),
            "a pin map made an unemittable query emittable\n  {}",
            report(case.query, &pins, &blind, &pinned)
        );
    }
});
