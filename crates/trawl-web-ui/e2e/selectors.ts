// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// SINGLE SOURCE for every selector + expected copy string the specs use.
// Every entry is drift-guarded natively by
// `crates/trawl-web-ui/tests/e2e_selector_contract.rs`, which greps the
// *named source file* for the *hook*.

export const SEL = {
  resultsFooter: '.results-footer',
  resultsSummary: '.results-summary',
  paletteTrigger: '.topbar button.jump',
  paletteDialog: '[role="dialog"][aria-label="Command palette"]',
  paletteInput: '[role="combobox"][aria-label="Find a page"]',
  paletteList: '[role="listbox"][aria-label="Pages"]',
  paletteOption: 'a[role="option"]',
  paletteClose: 'button[aria-label="Close command palette"]',
  paletteScrim: '.command-palette-scrim',
  paletteStatus: '.command-palette-status[role="status"]',
  palettePath: '.command-palette-path',
  paletteLabel: '.command-palette-label',
  paletteCurrent: '.command-palette-current',
  paletteEmpty: '.command-palette-empty',
  paletteRailLink: 'nav.rail .grp > a[title]',
  paletteKbd: '.topbar button.jump .kbd',
  helpLink: 'nav.rail .bot a[title="Help"]',
  navToggle: '.topbar button.nav-toggle',
  sidebarCollapse: 'nav.rail .bot button.collapse',
  topbarCrumb: '.topbar .crumb',

  historyAwayLink: 'nav.rail a[title="Schema"]',
  historyPage: '.history-page',
  historyFormat: 'select[aria-label="History export format"]',
  historyFilter: 'input[placeholder="Filter history…"]',

  healthConfirm: '[role="alertdialog"]',
  healthAwayLink: 'nav.rail a[title="Schema"]',
  healthFooterHot: '.statusbar .grp[title="Hot buffer (events / bytes)"]',
  healthPage: '.health-page',
  healthCards: '.health-cards',
  healthFooterWal: '.statusbar .wal-measurement',
  healthSection: '.health-section',
  healthCapacity: '.health-capacity',
  healthLive: '.health-live',
  healthLiveState: '.health-live-state',
  healthIngestion: '.health-ingestion',
  healthStorage: '.health-storage',
  healthDisk: '.health-disk',
  healthDiagnostics: '.health-diagnostics',
  healthDiagnosticState: '.health-diagnostic-state',
  healthQueries: '.health-queries',
  healthCheck: '.health-check',
  healthQueryScroll: '.health-query-scroll',
  healthRefresh: '.health-refresh',
  healthQueriesRefresh: '.health-queries-refresh',
  healthRow: 'tr[data-query-id]',
  healthOwnRow: 'tr[data-own="true"]',
  railHealthLink: 'nav.rail a[title="Health"]',

  liveResultsTable: '.results-table',
  dslEditor: '.dsl-editor',
  /// CodeMirror's own contenteditable content div (standard CM6 class,
  /// not app-owned).
  cmContent: '.dsl-editor .cm-content',
  railHistoryLink: 'nav.rail a[title="History"]',
  notFoundHeading: '.login-card h1',
  notFoundSubtitle: '.login-card .subtitle',
  runButton: 'button.run',
  dateRangeTrigger: '.daterange .dr-trigger',
  realtimeTab: '.dr-pop >> text=Real-time',
  liveTailButton: '.rt-hint + .foot .btn-pri',
  loadHintError: '.results .load-hint.error',
  /// The parts below are chained off the notice (or one of its blocks), so
  /// they carry no ancestor of their own.
  queryErrorNotice: '#search-results .query-error[role="alert"]',
  queryErrorLead: '.query-error-lead',
  queryErrorBlock: '.query-error-block',
  queryErrorMessage: '.query-error-message',
  queryErrorExcerpt: 'figure.query-error-excerpt',
  queryErrorCaption: 'figcaption.query-error-caption',
  queryErrorText: 'pre.query-error-text',
  queryErrorCaret: '.query-error-caret',
  queryErrorCaretPad: '.query-error-caret .query-error-caret-pad',
  queryErrorDiagnostic: '.query-error-diagnostic',
  recoveringNotice: '#search-results .recovering-notice[role="alert"]',
  recoveringNoticeTitle: '.recovering-notice-title',
  recoveringNoticeText: '.recovering-notice-text',
  resourceNotice: '#search-results .resource-notice[role="alert"]',
  resourceNoticeTitle: '.resource-notice-title',
  resourceNoticeText: '.resource-notice-text',
  /// The `role="status"` wrapper stays mounted; the notice inside it comes and goes.
  liveCapNotice: '.live-cap-live[role="status"] .live-cap-notice',
  liveCapNoticeTitle: '.live-cap-notice-title',
  liveCapNoticeText: '.live-cap-notice-text',
  resultsEmptyCell: '.results-empty-cell',
  absoluteTab: '.dr-pop >> text=Absolute',
  dateRangeFrom: '.dr-from',
  dateRangeTo: '.dr-to',
  /// The wrapper carrying the Apply button (fleet-ui's Btn takes no class).
  dateRangeApply: '.dr-apply',
  urlNotice: '.url-notice',
  urlNoticeRepair: '.url-notice-repair',
  urlNoticeRaw: '.url-notice-raw',
  filtersBadChip: '.chip.bad',
  resultsPane: '.results',
  stopLive: '.tabs button.action.stop-live',
  histoStrip: '.histo',
  exportAction: '.tabs .action.export',
  savePreview: '.modal .preview',
  quickRangeOption: '.dr-pop .opt',
  modalPanel: '.modal',
  modalRefusal: '.m-refusal',
  modalResourceNotice: '.modal .resource-notice[role="alert"]',
  toastError: '.toast.error',
  toastAny: '.toast',
  toastSuccess: '.toast.success',
  toastLink: '.toast .link',
  drawerPanel: '.sd-drawer',
  drawerClose: '.sd-x',
  drawerTitle: '.sd-ttl',
  fieldCase: '.fc-case',
  fieldCaseJob: '.fc-job',
  /// The `.topbar` prefix keeps it away
  /// from any other `.user` a page might carry.
  topbarUser: '.topbar button.user',
  atmosphere: '.atmosphere',
  /// The role="menu" node INSIDE the panel
  /// wrapper, so the identity header (which sits outside it) is not part
  /// of what this matches.
  userMenu: '.user-menu [role="menu"]',
  userMenuItem: '.user-menu [role="menuitem"], .user-menu [role="menuitemradio"]',
  netAction: '.net-actions button',
  /// Selecting on the ROLE, not `.t`, is the point: the
  /// trailing View/Export actions live in `.tabs` too and must not be
  /// tabs.
  workspaceTab: '.tabs [role="tab"]',
  drawerTab: '.sd-tabs [role="tab"]',
  modalClose: '.modal .m-hd button.x',
  toastDismiss: '.toast button.x',
  editorTool: '.editor-tools button.tool',

  listSheet: '.list-sheet',
  runDetail: '.run-detail',
  runOpenNet: '.run-detail a.open-net',
  /// Every list page mounts exactly
  /// one `.tbl-body`, so this is unambiguous per page.
  tableRow: '.tbl-body .tbl-row',
  rowStretch: '.row-stretch',
  /// Row-relative, like every selector a spec scopes INSIDE a row:
  /// repeating the row class here would ask for a row inside a row.
  schemaQuickAction: '.row-act .qa',
  tableSortControl: '.fleet-table thead .th.sortable button',
  historySaveAsNet: 'button.link',

  /// The detail row is a SIBLING `<tr>` with no
  /// `.exp-col`, so a count of these is not a count of results.
  resultsRow: '.results-table tbody tr',
  resultsExpandControl: '.results-table td.exp-col button.row-stretch',
  resultsDetailCell: '.results-table td.detail',
  resultsDetailTag: '.results-table td.detail button.tag',
  resultsSelectedRow: '.results-table tbody tr.selected',
  /// Matched by id because that id
  /// is also what the caret's `aria-controls` and the "Jump to details"
  /// link point at.
  inspector: '#search-inspector',
  inspectorTag: '#search-inspector button.tag',
  resultsDetailFieldName: '.results-table td.detail .dg .k',
  inspectorFieldName: '#search-inspector .dg .k',
  resultsDetailNullToggle: '.results-table td.detail button.null-toggle',
  inspectorNullToggle: '#search-inspector button.null-toggle',
  /// `aria-sort` rides
  /// the CELL, so the assertion target is the th and not its button.
  resultsSortHeader: '.results-table th.sortable',
  resultsSortControl: '.results-table th.sortable button.th-sort',

  serviceFieldRow: '.sd-fields .sf-row',
  serviceFieldSample: '.c-sample',
  serviceFieldStat: '.sf-detail .sfd-kv',
  serviceDegradedBadge: 'button.deg-btn',
  serviceFieldSortControl: '.sf-hd .th.sortable button',
  serviceTopField: '.topfields .tf button.fn',

  filterRail: '.facet-panel',
  filterRailSummary: '.facet-panel > summary',
  filterRailContent: '.facet-panel > .facets',
  facetCount: '.facet-count',
  facetHint: '.facets .facets-hint',
  facetGroup: '.facets .g',
  /// Group-relative, like every entry below it: a spec scopes these to
  /// one group or one value row, and repeating the ancestor class would
  /// ask for a group inside a group.
  facetGroupHeader: 'button.g-hd',
  facetValue: '.vals .v',
  facetValueName: '.n',
  facetActions: '.act',
  facetOp: '.act button.op',
  facetMore: 'button.more',
  facetClear: '.facets .phead button.clear',
  facetFilterInput: '.facets .inp-wrap input',

  scopeStrip: '.scope',
  scopeFacts: '.scope-facts',
  scopeCount: '.scope-count',
  scopeExecution: '.scope-execution',
  scopeStarted: '.scope-started',
  draftState: '.console-hd .draft',
  draftDiagnostic: '.editor-wrap .draft-diagnostic',
  draftDiagnosticFirst: '.draft-diagnostic .draft-diagnostic-first',
  draftDiagnosticMore: '.draft-diagnostic details.draft-diagnostic-more',
  draftDiagnosticAdvice: '.editor-wrap .draft-diagnostic.draft-diagnostic-advice',
  draftDiagnosticOmitted: '.draft-diagnostic .draft-diagnostic-omitted',
  viewControl: '.tabs button.action.view',
  viewPanel: '#search-view',
  exactTable: '.results-table.exact',
  groupSearch: '.results-table.exact button.grp-search',
  resultsCap: '.results-cap',
  chartHost: '.chart',
  catChart: '.cat-chart',
  /// Matched by href because both links share the class.
  skipToQuery: 'a.skip-link[href="#search-query"]',
  filterChip: '.meta-chips .chip',
  chipRemove: '.meta-chips .chip button.x',
  statusLabel: '.statusbar .status-label',
  statusIngestRefusing: '.statusbar .grp.ingest-refusing',
  footerCount: '.statusbar .grp.count',
  intervalChip: '.interval-chips button.interval-chip',
  netRename: '.sd-ttl button.name',
  netRenameInput: '.sd-ttl input.name-edit',
  /// Scoped to the drawer body: the nets table
  /// underneath the drawer renders the very same row shape.
  netRunRow: '.sd-body .run-preview-table > tbody > .tbl-row',
  netRunPreview: '.sd-body .run-preview',

  /// Scoped by the group's own label id: the drawer mounts other Segmented strips and
  /// an unscoped `.seg-opt` would reach them.
  windowOption: '[role="group"][aria-labelledby="net-window-label"] .seg-opt',
  windowSpanInput: '#net-window-span',
  windowSpanHint: '#net-window-span-help',
  windowLagInput: '#net-lag',
  windowLagHint: '#net-lag-help',
  scheduleSaveError: '.net-sched .error-banner',

  // Preview-relative, like every selector a spec scopes INSIDE one
  // element: the runs list one level up renders both a table and the
  // same fleet-ui `.results-footer`, so each of these is used through
  // `netRunPreview` and repeating that class here would ask for a
  // preview inside a preview.
  previewRow: 'table tbody tr',
  previewScroll: '.preview-scroll',
  runUnavailable: '.run-unavailable',
  runExhausted: '.run-exhausted',
  previewCap: '.preview-cap',

  rangeDialog: '.dr-pop[role="dialog"]',
  rangeScrim: '.daterange .scrim',
  segmentedOption: '.dr-pop .seg-opt',
  dateRangeFromLabel: '.dr-pop .fld:has(.dr-from) label[for]',
  dateRangeToLabel: '.dr-pop .fld:has(.dr-to) label[for]',
} as const;

/// Timings the specs share with the app. Mirrored here rather than
/// re-derived: a spec that waits out a poll period has to wait out the
/// SAME one the app uses, and `e2e_selector_contract.rs` pins each value
/// against the Rust constant it copies.
export const TIMING = {
  repinPollMs: 3000,
} as const;

export const LIMITS = {
  aggregateFetchRows: 20000,
} as const;

export const COPY = {
  historyExport: 'Export this page',
  historyClear: 'Clear history',
  historyClearConfirm: 'Clear all history',
  historyClearFailed: 'Clear failed',
  historyClearDone: 'History cleared',
  historyPrev: '← Prev',
  historyNext: 'Next →',
  historyCancel: 'Cancel',

  historyH1: 'Search history',
  notFoundHeading: '404',
  notFoundSubtitle: 'That page does not exist.',
  /// crates/fleet-ui/src/loaded/component.rs + state.rs error_copy():
  /// `format!("Couldn't load {what}: {msg}")` — results_table.rs passes
  /// `label="results"`, so the stable substring is this prefix (the `msg`
  /// tail comes from the server's own error envelope and is not pinned
  /// here).
  loadHintErrorPrefix: "Couldn't load results:",
  railHintNothingToCount: 'No field values to count.',
  railHintAggregate: 'Field values are not counted for an aggregate result.',
  liveTailButtonText: 'Live Tail',
  stopLiveText: 'Stop live',
  draftDirty: 'Edited',
  lintMarkerName: 'Query syntax error',
  adviceMarkerName: 'Query advice',
  queryErrorCaption: 'Query sent to server',
  recoveringTitle: 'Search is recovering',
  resourceTitle: 'Query ran out of resources',
  liveCapTitle: 'Live aggregate is partial',
  liveCapAdvice: 'Narrow the query, or switch to Snapshot.',
  streamUnavailable: 'Live stream unavailable. Retry or switch to Snapshot.',
  saveAsNetTool: 'Save as Net',
  copyUrlTool: 'Copy search URL',
  urlNoticeFiltersPrefix: "This link's filters could not be read:",
  urlNoticeRangePrefix: "This link's time range could not be read:",
  urlNoticePagePrefix: "This link's page could not be read:",
  urlNoticeRepairFilters: 'Drop filters',
  urlNoticeRepairRange: 'Use last 15 minutes',
  urlNoticeRepairPage: 'Go to page 1',
  filtersUnreadableChip: 'filters unreadable',
  reservedSet: " #&/:%'!~*()日本語😀",
  /// What the app's encoder writes AND what the browser keeps in
  /// location.search, which are the same string. encodeURIComponent
  /// differs from it at the apostrophe alone.
  reservedSetEncoded: '%20%23%26%2F%3A%25%27!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80',
  emptyQueryRefusal: 'Nothing to export: the query is empty.',
  urlNoticeTooLong: 'This link is too long to read.',
  urlNoticeRepairLink: 'Start over',
  linkTooLongToast: "Can't open this search: link too long",
  modalCloseName: 'Close dialog',
  toastDismissName: 'Dismiss notification',
  accountMenuName: 'Account',

  // Each of these is the format string as the source writes it, filled
  // by `nameFrom` below. Pinning the RESOLVED name would hide the words
  // around the values, which is the half a rename actually breaks.
  sortName: 'Sort by {label}',
  sortNameAscending: 'Sort by {label}, ascending',
  sortNameDescending: 'Sort by {label}, descending',
  resultsExpandName: 'Show details for result {}',
  resultsSortName: 'Sort by {column}',
  /// Column labels a sort proof names. `sort_th` takes them as
  /// literals, so a renamed column would otherwise fail a spec for a
  /// reason that reads like a broken sort.
  schemaServiceHeader: 'Service',
  schemaEventsHeader: 'Events',
  serviceFieldHeader: 'Field',
  serviceCardinalityHeader: 'Cardinality',
  resultsTagName: 'Include {field_for_label} = {value_for_label}',
  facetIncludeName: 'Include {field} = {v}',
  facetExcludeName: 'Exclude {field} = {v}',
  /// `{noun}` is `value` for one, else `values`.
  facetMoreName: 'Show {extra} more listed {noun} for {field}',
  facetGroupName: '{field}, {total} values',
  chipRemoveName: 'Remove filter {} = {}',
  schemaSearchName: 'Search {name_label_search}',
  schemaTailName: 'Live tail {name_label_tail}',
  netRenameName: 'Rename {name}',
  netRenameInputName: 'New name for {name}',
  rangeDialogName: 'Time range',
  addScheduleButton: '+ Add Schedule',

  windowLabel: 'Each run covers',
  windowOptionQuery: 'Query text',
  windowOptionSinceLast: 'Since last run',
  windowOptionFixed: 'Fixed span',
  intervalLabel: 'Run every',
  maxRunsLabel: 'Keep schedule running for',
  windowSpanLabel: 'Trailing span',
  windowSpanHint: 'At least 60 seconds.',
  windowLagLabel: 'Late-arrival lag',
  /// The query-mode one continues into a `<code>`
  /// element, so only the sentence before it is pinned.
  windowQueryHint: 'Runs the saved text as written.',
  windowSinceLastHint: "Each run covers from the previous run's covered point to the run time.",
  windowFixedHint: 'Each run covers the trailing span below, measured from the run time.',
  windowRemovalHint: 'Saving removes the window and lag.',
  windowRescheduleHint: 'Changing the window or interval may run the schedule immediately.',
  windowLagHintText: 'Moves both bounds back by this much. Blank is none.',
  netRunNow: 'Run now',
  netRunNowSpentReason: 'Run limit reached. Raise it in the schedule to run again.',
  runStartedToast: 'Run started · covers {from}–{to} UTC',
  runStartedPlain: 'Run started',
  runViewLink: 'View run',
  runNotStarted: 'Run not started',
  runNowSinceLastLine: 'Run now reads from where the last successful run stopped.',
  runNowFixedLine: 'Run now reads the span ending now; results can overlap earlier runs.',
  runOriginManual: 'Manual',
  previewCapLine: 'This run stored {n} rows; {fetched} were fetched. Paging covers the fetched rows.',
  apiStatusText: 'server returned {0}',

  // Not copy the user sees: the substrings `harness/server.mjs` keys
  // `/api/v1/query` on under the `corpus` scenario. A spec asserts the
  // drawer's reads by these, and a drifted builder would fall through to
  // the pipeline catch-all and render an error arm that reads like a
  // broken app rather than a stale fixture.
  queryShapeTopValues: '| top 10 ',
  queryShapeCardinality: '| stats dc(',
  queryShapeTimechart: '| timechart span=1h count()',
} as const;

export function nameFrom(template: string, ...values: string[]): string {
  return values.reduce((out, v) => out.replace(/\{[^}]*\}/, v), template);
}
