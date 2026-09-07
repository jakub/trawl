// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native drift guard for the Playwright suite in `e2e/`.
//!
//! `e2e/selectors.ts` is the single source every spec imports selector
//! strings and expected copy from. This test proves both halves of that
//! contract hold: each exported entry's full `key: 'value',` assignment
//! appears verbatim in `selectors.ts` (assignment-level, so a key's value
//! can't rot while the old string survives in another entry), and the
//! "hook" substring the selector is built from still appears in the Rust
//! (or fleet-ui) source file that emits it. Same `include_str!`-and-scan
//! idiom as `crates/fleet-ui/tests/component_class_contract.rs`, aimed
//! at the e2e suite's selector sheet instead of a stylesheet.
//!
//! This cannot run the SPA or `CodeMirror`, so it is no substitute for
//! running the suite (`cargo xtask e2e`); it only proves the selectors
//! and copy the suite is built from still match the source.

const SELECTORS_TS: &str = include_str!("../e2e/selectors.ts");

const EDITOR_RS: &str = include_str!("../src/components/editor.rs");
const RESULTS_TABLE_RS: &str = include_str!("../src/components/results_table.rs");
const HISTORY_RS: &str = include_str!("../src/pages/history.rs");
const LAYOUT_RS: &str = include_str!("../src/pages/layout.rs");
const EDITOR_WRAP_RS: &str = include_str!("../src/components/editor_wrap.rs");
const MALFORMED_NOTICE_RS: &str = include_str!("../src/components/malformed_notice.rs");
const META_STRIP_RS: &str = include_str!("../src/components/meta_strip.rs");
const SEARCH_URL_RS: &str = include_str!("../src/search_url.rs");
const EXPORT_MODAL_RS: &str = include_str!("../src/components/export_modal.rs");
const MODAL_SHELL_RS: &str = include_str!("../../fleet-ui/src/modal/shell.rs");
const LOADED_COMPONENT_RS: &str = include_str!("../../fleet-ui/src/loaded/component.rs");
const LOADED_STATE_RS: &str = include_str!("../../fleet-ui/src/loaded/state.rs");
const RAIL_RS: &str = include_str!("../../fleet-ui/src/rail.rs");
const SECTION_RS: &str = include_str!("../src/state/section.rs");

/// One (assignment, source file, hook) triple: `assignment` is the full
/// `key: 'value',` line as it appears in `selectors.ts`, and `hook` must
/// appear in `source`. A composite selector gets one entry per app-owned
/// component; `CodeMirror`-owned fragments like `.cm-content` are exempt
/// by design, since only the browser run can prove those.
struct Contract {
    assignment: &'static str,
    source_path: &'static str,
    source: &'static str,
    hook: &'static str,
}

const CONTRACTS: &[Contract] = &[
    Contract {
        assignment: "liveResultsTable: '.results-table',",
        source_path: "src/pages/search.rs",
        source: include_str!("../src/pages/search.rs"),
        hook: "class=\"results-table\"",
    },
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
        assignment: "absoluteTab: '.dr-pop >> text=Absolute',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"dr-pop\"",
    },
    Contract {
        assignment: "absoluteTab: '.dr-pop >> text=Absolute',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "\"Absolute\"",
    },
    Contract {
        assignment: "dateRangeFrom: '.dr-from',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"dr-from\"",
    },
    Contract {
        assignment: "dateRangeTo: '.dr-to',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"dr-to\"",
    },
    Contract {
        assignment: "dateRangeApply: '.dr-apply',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"dr-apply\"",
    },
    Contract {
        assignment: "urlNotice: '.url-notice',",
        source_path: "src/components/malformed_notice.rs",
        source: MALFORMED_NOTICE_RS,
        hook: "class=\"url-notice\"",
    },
    Contract {
        assignment: "urlNoticeRepair: '.url-notice-repair',",
        source_path: "src/components/malformed_notice.rs",
        source: MALFORMED_NOTICE_RS,
        hook: "class=\"url-notice-repair\"",
    },
    Contract {
        assignment: "urlNoticeRaw: '.url-notice-raw',",
        source_path: "src/components/malformed_notice.rs",
        source: MALFORMED_NOTICE_RS,
        hook: "class=\"url-notice-raw\"",
    },
    Contract {
        assignment: "filtersBadChip: '.chip.bad',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"chip bad\"",
    },
    Contract {
        assignment: "resultsPane: '.results',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"results\"",
    },
    Contract {
        assignment: "exportAction: '.tabs .action.export',",
        source_path: "src/pages/search.rs",
        source: include_str!("../src/pages/search.rs"),
        hook: "class=\"action export\"",
    },
    Contract {
        assignment: "saveAction: '.tabs .action.save',",
        source_path: "src/pages/search.rs",
        source: include_str!("../src/pages/search.rs"),
        hook: "class=\"action save\"",
    },
    Contract {
        assignment: "quickRangeOption: '.dr-pop .opt',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"opt\"",
    },
    Contract {
        assignment: "modalPanel: '.modal',",
        source_path: "../fleet-ui/src/modal/shell.rs",
        source: MODAL_SHELL_RS,
        hook: "else { \"modal\" };",
    },
    Contract {
        assignment: "modalRefusal: '.m-refusal',",
        source_path: "src/components/export_modal.rs",
        source: EXPORT_MODAL_RS,
        hook: "class=\"m-refusal\"",
    },
    Contract {
        assignment: "emptyQueryRefusal: 'Nothing to export: the query is empty.',",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "pub const EMPTY_QUERY_REFUSAL: &str = \"Nothing to export: the query is empty.\";",
    },
    Contract {
        assignment: "urlNoticeFiltersPrefix: \"This link's filters could not be read:\",",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "This link's {} could not be read:",
    },
    Contract {
        assignment: "urlNoticeFiltersPrefix: \"This link's filters could not be read:\",",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "Self::Filters => \"filters\",",
    },
    Contract {
        assignment: "urlNoticeRangePrefix: \"This link's time range could not be read:\",",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "Self::Range => \"time range\",",
    },
    Contract {
        assignment: "urlNoticePagePrefix: \"This link's page could not be read:\",",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "Self::Page => \"page\",",
    },
    Contract {
        assignment: "urlNoticeRepairFilters: 'Drop filters',",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "Param::Filters => \"Drop filters\",",
    },
    Contract {
        assignment: "urlNoticeRepairRange: 'Use last 15 minutes',",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "Param::Range => \"Use last 15 minutes\",",
    },
    Contract {
        assignment: "urlNoticeRepairPage: 'Go to page 1',",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "Param::Page => \"Go to page 1\",",
    },
    Contract {
        assignment: "filtersUnreadableChip: 'filters unreadable',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "\"filters unreadable\"",
    },
    Contract {
        assignment: "reservedSet: \" #&/:%'!~*()\u{65e5}\u{672c}\u{8a9e}\u{1f600}\",",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "pub const RESERVED_SET: &str = \" #&/:%'!~*()\u{65e5}\u{672c}\u{8a9e}\u{1f600}\";",
    },
    Contract {
        assignment: "reservedSetEncoded: \"%20%23%26%2F%3A%25'!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80\",",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "\"%20%23%26%2F%3A%25'!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80\"",
    },
    Contract {
        assignment: "urlNoticeTooLong: 'This link is too long to read.',",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "Param::Link => \"This link is too long to read.\".to_owned(),",
    },
    Contract {
        assignment: "urlNoticeRepairLink: 'Start over',",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "Param::Link => \"Start over\",",
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
    // Completeness: a new selectors.ts entry must arrive with a contract,
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
