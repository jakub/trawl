// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native drift guard for issue #118's Playwright suite (`e2e/`).
//!
//! `e2e/selectors.ts` is the single source every spec imports selector
//! strings and expected copy from. This test proves both halves of that
//! contract hold: each exported entry's full `key: 'value',` assignment
//! appears verbatim in `selectors.ts` (assignment-level, so a key's value
//! can't rot while the old string survives in another entry), AND the
//! "hook" substring the selector is built from still appears in the Rust
//! (or fleet-ui) source file that is supposed to emit it — the same
//! `include_str!`-and-scan idiom as
//! `crates/fleet-ui/tests/component_class_contract.rs`, applied across
//! the crate boundary into the e2e suite's own selector sheet instead of
//! into a stylesheet.
//!
//! This cannot run the SPA or `CodeMirror`, so it is not a substitute for
//! actually running the suite (`cargo xtask e2e`) — it only proves the
//! selectors/copy the suite is BUILT from haven't silently drifted out
//! from under it between runs.

const SELECTORS_TS: &str = include_str!("../e2e/selectors.ts");

const EDITOR_RS: &str = include_str!("../src/components/editor.rs");
const RESULTS_TABLE_RS: &str = include_str!("../src/components/results_table.rs");
const HISTORY_RS: &str = include_str!("../src/pages/history.rs");
const LAYOUT_RS: &str = include_str!("../src/pages/layout.rs");
const EDITOR_WRAP_RS: &str = include_str!("../src/components/editor_wrap.rs");
const LOADED_COMPONENT_RS: &str = include_str!("../../fleet-ui/src/loaded/component.rs");
const LOADED_STATE_RS: &str = include_str!("../../fleet-ui/src/loaded/state.rs");
const RAIL_RS: &str = include_str!("../../fleet-ui/src/rail.rs");
const SECTION_RS: &str = include_str!("../src/state/section.rs");

/// One (assignment, source file, hook) triple: `assignment` is the FULL
/// `key: 'value',` line as it appears in `selectors.ts` — pinning the
/// assignment (not a bare substring) means a key's value can't rot while
/// the old string survives elsewhere in the file — and `hook` must appear
/// in `source`. A composite selector gets one entry per app-owned
/// component (CodeMirror-owned fragments like `.cm-content` are exempt by
/// design: only the browser run can prove those).
struct Contract {
    assignment: &'static str,
    source_path: &'static str,
    source: &'static str,
    hook: &'static str,
}

const CONTRACTS: &[Contract] = &[
    Contract {
        assignment: "dslEditor: '.dsl-editor',",
        source_path: "src/components/editor.rs",
        source: EDITOR_RS,
        hook: "class=\"dsl-editor\"",
    },
    // App-owned half only; `.cm-content` itself is CodeMirror's.
    Contract {
        assignment: "cmContent: '.dsl-editor .cm-content',",
        source_path: "src/components/editor.rs",
        source: EDITOR_RS,
        hook: "class=\"dsl-editor\"",
    },
    Contract {
        assignment: "railHistoryLink: 'nav.rail a[title=\"History\"]',",
        source_path: "../fleet-ui/src/rail.rs",
        source: RAIL_RS,
        hook: "class=\"rail\"",
    },
    Contract {
        assignment: "railHistoryLink: 'nav.rail a[title=\"History\"]',",
        source_path: "src/state/section.rs",
        source: SECTION_RS,
        hook: "label: \"History\"",
    },
    Contract {
        assignment: "notFoundHeading: '.login-card h1',",
        source_path: "src/pages/layout.rs",
        source: LAYOUT_RS,
        hook: "class=\"login-card\"",
    },
    Contract {
        assignment: "notFoundSubtitle: '.login-card .subtitle',",
        source_path: "src/pages/layout.rs",
        source: LAYOUT_RS,
        hook: "class=\"subtitle\"",
    },
    Contract {
        assignment: "runButton: 'button.run',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"run\"",
    },
    Contract {
        assignment: "dateRangeTrigger: '.daterange .dr-trigger',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"daterange\"",
    },
    Contract {
        assignment: "dateRangeTrigger: '.daterange .dr-trigger',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"dr-trigger\"",
    },
    Contract {
        assignment: "realtimeTab: '.dr-pop >> text=Real-time',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"dr-pop\"",
    },
    Contract {
        assignment: "realtimeTab: '.dr-pop >> text=Real-time',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "\"Real-time\"",
    },
    Contract {
        assignment: "liveTailButton: '.rt-hint button',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"rt-hint\"",
    },
    Contract {
        assignment: "loadHintError: '.results .load-hint.error',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"results\"",
    },
    Contract {
        assignment: "loadHintError: '.results .load-hint.error',",
        source_path: "../fleet-ui/src/loaded/component.rs",
        source: LOADED_COMPONENT_RS,
        hook: "class=\"load-hint error\"",
    },
    Contract {
        assignment: "historyH1: 'Search history',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "\"Search history\"",
    },
    Contract {
        assignment: "notFoundHeading: '404',",
        source_path: "src/pages/layout.rs",
        source: LAYOUT_RS,
        hook: "<h1>\"404\"</h1>",
    },
    Contract {
        assignment: "notFoundSubtitle: 'That page does not exist.',",
        source_path: "src/pages/layout.rs",
        source: LAYOUT_RS,
        hook: "That page does not exist.",
    },
    Contract {
        assignment: "loadHintErrorPrefix: \"Couldn't load results:\",",
        source_path: "../fleet-ui/src/loaded/state.rs",
        source: LOADED_STATE_RS,
        hook: "Couldn't load {what}: {msg}",
    },
    Contract {
        assignment: "loadHintErrorPrefix: \"Couldn't load results:\",",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "label=\"results\"",
    },
    Contract {
        assignment: "liveTailButtonText: 'Live Tail',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "\"Live Tail\"",
    },
];

#[test]
fn every_selector_assignment_appears_in_selectors_ts() {
    for c in CONTRACTS {
        assert!(
            SELECTORS_TS.contains(c.assignment),
            "e2e/selectors.ts no longer contains the assignment `{}` — \
             either the key's value changed (re-verify against {}'s `{}` \
             hook and update both sides together) or the entry was \
             renamed/removed while specs may still import it.",
            c.assignment,
            c.source_path,
            c.hook,
        );
    }
}

#[test]
fn every_source_hook_still_exists() {
    for c in CONTRACTS {
        assert!(
            c.source.contains(c.hook),
            "{} no longer contains the hook `{}` that e2e/selectors.ts's \
             `{}` assignment is built from — the Playwright suite would \
             then be asserting against markup that doesn't exist. Update \
             selectors.ts (and the specs that use it) to match the real \
             source, or restore the hook if the change was accidental.",
            c.source_path,
            c.hook,
            c.assignment,
        );
    }
}

#[test]
fn every_selectors_ts_entry_is_pinned() {
    // Completeness: a NEW selectors.ts entry must arrive with a contract,
    // or it is an unguarded selector the suite can silently rot on.
    for line in SELECTORS_TS.lines() {
        let trimmed = line.trim_start();
        let is_entry = !trimmed.starts_with("///")
            && !trimmed.starts_with("//")
            && trimmed.split_once(": ").is_some_and(|(key, rest)| {
                !key.is_empty()
                    && key.chars().all(|ch| ch.is_ascii_alphanumeric())
                    && (rest.starts_with('\'') || rest.starts_with('"'))
            });
        if is_entry {
            assert!(
                CONTRACTS.iter().any(|c| c.assignment == trimmed),
                "e2e/selectors.ts entry `{trimmed}` has no drift-guard \
                 contract in e2e_selector_contract.rs — add one pinning \
                 it to the source that renders it.",
            );
        }
    }
}
