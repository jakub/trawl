// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Completion for partial DSL expressions, with `CodeMirror` UTF-16 offsets.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
use crate::offset::{utf8_to_utf16, utf16_to_utf8};
use serde::Serialize;
/// JS-side autocomplete entry.
#[derive(Serialize)]
struct Completion {
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'static str>,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
pub(crate) struct CompletionResult {
    from: usize,
    options: Vec<Completion>,
    filter: bool,
}

/// Autocomplete: offers pipe stages when the word being typed follows a
/// `|`, otherwise function names. A heuristic, not a context-aware
/// completion over the parse tree.
///
/// `pos_utf16` is the cursor position in UTF-16 code units (as `CodeMirror`
/// reports). We translate to a UTF-8 byte offset before slicing.
pub(crate) fn complete_at(doc: &str, pos_utf16: usize) -> Option<CompletionResult> {
    let pos_utf8 = utf16_to_utf8(doc, pos_utf16);
    // `utf16_to_utf8` is supposed to land on a char boundary, but if
    // CodeMirror ever hands us a cursor that sits between two UTF-16
    // surrogate units we'd slice mid-codepoint and panic across the
    // wasm boundary. `.get()` returns None on a non-boundary slice,
    // and the `?` bails out of completion entirely — an acceptable
    // "no completions" fallback for the niche case.
    let prefix = doc.get(..pos_utf8)?;
    let last_word_start_utf8 = prefix
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(pos_utf8, |(i, _)| i);

    let word = &prefix[last_word_start_utf8..];
    if word.is_empty() {
        return None;
    }

    let mut pipe = None;
    let mut word_is_bare = false;
    trawl_core::parser::scan::scan_outside_quotes(prefix, |i, b| {
        if b == b'|' {
            pipe = Some(i);
        }
        if i == last_word_start_utf8 {
            word_is_bare = true;
        }
        false
    });
    if !word_is_bare {
        return None;
    }
    let after_pipe = pipe.is_some_and(|i| prefix[i + 1..last_word_start_utf8].trim().is_empty());

    let options: Vec<Completion> = if after_pipe {
        trawl_core::parser::suggest::KNOWN_PIPE_STAGES
            .iter()
            .filter(|s| s.starts_with(word))
            .map(|s| Completion {
                label: (*s).to_string(),
                detail: Some("pipe stage"),
                kind: "keyword",
            })
            .collect()
    } else {
        trawl_core::parser::suggest::KNOWN_FUNCTIONS
            .iter()
            .filter(|s| s.starts_with(word))
            .map(|s| Completion {
                label: format!("{s}()"),
                detail: Some("function"),
                kind: "function",
            })
            .collect()
    };

    if options.is_empty() {
        return None;
    }

    // `CompletionResult::from` must be in UTF-16 code units.
    Some(CompletionResult {
        from: utf8_to_utf16(doc, last_word_start_utf8),
        options,
        filter: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stages_and_expressions() {
        for q in [
            "cou",
            "* | stats cou",
            "* | stats count(), cou",
            "message=\"a|b\" | stats cou",
            "message=\"日本🐟\" | stats cou",
        ] {
            let result = complete_at(q, q.encode_utf16().count()).unwrap();
            assert!(result.options.iter().any(|o| o.label == "count()"), "{q}");
            assert_eq!(result.from, q.encode_utf16().count() - 3);
        }
        let q = "* | sta";
        assert!(
            complete_at(q, q.len())
                .unwrap()
                .options
                .iter()
                .any(|o| o.label == "stats")
        );
    }
    #[test]
    fn no_completion_inside_literals_or_comments() {
        for q in [
            "message=\"| cou",
            "* # | cou",
            "* | stats `cou",
            "message=/cou",
        ] {
            assert!(complete_at(q, q.encode_utf16().count()).is_none(), "{q}");
        }
    }
}
