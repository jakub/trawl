// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// SINGLE SOURCE for every selector + expected copy string the specs use.
// Every entry is drift-guarded natively by
// `crates/trawl-web-ui/tests/e2e_selector_contract.rs`, which greps the
// *named source file* for the *hook* — re-read both sides on drift, this
// sheet is a starting point, not gospel.

export const SEL = {
  /// crates/fleet-ui/src/pager.rs — the shared table footer and summary.
  resultsFooter: '.results-footer',
  resultsSummary: '.results-summary',
  historyAwayLink: 'nav.rail a[title="Schema"]',
  historyPage: '.history-page',
  historyFormat: 'select[aria-label="History export format"]',
  historyFilter: 'input[placeholder="Filter history…"]',

  /// crates/fleet-ui/src/modal/confirm.rs ConfirmModal.
  healthConfirm: '[role="alertdialog"]',
  /// crates/fleet-ui/src/rail.rs title, crates/trawl-web-ui/src/state/section.rs Sources label.
  healthAwayLink: 'nav.rail a[title="Sources"]',
  /// crates/trawl-web-ui/src/components/status_bar.rs hot buffer group.
  healthFooterHot: '.statusbar .grp[title="Hot buffer (events / bytes)"]',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthPage: '.health-page',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthSection: '.health-section',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthCapacity: '.health-capacity',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthLive: '.health-live',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthLiveState: '.health-live-state',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthQueries: '.health-queries',
  /// crates/trawl-web-ui/src/pages/health.rs named keyboard-scrollable query region.
  healthQueryScroll: '.health-query-scroll',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthRefresh: '.health-refresh',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthQueriesRefresh: '.health-queries-refresh',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthRow: 'tr[data-query-id]',
  /// crates/trawl-web-ui/src/pages/health.rs HealthPage and HealthQueries.
  healthOwnRow: 'tr[data-own="true"]',
  /// crates/fleet-ui/src/rail.rs title, crates/trawl-web-ui/src/state/section.rs Health label and path.
  railHealthLink: 'nav.rail a[title="Health"]',

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
  /// crates/fleet-ui/src/range_dialog.rs RangeDialog — opens
  /// the date-range/live-tail popover.
  dateRangeTrigger: '.daterange .dr-trigger',
  /// crates/fleet-ui/src/range_dialog.rs RangePanel —
  /// the Segmented tab strip's "Real-time" option (fleet-ui Segmented
  /// renders one element per SegmentedOption carrying its label text).
  realtimeTab: '.dr-pop >> text=Real-time',
  /// crates/fleet-ui/src/range_dialog.rs RangePanel —
  /// the Real-time tab's "Live Tail" button, which calls `on_live`.
  liveTailButton: '.rt-hint button',
  /// crates/fleet-ui/src/loaded/component.rs Loaded's default Error arm.
  loadHintError: '.results .load-hint.error',
  /// crates/fleet-ui/src/range_dialog.rs RangePanel —
  /// the Segmented tab strip's "Absolute" option.
  absoluteTab: '.dr-pop >> text=Absolute',
  /// crates/fleet-ui/src/range_dialog.rs RangePanel —
  /// the Absolute tab's From input.
  dateRangeFrom: '.dr-from',
  /// crates/fleet-ui/src/range_dialog.rs RangePanel —
  /// the Absolute tab's To input.
  dateRangeTo: '.dr-to',
  /// crates/fleet-ui/src/range_dialog.rs RangePanel —
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
  /// crates/fleet-ui/src/range_dialog.rs RangePanel —
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
  /// own native button. Since ADR-0029 Format is a button too, so this
  /// matches THREE buttons, not two.
  editorTool: '.editor-tools button.tool',

  // -- the stretched row control and its neighbours (ADR-0029) -------
  /// One data row of a `.tbl` div table. Every list page mounts exactly
  /// one `.tbl-body`, so this is unambiguous per page: schema services,
  /// nets, runs and history all render the same row shape.
  tableRow: '.tbl-body .tbl-row',
  /// The ONE control a list row carries, stretched over the row by its
  /// `::after`. A link on schema, nets and runs; a button on history.
  rowStretch: '.row-stretch',
  /// crates/trawl-web-ui/src/pages/schema.rs — a row's hover/focus
  /// revealed quick actions (Search, Live tail), told apart by name.
  /// Row-relative, like every selector a spec scopes INSIDE a row:
  /// repeating the row class here would ask for a row inside a row.
  schemaQuickAction: '.row-act .qa',
  /// crates/trawl-web-ui/src/pages/schema.rs — the services table's
  /// sortable header controls, rendered by `components/sort_th.rs`.
  /// The same selector reaches the nets table's one sortable header.
  tableSortControl: '.tbl-hd .th.sortable button',
  /// crates/trawl-web-ui/src/pages/history.rs — the Save as Net control
  /// nested inside the history row, above the stretched rerun button.
  /// Row-relative: `.link` belongs to the history row and nothing else.
  historySaveAsNet: 'button.link',

  // -- results table --------------------------------------------------
  /// crates/trawl-web-ui/src/components/results_table.rs — one data row
  /// of the snapshot table. The detail row is a SIBLING `<tr>` with no
  /// `.exp-col`, so a count of these is not a count of results.
  resultsRow: '.results-table tbody tr',
  /// Its one control: the caret button in the expander cell, stretched
  /// over the row.
  resultsExpandControl: '.results-table td.exp-col button.row-stretch',
  /// The expanded row's detail cell, one per open row.
  resultsDetailCell: '.results-table td.detail',
  /// A key/value tag inside that detail cell, which adds an include
  /// filter for the field it names.
  resultsDetailTag: '.results-table td.detail button.tag',
  /// A sortable column header of the real `<table>`. `aria-sort` rides
  /// the CELL, so the assertion target is the th and not its button.
  resultsSortHeader: '.results-table th.sortable',
  /// The control inside that header.
  resultsSortControl: '.results-table th.sortable button.th-sort',

  // -- service drawer -------------------------------------------------
  /// crates/trawl-web-ui/src/components/service_drawer.rs — one field
  /// row of the Fields pane.
  serviceFieldRow: '.sd-fields .sf-row',
  /// The degraded badge nested above that row's control, which opens
  /// the field's case file instead of expanding the row. Row-relative;
  /// the row's own control is `rowStretch`.
  serviceDegradedBadge: 'button.deg-btn',
  /// The Fields pane's sortable headers — the same `sort_th` helper the
  /// schema and nets tables use.
  serviceFieldSortControl: '.sf-hd .th.sortable button',
  /// crates/trawl-web-ui/src/components/service_drawer.rs — a row of the
  /// overview's "Top fields by cardinality" card, whose control searches
  /// for the field through the navigator.
  serviceTopField: '.topfields .tf button.fn',

  // -- facet rail -----------------------------------------------------
  /// crates/trawl-web-ui/src/components/facet_sidebar.rs — one facet
  /// group (a field and its values).
  facetGroup: '.facets .g',
  /// Its header, which collapses the group and carries aria-expanded.
  /// Group-relative, like every entry below it: a spec scopes these to
  /// one group or one value row, and repeating the ancestor class would
  /// ask for a group inside a group.
  facetGroupHeader: 'button.g-hd',
  /// One value row inside a group. Not a control itself since ADR-0029:
  /// the two buttons in its action area are. `.vals` is the group's own
  /// value list, so this stays unambiguous when a spec uses it against
  /// the page.
  facetValue: '.vals .v',
  /// The value's name span, which must stay clear of the action area.
  /// Row-relative.
  facetValueName: '.n',
  /// The action area, revealed by hover or focus-within (opacity, never
  /// `display: none`, or Tab could not reach the buttons). Row-relative.
  facetActions: '.act',
  /// Include (+) and exclude (⊘), told apart by accessible name.
  /// Row- or group-relative.
  facetOp: '.act button.op',
  /// The group's "+ N more" control. Group-relative.
  facetMore: 'button.more',
  /// The rail header's "Clear all".
  facetClear: '.facets .phead button.clear',
  /// The rail's own value filter (fleet-ui's SearchInput), which is the
  /// focusable immediately before the first group header.
  facetFilterInput: '.facets .inp-wrap input',

  // -- chrome ---------------------------------------------------------
  /// crates/trawl-web-ui/src/components/meta_strip.rs — one active
  /// filter's chip. Counting these is how a spec says "exactly one
  /// filter was added": the URL payload is base64 and says nothing on
  /// sight.
  filterChip: '.meta-chips .chip',
  /// crates/trawl-web-ui/src/components/meta_strip.rs — a filter chip's
  /// remove control, named after the filter it drops.
  chipRemove: '.meta-chips .chip button.x',
  /// crates/trawl-web-ui/src/components/status_bar.rs — the theme
  /// control. Its visible text is the theme in force and its accessible
  /// name is the theme a press would produce.
  themeControl: '.statusbar button.grp.clickable',
  /// crates/trawl-web-ui/src/components/net_drawer.rs — one schedule
  /// interval preset, an exclusive set carrying aria-pressed.
  intervalChip: '.interval-chips button.interval-chip',
  /// crates/trawl-web-ui/src/components/net_drawer.rs — the drawer
  /// title's rename trigger (fleet-ui's drawer owns `.sd-ttl`).
  netRename: '.sd-ttl button.name',
  /// The input that REPLACES that trigger while a rename is open. The
  /// two never coexist, so a spec reads one of them to say which half
  /// of the swap is on screen.
  netRenameInput: '.sd-ttl input.name-edit',
  /// crates/trawl-web-ui/src/components/net_drawer.rs — one run row of
  /// the drawer's Runs tab. Scoped to the drawer body: the nets table
  /// underneath the drawer renders the very same row shape.
  netRunRow: '.sd-body .tbl-body .tbl-row',
  /// The result preview an expanded run row mounts under itself. One
  /// per open row, which is how a spec says the row expanded ONCE.
  netRunPreview: '.sd-body .run-preview',

  // -- range dialog ---------------------------------------------------
  /// crates/fleet-ui/src/range_dialog.rs RangePanel
  /// — the panel as a modal dialog on fleet-ui's overlay stack.
  rangeDialog: '.dr-pop[role="dialog"]',
  /// Its scrim, a sibling before the panel. Dismissal is mousedown.
  rangeScrim: '.daterange .scrim',
  /// fleet-ui's Segmented option inside the dialog — the strip is the
  /// panel's first control, so it is where initial focus lands.
  segmentedOption: '.dr-pop .seg-opt',
  /// The Absolute tab's From label, paired to `.dr-from` by `for`.
  dateRangeFromLabel: '.dr-pop .fld:has(.dr-from) label[for]',
  /// Its To twin.
  dateRangeToLabel: '.dr-pop .fld:has(.dr-to) label[for]',
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
  historyExport: 'Export this page',
  historyClear: 'Clear history',
  historyClearConfirm: 'Clear all history',
  historyClearFailed: 'Clear failed',
  historyClearDone: 'History cleared',
  /// crates/fleet-ui/src/pager.rs — exact accessible pager button names.
  historyPrev: '← Prev',
  historyNext: 'Next →',
  historyCancel: 'Cancel',

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
  /// crates/trawl-web-ui/src/components/editor_wrap.rs — app-supplied
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

  // -- names that are Rust format strings (ADR-0029) ------------------
  // Each of these is the format string as the source writes it, filled
  // by `nameFrom` below. Pinning the RESOLVED name would hide the words
  // around the values, which is the half a rename actually breaks.
  /// crates/trawl-web-ui/src/sort_label.rs — an unsorted column.
  sortName: 'Sort by {label}',
  /// Its sorted spellings; the div tables carry no `aria-sort`, so the
  /// direction is in the name.
  sortNameAscending: 'Sort by {label}, ascending',
  sortNameDescending: 'Sort by {label}, descending',
  /// crates/trawl-web-ui/src/components/results_table.rs — the caret
  /// button's name, numbered from 1 in render order.
  resultsExpandName: 'Show details for result {}',
  /// crates/trawl-web-ui/src/components/results_table.rs — a results
  /// column's sort control. Direction is NOT in the name here: the real
  /// `<table>` carries it on the `<th>`'s aria-sort.
  resultsSortName: 'Sort by {column}',
  /// Column labels a sort proof names. `sort_th` takes them as
  /// literals, so a renamed column would otherwise fail a spec for a
  /// reason that reads like a broken sort.
  schemaServiceHeader: 'Service',
  schemaEventsHeader: 'Events',
  serviceFieldHeader: 'Field',
  serviceCardinalityHeader: 'Cardinality',
  /// crates/trawl-web-ui/src/components/results_table.rs — a detail
  /// row's tag, which adds an include filter.
  resultsTagName: 'Include {field_for_label} = {value_for_label}',
  /// crates/trawl-web-ui/src/components/facet_sidebar.rs — the two
  /// value controls.
  facetIncludeName: 'Include {field} = {v}',
  facetExcludeName: 'Exclude {field} = {v}',
  /// Its "+ N more" control, whose visible text stays `+ N more`.
  facetMoreName: 'Show {extra} more values for {field}',
  /// The group header. Its count lives in a span inside the button, so
  /// an unnamed header read as `_time8`; this names the two in words.
  facetGroupName: '{field}, {total} values',
  /// crates/trawl-web-ui/src/components/meta_strip.rs — the chip's
  /// remove control. Positional placeholders, as the source writes it.
  chipRemoveName: 'Remove filter {} = {}',
  /// crates/trawl-web-ui/src/pages/schema.rs — the row quick actions.
  schemaSearchName: 'Search {name_label_search}',
  schemaTailName: 'Live tail {name_label_tail}',
  /// crates/trawl-web-ui/src/components/status_bar.rs — the theme
  /// control. The name opens with the visible word (the theme in force)
  /// and then says what a press produces, so the label is inside the
  /// name.
  themeSwitchName: 'Theme {current}: switch to {next} theme',
  /// crates/trawl-web-ui/src/components/net_drawer.rs — the drawer
  /// title's rename trigger.
  netRenameName: 'Rename {name}',
  /// crates/trawl-web-ui/src/components/net_drawer.rs — the rename
  /// editor's input. It replaces the heading it edits, so the name it is
  /// editing is nowhere on screen to label it.
  netRenameInputName: 'New name for {name}',
  /// crates/fleet-ui/src/range_dialog.rs — the range
  /// dialog's own name. Not a format string.
  rangeDialogName: 'Time range',
  /// crates/trawl-web-ui/src/components/net_drawer.rs — what stands in
  /// for the schedule form until a net has a schedule. The interval
  /// presets live behind it.
  addScheduleButton: '+ Add Schedule',

  // -- the DSL shapes the stub dispatches on --------------------------
  // Not copy the user sees: the substrings `harness/server.mjs` keys
  // `/api/v1/query` on under the `corpus` scenario. A spec asserts the
  // drawer's reads by these, and a drifted builder would fall through to
  // the pipeline catch-all and render an error arm that reads like a
  // broken app rather than a stale fixture.
  queryShapeTopValues: '| top 10 ',
  queryShapeCardinality: '| stats dc(',
  queryShapeTimechart: '| timechart span=1h count()',
} as const;

/** Fill a `COPY` name that is a Rust format string.
 *
 * The accessible names above are `format!` templates, pinned verbatim
 * against the source that writes them. Each `{…}` placeholder is
 * replaced, left to right, by the next value given — so a spec never
 * spells out the words around the values, and a reworded label fails the
 * drift guard instead of failing a spec for an unrelated-looking reason.
 */
export function nameFrom(template: string, ...values: string[]): string {
  return values.reduce((out, v) => out.replace(/\{[^}]*\}/, v), template);
}
