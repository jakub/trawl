// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parse arbitrary DSL text and, for every input the parser accepts,
//! format it through the checked renderer (ADR-0061).
//!
//! The claim under test: formatting never changes what a query means.
//! The checked renderer returns text whose reparse has the query's
//! canonical form, or refuses as too long. An unfaithful refusal is a
//! formatter defect, so it fails here. The reparse, the comparison and a
//! second format run here as well, rather than being trusted to the
//! renderer under test: the returned text reparses to the same canonical
//! form, and formatting it again gives the same text.

#![no_main]

use libfuzzer_sys::fuzz_target;

use trawl_core::canonical_form::same_query;
use trawl_core::format::{FormatError, checked_format_query};
use trawl_core::parser::parse;

fuzz_target!(|data: &str| {
    let Ok(query) = parse(data) else {
        return;
    };
    let text = match checked_format_query(&query) {
        Ok(text) => text,
        Err(FormatError::TooLong { .. }) => return,
        Err(FormatError::Unfaithful) => panic!("{data:?} formats unfaithfully"),
    };
    let reparsed = parse(&text)
        .unwrap_or_else(|e| panic!("{data:?} formatted as {text:?}, which does not parse: {e:?}"));
    assert!(
        same_query(&query, &reparsed),
        "{data:?} formatted as {text:?}, which means something else"
    );
    assert_eq!(
        checked_format_query(&reparsed),
        Ok(text),
        "{data:?}: formatting the formatted text again changed it"
    );
});
