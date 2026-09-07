// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// SINGLE SOURCE for every selector + expected copy string the specs use.
// Every entry is drift-guarded natively by
// `crates/trawl-web-ui/tests/e2e_selector_contract.rs`, which greps the
// *named source file* for the *hook* — re-read both sides on drift, this
// sheet is a starting point, not gospel.

export const SEL = {
  /// crates/trawl-web-ui/src/pages/search.rs LiveRawTable.
  liveResultsTable: '.results-table',
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
  /// crates/trawl-web-ui/src/components/editor_wrap.rs DateRangePopover —
  /// the Segmented tab strip's "Absolute" option.
  absoluteTab: '.dr-pop >> text=Absolute',
  /// crates/trawl-web-ui/src/components/editor_wrap.rs DateRangePopover —
  /// the Absolute tab's From input.
  dateRangeFrom: '.dr-from',
  /// crates/trawl-web-ui/src/components/editor_wrap.rs DateRangePopover —
  /// the Absolute tab's To input.
  dateRangeTo: '.dr-to',
  /// crates/trawl-web-ui/src/components/editor_wrap.rs DateRangePopover —
  /// the wrapper carrying the Apply button (fleet-ui's Btn takes no class).
  dateRangeApply: '.dr-apply',
  /// crates/trawl-web-ui/src/components/malformed_notice.rs — the banner a
  /// search URL that cannot be read shows instead of results.
  urlNotice: '.url-notice',
  /// crates/trawl-web-ui/src/components/malformed_notice.rs — its one
  /// repair button.
  urlNoticeRepair: '.url-notice-repair',
  /// crates/trawl-web-ui/src/components/malformed_notice.rs — the raw
  /// parameter value, rendered as text.
  urlNoticeRaw: '.url-notice-raw',
  /// crates/trawl-web-ui/src/components/meta_strip.rs — the chip that says
  /// the link's filters could not be read.
  filtersBadChip: '.chip.bad',
  /// crates/trawl-web-ui/src/components/results_table.rs — the snapshot
  /// results pane. Absent entirely while the malformed banner is up: the
  /// banner is the whole results area then (ADR-0027).
  resultsPane: '.results',
  /// crates/trawl-web-ui/src/pages/search.rs — the tab strip's trailing
  /// Export action, which opens the export modal.
  exportAction: '.tabs .action.export',
  /// crates/trawl-web-ui/src/pages/search.rs — its Save twin.
  saveAction: '.tabs .action.save',
  /// crates/trawl-web-ui/src/components/editor_wrap.rs DateRangePopover —
  /// one quick-range preset in the Relative tab's grid.
  quickRangeOption: '.dr-pop .opt',
  /// crates/fleet-ui/src/modal/shell.rs — the modal panel itself (the
  /// scrim is `.modal-scrim`, a different class).
  modalPanel: '.modal',
  /// crates/trawl-web-ui/src/components/export_modal.rs — what the modal
  /// says when it refuses its own query rather than posting it.
  modalRefusal: '.m-refusal',
  /// crates/fleet-ui/src/toast/runtime.rs Toasts, whose class is
  /// `toast {kind}` from crates/fleet-ui/src/toast/kinds.rs. An error
  /// toast is what a producer refusal raises.
  toastError: '.toast.error',
  /// crates/fleet-ui/src/toast/runtime.rs — a toast of ANY kind. What a
  /// spec asserting that NOTHING was announced has to watch, since the
  /// kind is the thing under test.
  toastAny: '.toast',
  /// crates/fleet-ui/src/drawer.rs — the sliding panel itself (the scrim
  /// is `.sd-scrim`, a different element).
  drawerPanel: '.sd-drawer',
  /// crates/fleet-ui/src/drawer.rs — the built-in close button in the
  /// header's actions row.
  drawerClose: '.sd-x',
  /// crates/fleet-ui/src/drawer.rs — the header title slot, filled by
  /// whatever the app passes as `title`.
  drawerTitle: '.sd-ttl',
  /// crates/trawl-web-ui/src/components/field_case_drawer.rs — the case
  /// file body, present in the loaded, missing and error arms alike.
  fieldCase: '.fc-case',
  /// crates/trawl-web-ui/src/components/field_case_drawer.rs `job_block`
  /// — present exactly when a repin job was adopted, which makes it the
  /// positive control for "the status poll actually started".
  fieldCaseJob: '.fc-job',
  /// crates/fleet-ui/src/topbar.rs — the account menu's trigger. A
  /// native button since ADR-0028; the `.topbar` prefix keeps it away
  /// from any other `.user` a page might carry.
  topbarUser: '.topbar button.user',
  /// crates/fleet-ui/src/menu.rs mounted by topbar.rs with
  /// `panel_class="user-menu"` — the role="menu" node INSIDE the panel
  /// wrapper, so the identity header (which sits outside it) is not part
  /// of what this matches.
  userMenu: '.user-menu [role="menu"]',
  /// The account menu's command buttons. Ordered as rendered, so
  /// `.nth(0)` is the theme item and `.nth(1)` Sign Out; the separator
  /// between them is role="separator" and is not matched.
  userMenuItem: '.user-menu [role="menuitem"]',
  /// crates/fleet-ui/src/actions_menu.rs — the row overflow trigger.
  actionsMenuTrigger: '.actions-wrap button.btn-icon',
  /// crates/fleet-ui/src/menu.rs mounted by actions_menu.rs with
  /// `panel_class="actions-menu"` — the row menu's command buttons.
  actionsMenuItem: '.actions-menu [role="menuitem"]',
  /// crates/fleet-ui/src/tabs.rs TabsStyle::Workspace — one tab of the
  /// workspace strip. Selecting on the ROLE, not `.t`, is the point: the
  /// trailing Save/Export actions live in `.tabs` too and must not be
  /// tabs.
  workspaceTab: '.tabs [role="tab"]',
  /// crates/fleet-ui/src/tabs.rs TabsStyle::Drawer — one tab of the
  /// drawer strip (the service drawer's Overview/Fields/Live Tail).
  drawerTab: '.sd-tabs [role="tab"]',
  /// crates/fleet-ui/src/modal/shell.rs — the dialog header's close
  /// button.
  modalClose: '.modal .m-hd button.x',
  /// crates/fleet-ui/src/toast/runtime.rs — one toast's dismiss button.
  toastDismiss: '.toast button.x',
  /// crates/trawl-web-ui/src/components/editor_wrap.rs — a tool link
  /// under the editor. Save is a plain button; Share is fleet-ui's
  /// CopyButton in bare mode, which renders the caller's class on its
  /// own native button.
  editorTool: '.editor-tools button.tool',
} as const;

/// Timings the specs share with the app. Mirrored here rather than
/// re-derived: a spec that waits out a poll period has to wait out the
/// SAME one the app uses, and `e2e_selector_contract.rs` pins each value
/// against the Rust constant it copies.
export const TIMING = {
  /// crates/trawl-web-ui/src/repin_flow.rs REPIN_POLL_MS — how often the
  /// field case file re-reads the repin status while it tracks a job.
  repinPollMs: 3000,
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
  /// crates/trawl-web-ui/src/search_url.rs Malformed::message + Param::noun.
  urlNoticeFiltersPrefix: "This link's filters could not be read:",
  /// crates/trawl-web-ui/src/search_url.rs Malformed::message + Param::noun.
  urlNoticeRangePrefix: "This link's time range could not be read:",
  /// crates/trawl-web-ui/src/search_url.rs Malformed::message + Param::noun.
  urlNoticePagePrefix: "This link's page could not be read:",
  /// crates/trawl-web-ui/src/search_url.rs Malformed::repair_label.
  urlNoticeRepairFilters: 'Drop filters',
  /// crates/trawl-web-ui/src/search_url.rs Malformed::repair_label.
  urlNoticeRepairRange: 'Use last 15 minutes',
  /// crates/trawl-web-ui/src/search_url.rs Malformed::repair_label.
  urlNoticeRepairPage: 'Go to page 1',
  /// crates/trawl-web-ui/src/components/meta_strip.rs — the bad chip's text.
  filtersUnreadableChip: 'filters unreadable',
  /// crates/trawl-web-ui/src/search_url.rs RESERVED_SET — the percent-codec
  /// drift guard's input, typed into the editor by search-url.spec.ts and
  /// checked byte for byte against the native table test's expectation.
  reservedSet: " #&/:%'!~*()日本語😀",
  /// crates/trawl-web-ui/src/search_url.rs RESERVED_SET_ENCODED — what
  /// the app's encoder writes AND what the browser keeps in
  /// location.search, which are the same string. encodeURIComponent
  /// differs from it at the apostrophe alone.
  reservedSetEncoded: '%20%23%26%2F%3A%25%27!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80',
  /// crates/trawl-web-ui/src/search_url.rs EMPTY_QUERY_REFUSAL.
  emptyQueryRefusal: 'Nothing to export: the query is empty.',
  /// crates/trawl-web-ui/src/search_url.rs Malformed::message, the
  /// whole-link arm.
  urlNoticeTooLong: 'This link is too long to read.',
  /// crates/trawl-web-ui/src/search_url.rs Malformed::repair_label.
  urlNoticeRepairLink: 'Start over',
  /// crates/trawl-web-ui/src/search_url.rs refusal_copy: what a
  /// navigation refused by the producer's own link bound says.
  linkTooLongToast: "Can't open this search: link too long",
  /// crates/fleet-ui/src/modal/shell.rs — the close button's whole
  /// accessible name (the glyph is an icon, so aria-label is all of it).
  modalCloseName: 'Close dialog',
  /// crates/fleet-ui/src/toast/runtime.rs — the dismiss button's whole
  /// accessible name; the multiplication sign is aria-hidden.
  toastDismissName: 'Dismiss notification',
  /// crates/fleet-ui/src/actions_menu.rs — the row trigger's aria-label
  /// AND the menu panel's aria-label, which are deliberately the same
  /// string: the trigger names the menu it opens.
  actionsName: 'Actions',
  /// crates/fleet-ui/src/topbar.rs — the account menu panel's
  /// aria-label. The TRIGGER is named by its visible user name instead,
  /// so this is the panel's name only.
  accountMenuName: 'Account',
} as const;
