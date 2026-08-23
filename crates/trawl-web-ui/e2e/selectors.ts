// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// SINGLE SOURCE for every selector + expected copy string the specs use.
// Every entry is drift-guarded natively by
// `crates/trawl-web-ui/tests/e2e_selector_contract.rs`, which greps the
// *named source file* for the *hook* — re-read both sides on drift, this
// sheet is a starting point, not gospel.

export const SEL = {
  /// crates/trawl-web-ui/src/components/editor.rs — the CodeMirror mount div.
  dslEditor: '.dsl-editor',
  /// CodeMirror's own contenteditable content div (standard CM6 class,
  /// not app-owned) — the click target that focuses the editor.
  cmContent: '.dsl-editor .cm-content',
  /// crates/fleet-ui/src/rail.rs — <A title={label}> per RailItem; trawl's
  /// item list is crates/trawl-web-ui/src/state/section.rs (label "History",
  /// path "/search/history").
  railHistoryLink: 'nav.rail a[title="History"]',
  /// crates/trawl-web-ui/src/pages/layout.rs NotFound — 404 heading.
  notFoundHeading: '.login-card h1',
  /// crates/trawl-web-ui/src/pages/layout.rs NotFound — 404 subtitle.
  notFoundSubtitle: '.login-card .subtitle',
  /// crates/trawl-web-ui/src/components/editor_wrap.rs — Run/Haul button.
  runButton: 'button.run',
  /// crates/trawl-web-ui/src/components/editor_wrap.rs DateRange — opens
  /// the date-range/live-tail popover.
  dateRangeTrigger: '.daterange .dr-trigger',
  /// crates/trawl-web-ui/src/components/editor_wrap.rs DateRangePopover —
  /// the Segmented tab strip's "Real-time" option (fleet-ui Segmented
  /// renders one element per SegmentedOption carrying its label text).
  realtimeTab: '.dr-pop >> text=Real-time',
  /// crates/trawl-web-ui/src/components/editor_wrap.rs DateRangePopover —
  /// the Real-time tab's "Live Tail" button, which calls `on_live`.
  liveTailButton: '.rt-hint button',
  /// crates/fleet-ui/src/loaded/component.rs Loaded's default Error arm.
  loadHintError: '.results .load-hint.error',
} as const;

export const COPY = {
  /// crates/trawl-web-ui/src/pages/history.rs HistoryPage.
  historyH1: 'Search history',
  /// crates/trawl-web-ui/src/pages/layout.rs NotFound.
  notFoundHeading: '404',
  /// crates/trawl-web-ui/src/pages/layout.rs NotFound.
  notFoundSubtitle: 'That page does not exist.',
  /// crates/fleet-ui/src/loaded/component.rs + state.rs error_copy():
  /// `format!("Couldn't load {what}: {msg}")` — results_table.rs passes
  /// `label="results"`, so the stable substring is this prefix (the `msg`
  /// tail comes from the server's own error envelope and is not pinned
  /// here).
  loadHintErrorPrefix: "Couldn't load results:",
  /// crates/trawl-web-ui/src/components/editor_wrap.rs DateRangePopover —
  /// Real-time tab's "Live Tail" button text.
  liveTailButtonText: 'Live Tail',
} as const;
