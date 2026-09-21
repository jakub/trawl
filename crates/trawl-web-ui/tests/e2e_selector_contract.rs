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
//! "hook" substring the selector is built from still appears in a
//! comment-stripped view of the Rust (or fleet-ui) source file that
//! emits it, so prose describing markup cannot answer for markup that
//! was deleted. Same `include_str!`-and-scan
//! idiom as `crates/fleet-ui/tests/component_class_contract.rs`, aimed
//! at the e2e suite's selector sheet instead of a stylesheet.
//!
//! This cannot run the SPA or `CodeMirror`, so it is no substitute for
//! running the suite (`cargo xtask e2e`); it only proves the selectors
//! and copy the suite is built from still match the source.

const SELECTORS_TS: &str = include_str!("../e2e/selectors.ts");

const EDITOR_RS: &str = include_str!("../src/components/editor.rs");
const RESULTS_TABLE_RS: &str = include_str!("../src/components/results_table.rs");
const EXACT_TABLE_RS: &str = include_str!("../src/components/exact_table.rs");
const CAT_CHART_RS: &str = include_str!("../src/components/cat_chart.rs");
const HISTORY_RS: &str = include_str!("../src/pages/history.rs");
const LAYOUT_RS: &str = include_str!("../src/pages/layout.rs");
const RANGE_DIALOG_RS: &str = include_str!("../../fleet-ui/src/range_dialog.rs");
const EDITOR_WRAP_RS: &str = include_str!("../src/components/editor_wrap.rs");
const MALFORMED_NOTICE_RS: &str = include_str!("../src/components/malformed_notice.rs");
const META_STRIP_RS: &str = include_str!("../src/components/meta_strip.rs");
const SEARCH_URL_RS: &str = include_str!("../src/search_url.rs");
const EXPORT_MODAL_RS: &str = include_str!("../src/components/export_modal.rs");
const MODAL_SHELL_RS: &str = include_str!("../../fleet-ui/src/modal/shell.rs");
const TOAST_RUNTIME_RS: &str = include_str!("../../fleet-ui/src/toast/runtime.rs");
const TOAST_KINDS_RS: &str = include_str!("../../fleet-ui/src/toast/kinds.rs");
const LOADED_COMPONENT_RS: &str = include_str!("../../fleet-ui/src/loaded/component.rs");
const LOADED_STATE_RS: &str = include_str!("../../fleet-ui/src/loaded/state.rs");
const SIDEBAR_RS: &str = include_str!("../../fleet-ui/src/sidebar.rs");
const SECTION_RS: &str = include_str!("../src/state/section.rs");
const DRAWER_RS: &str = include_str!("../../fleet-ui/src/drawer.rs");
const FIELD_CASE_DRAWER_RS: &str = include_str!("../src/components/field_case_drawer.rs");
const REPIN_FLOW_RS: &str = include_str!("../src/repin_flow.rs");
const PALETTE_RS: &str = include_str!("../../fleet-ui/src/command_palette.rs");
const KBD_RS: &str = include_str!("../../fleet-ui/src/kbd.rs");
const TOPBAR_RS: &str = include_str!("../../fleet-ui/src/topbar.rs");
const ATMOSPHERE_RS: &str = include_str!("../../fleet-ui/src/atmosphere/component.rs");
const MENU_RS: &str = include_str!("../../fleet-ui/src/menu.rs");
const TABS_RS: &str = include_str!("../../fleet-ui/src/tabs.rs");
const COPY_BUTTON_RS: &str = include_str!("../../fleet-ui/src/copy_button.rs");
const SCHEMA_RS: &str = include_str!("../src/pages/schema.rs");
const NETS_RS: &str = include_str!("../src/pages/nets.rs");
const RUNS_RS: &str = include_str!("../src/pages/runs.rs");
const SORT_TH_RS: &str = include_str!("../src/components/sort_th.rs");
const SORT_LABEL_RS: &str = include_str!("../src/sort_label.rs");
const SERVICE_DRAWER_RS: &str = include_str!("../src/components/service_drawer.rs");
const NET_DRAWER_RS: &str = include_str!("../src/components/net_drawer.rs");
const FACET_SIDEBAR_RS: &str = include_str!("../src/components/facet_sidebar.rs");
const STATUS_BAR_RS: &str = include_str!("../src/components/status_bar.rs");
const HISTOGRAM_RS: &str = include_str!("../src/components/histogram.rs");
const DRAWER_QUERY_RS: &str = include_str!("../src/drawer_query.rs");
const SEARCH_INPUT_RS: &str = include_str!("../../fleet-ui/src/search_input.rs");
const SEGMENTED_COMPONENT_RS: &str = include_str!("../../fleet-ui/src/segmented/component.rs");
const SCHEDULE_EDIT_RS: &str = include_str!("../src/schedule_edit.rs");
const API_MOD_RS: &str = include_str!("../src/api/mod.rs");

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

const HEALTH_RS: &str = include_str!("../src/pages/health.rs");

const CONTRACTS: &[Contract] = &[
    Contract {
        assignment: "savePreview: '.modal .preview',",
        source_path: "src/components/save_as_net_modal.rs",
        source: include_str!("../src/components/save_as_net_modal.rs"),
        hook: "class=\"preview\"",
    },
    Contract {
        assignment: "savePreview: '.modal .preview',",
        source_path: "../fleet-ui/src/modal/shell.rs",
        source: MODAL_SHELL_RS,
        hook: "\"modal\"",
    },
    Contract {
        assignment: "helpLink: 'nav.rail .bot a[title=\"Help\"]',",
        source_path: "../fleet-ui/src/sidebar.rs",
        source: SIDEBAR_RS,
        hook: "class=\"bot\"",
    },
    Contract {
        assignment: "helpLink: 'nav.rail .bot a[title=\"Help\"]',",
        source_path: "src/pages/layout.rs",
        source: LAYOUT_RS,
        hook: "title=\"Help\" href=\"https://trawl.sh\"",
    },
    Contract {
        assignment: "paletteTrigger: '.topbar button.jump',",
        source_path: "../fleet-ui/src/topbar.rs",
        source: TOPBAR_RS,
        hook: "class=\"jump\"",
    },
    Contract {
        assignment: "paletteDialog: '[role=\"dialog\"][aria-label=\"Command palette\"]',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "aria-label=\"Command palette\"",
    },
    Contract {
        assignment: "paletteInput: '[role=\"combobox\"][aria-label=\"Find a page\"]',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "aria-label=\"Find a page\"",
    },
    Contract {
        assignment: "paletteList: '[role=\"listbox\"][aria-label=\"Pages\"]',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "aria-label=\"Pages\"",
    },
    Contract {
        assignment: "paletteOption: 'a[role=\"option\"]',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "attr:role=\"option\"",
    },
    Contract {
        assignment: "paletteClose: 'button[aria-label=\"Close command palette\"]',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "aria-label=\"Close command palette\"",
    },
    Contract {
        assignment: "paletteScrim: '.command-palette-scrim',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "class=\"command-palette-scrim\"",
    },
    Contract {
        assignment: "paletteStatus: '.command-palette-status[role=\"status\"]',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "class=\"command-palette-status\"",
    },
    Contract {
        assignment: "palettePath: '.command-palette-path',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "class=\"command-palette-path\"",
    },
    Contract {
        assignment: "paletteLabel: '.command-palette-label',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "class=\"command-palette-label\"",
    },
    Contract {
        assignment: "paletteCurrent: '.command-palette-current',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "class=\"command-palette-current\"",
    },
    Contract {
        assignment: "paletteEmpty: '.command-palette-empty',",
        source_path: "../fleet-ui/src/command_palette.rs",
        source: PALETTE_RS,
        hook: "class=\"command-palette-empty\"",
    },
    Contract {
        assignment: "paletteRailLink: 'nav.rail .grp > a[title]',",
        source_path: "../fleet-ui/src/sidebar.rs",
        source: SIDEBAR_RS,
        hook: "attr:title=label_attr",
    },
    Contract {
        assignment: "navToggle: '.topbar button.nav-toggle',",
        source_path: "../fleet-ui/src/topbar.rs",
        source: TOPBAR_RS,
        hook: "class=\"nav-toggle\"",
    },
    Contract {
        assignment: "sidebarCollapse: 'nav.rail .bot button.collapse',",
        source_path: "../fleet-ui/src/sidebar.rs",
        source: SIDEBAR_RS,
        hook: "class=\"it collapse\"",
    },
    Contract {
        assignment: "topbarCrumb: '.topbar .crumb',",
        source_path: "../fleet-ui/src/topbar.rs",
        source: TOPBAR_RS,
        hook: "class=\"crumb\"",
    },
    Contract {
        assignment: "paletteKbd: '.topbar button.jump .kbd',",
        source_path: "../fleet-ui/src/kbd.rs",
        source: KBD_RS,
        hook: "else { \"kbd\" }",
    },
    Contract {
        assignment: "historyAwayLink: 'nav.rail a[title=\"Schema\"]',",
        source_path: "src/state/section.rs",
        source: SECTION_RS,
        hook: "label: \"Schema\"",
    },
    Contract {
        assignment: "historyPage: '.history-page',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "class=\"page history-page\"",
    },
    Contract {
        assignment: "historyFormat: 'select[aria-label=\"History export format\"]',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "aria-label=\"History export format\"",
    },
    Contract {
        assignment: "historyFilter: 'input[placeholder=\"Filter history…\"]',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "placeholder=\"Filter history…\"",
    },
    Contract {
        assignment: "historyExport: 'Export this page',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "Export this page",
    },
    Contract {
        assignment: "historyClear: 'Clear history',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "Clear history",
    },
    Contract {
        assignment: "historyClearConfirm: 'Clear all history',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "Clear all history",
    },
    Contract {
        assignment: "historyClearFailed: 'Clear failed',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "Clear failed",
    },
    Contract {
        assignment: "historyClearDone: 'History cleared',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "History cleared",
    },
    Contract {
        assignment: "resultsFooter: '.results-footer',",
        source_path: "../fleet-ui/src/pager.rs",
        source: include_str!("../../fleet-ui/src/pager.rs"),
        hook: "class=\"results-footer\"",
    },
    Contract {
        assignment: "resultsSummary: '.results-summary',",
        source_path: "../fleet-ui/src/pager.rs",
        source: include_str!("../../fleet-ui/src/pager.rs"),
        hook: "class=\"results-summary\"",
    },
    Contract {
        assignment: "historyPrev: '← Prev',",
        source_path: "../fleet-ui/src/pager.rs",
        source: include_str!("../../fleet-ui/src/pager.rs"),
        hook: "← Prev",
    },
    Contract {
        assignment: "historyNext: 'Next →',",
        source_path: "../fleet-ui/src/pager.rs",
        source: include_str!("../../fleet-ui/src/pager.rs"),
        hook: "Next →",
    },
    Contract {
        assignment: "historyCancel: 'Cancel',",
        source_path: "../fleet-ui/src/modal/confirm.rs",
        source: include_str!("../../fleet-ui/src/modal/confirm.rs"),
        hook: "Cancel",
    },
    Contract {
        assignment: "healthCheck: '.health-check',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-check\"",
    },
    Contract {
        assignment: "healthQueryScroll: '.health-query-scroll',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-query-scroll tbl-scroll\"",
    },
    Contract {
        assignment: "healthAwayLink: 'nav.rail a[title=\"Schema\"]',",
        source_path: "src/state/section.rs",
        source: SECTION_RS,
        hook: "label: \"Schema\"",
    },
    Contract {
        assignment: "railHealthLink: 'nav.rail a[title=\"Health\"]',",
        source_path: "src/state/section.rs",
        source: SECTION_RS,
        hook: "label: \"Health\"",
    },
    Contract {
        assignment: "railHealthLink: 'nav.rail a[title=\"Health\"]',",
        source_path: "src/state/section.rs",
        source: SECTION_RS,
        hook: "path: \"/settings/health\"",
    },
    Contract {
        assignment: "healthConfirm: '[role=\"alertdialog\"]',",
        source_path: "../../fleet-ui/src/modal/confirm.rs",
        source: include_str!("../../fleet-ui/src/modal/confirm.rs"),
        hook: "role=\"alertdialog\"",
    },
    Contract {
        assignment: "healthAwayLink: 'nav.rail a[title=\"Schema\"]',",
        source_path: "../../fleet-ui/src/sidebar.rs",
        source: SIDEBAR_RS,
        hook: "title=",
    },
    Contract {
        assignment: "healthFooterHot: '.statusbar .grp[title=\"Hot buffer (events / bytes)\"]',",
        source_path: "src/components/status_bar.rs",
        source: STATUS_BAR_RS,
        hook: "title=\"Hot buffer (events / bytes)\"",
    },
    Contract {
        assignment: "healthFooterWal: '.statusbar .wal-measurement',",
        source_path: "src/components/status_bar.rs",
        source: STATUS_BAR_RS,
        hook: "class=\"grp wal-measurement\"",
    },
    Contract {
        assignment: "healthCards: '.health-cards',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-cards\"",
    },
    Contract {
        assignment: "healthPage: '.health-page',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-page\"",
    },
    Contract {
        assignment: "healthSection: '.health-section',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-section\"",
    },
    Contract {
        assignment: "healthCapacity: '.health-capacity',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-capacity\"",
    },
    Contract {
        assignment: "healthLive: '.health-live',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-live\"",
    },
    Contract {
        assignment: "healthLiveState: '.health-live-state',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-live-state\"",
    },
    Contract {
        assignment: "healthIngestion: '.health-ingestion',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-ingestion\"",
    },
    Contract {
        assignment: "healthStorage: '.health-storage',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-storage\"",
    },
    Contract {
        assignment: "healthDiagnostics: '.health-diagnostics',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-diagnostics\"",
    },
    Contract {
        assignment: "healthDiagnosticState: '.health-diagnostic-state',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-diagnostic-state\"",
    },
    Contract {
        assignment: "healthQueries: '.health-queries',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"health-queries tbl fleet-table-frame\"",
    },
    Contract {
        assignment: "healthRefresh: '.health-refresh',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"btn health-refresh\"",
    },
    Contract {
        assignment: "healthQueriesRefresh: '.health-queries-refresh',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "class=\"btn health-queries-refresh\"",
    },
    Contract {
        assignment: "healthRow: 'tr[data-query-id]',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "data-query-id=",
    },
    Contract {
        assignment: "healthOwnRow: 'tr[data-own=\"true\"]',",
        source_path: "src/pages/health.rs",
        source: HEALTH_RS,
        hook: "data-own=",
    },
    Contract {
        assignment: "railHealthLink: 'nav.rail a[title=\"Health\"]',",
        source_path: "../../fleet-ui/src/sidebar.rs",
        source: SIDEBAR_RS,
        hook: "title=",
    },
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
        source_path: "../fleet-ui/src/sidebar.rs",
        source: SIDEBAR_RS,
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
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"daterange\"",
    },
    Contract {
        assignment: "dateRangeTrigger: '.daterange .dr-trigger',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"dr-trigger\"",
    },
    Contract {
        assignment: "realtimeTab: '.dr-pop >> text=Real-time',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"dr-pop\"",
    },
    Contract {
        assignment: "realtimeTab: '.dr-pop >> text=Real-time',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "\"Real-time\"",
    },
    Contract {
        assignment: "liveTailButton: '.rt-hint + .foot .btn-pri',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
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
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"dr-pop\"",
    },
    Contract {
        assignment: "absoluteTab: '.dr-pop >> text=Absolute',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "\"Absolute\"",
    },
    Contract {
        assignment: "dateRangeFrom: '.dr-from',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"dr-from\"",
    },
    Contract {
        assignment: "dateRangeTo: '.dr-to',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"dr-to\"",
    },
    Contract {
        assignment: "dateRangeApply: '.dr-apply',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
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
        assignment: "stopLive: '.tabs button.action.stop-live',",
        source_path: "src/pages/search.rs",
        source: include_str!("../src/pages/search.rs"),
        hook: "class=\"action stop-live\"",
    },
    Contract {
        assignment: "histoStrip: '.histo',",
        source_path: "src/components/histogram.rs",
        source: HISTOGRAM_RS,
        hook: "class=\"histo\"",
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
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
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
        assignment: "reservedSetEncoded: '%20%23%26%2F%3A%25%27!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80',",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "\"%20%23%26%2F%3A%25%27!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80\"",
    },
    Contract {
        assignment: "urlNoticeTooLong: 'This link is too long to read.',",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "_ => \"This link is too long to read.\".to_owned(),",
    },
    Contract {
        assignment: "urlNoticeRepairLink: 'Start over',",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "Param::Link => \"Start over\",",
    },
    Contract {
        assignment: "toastError: '.toast.error',",
        source_path: "../fleet-ui/src/toast/runtime.rs",
        source: TOAST_RUNTIME_RS,
        hook: "format!(\"toast {}\", toast.kind.as_class())",
    },
    Contract {
        assignment: "toastError: '.toast.error',",
        source_path: "../fleet-ui/src/toast/kinds.rs",
        source: TOAST_KINDS_RS,
        hook: "Self::Error => \"error\",",
    },
    Contract {
        assignment: "linkTooLongToast: \"Can't open this search: link too long\",",
        source_path: "src/search_url.rs",
        source: SEARCH_URL_RS,
        hook: "\"Can't open this search: link too long\"",
    },
    Contract {
        assignment: "liveTailButtonText: 'Live Tail',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "\"Live Tail\"",
    },
    Contract {
        assignment: "stopLiveText: 'Stop live',",
        source_path: "src/pages/search.rs",
        source: include_str!("../src/pages/search.rs"),
        hook: ">\"Stop live\"<",
    },
    Contract {
        assignment: "draftDirty: 'Edited',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "\"Edited\"",
    },
    Contract {
        assignment: "saveAsNetTool: 'Save as Net',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: ">\"Save as Net\"<",
    },
    Contract {
        assignment: "copyUrlTool: 'Copy search URL',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: ">\"Copy search URL\"<",
    },
    Contract {
        assignment: "toastAny: '.toast',",
        source_path: "../fleet-ui/src/toast/runtime.rs",
        source: TOAST_RUNTIME_RS,
        hook: "format!(\"toast {}\", toast.kind.as_class())",
    },
    Contract {
        assignment: "drawerPanel: '.sd-drawer',",
        source_path: "../fleet-ui/src/drawer.rs",
        source: DRAWER_RS,
        hook: "class=\"sd-drawer\"",
    },
    Contract {
        assignment: "drawerClose: '.sd-x',",
        source_path: "../fleet-ui/src/drawer.rs",
        source: DRAWER_RS,
        hook: "class=\"sd-x\"",
    },
    Contract {
        assignment: "drawerTitle: '.sd-ttl',",
        source_path: "../fleet-ui/src/drawer.rs",
        source: DRAWER_RS,
        hook: "class=\"sd-ttl\"",
    },
    Contract {
        assignment: "fieldCase: '.fc-case',",
        source_path: "src/components/field_case_drawer.rs",
        source: FIELD_CASE_DRAWER_RS,
        hook: "class=\"fc-case\"",
    },
    // The job block's class is composite in the source, and only the
    // second half is the hook the spec selects on.
    Contract {
        assignment: "fieldCaseJob: '.fc-job',",
        source_path: "src/components/field_case_drawer.rs",
        source: FIELD_CASE_DRAWER_RS,
        hook: "class=\"fc-sec fc-job\"",
    },
    // Not a selector: a timing the specs wait out. Same drift mechanism,
    // because a spec waiting out the WRONG period proves nothing — it
    // would pass whether or not the poll stopped.
    Contract {
        assignment: "repinPollMs: 3000,",
        source_path: "src/repin_flow.rs",
        source: REPIN_FLOW_RS,
        hook: "pub const REPIN_POLL_MS: u32 = 3_000;",
    },
    // -- native controls and the shared menu contract (ADR-0028) ------
    Contract {
        assignment: "atmosphere: '.atmosphere',",
        source_path: "../fleet-ui/src/atmosphere/component.rs",
        source: ATMOSPHERE_RS,
        hook: "class=\"atmosphere\"",
    },
    Contract {
        assignment: "topbarUser: '.topbar button.user',",
        source_path: "../fleet-ui/src/topbar.rs",
        source: TOPBAR_RS,
        hook: "class=\"user\"",
    },
    // Two rows, because the selector spans two files: the panel class is
    // the topbar's, the `role="menu"` node inside it is the shared
    // menu's.
    Contract {
        assignment: "userMenu: '.user-menu [role=\"menu\"]',",
        source_path: "../fleet-ui/src/topbar.rs",
        source: TOPBAR_RS,
        hook: "panel_class=\"user-menu\"",
    },
    Contract {
        assignment: "userMenu: '.user-menu [role=\"menu\"]',",
        source_path: "../fleet-ui/src/menu.rs",
        source: MENU_RS,
        hook: "<div role=\"menu\" aria-label=menu_label",
    },
    Contract {
        assignment: "userMenuItem: '.user-menu [role=\"menuitem\"], .user-menu [role=\"menuitemradio\"]',",
        source_path: "../fleet-ui/src/menu.rs",
        source: MENU_RS,
        hook: "role=\"menuitem\"",
    },
    Contract {
        assignment: "userMenuItem: '.user-menu [role=\"menuitem\"], .user-menu [role=\"menuitemradio\"]',",
        source_path: "../fleet-ui/src/menu.rs",
        source: MENU_RS,
        hook: "role=\"menuitemradio\"",
    },
    Contract {
        assignment: "netAction: '.net-actions button',",
        source_path: "src/pages/nets.rs",
        source: NETS_RS,
        hook: "class=\"row-menu net-actions\"",
    },
    Contract {
        assignment: "workspaceTab: '.tabs [role=\"tab\"]',",
        source_path: "../fleet-ui/src/tabs.rs",
        source: TABS_RS,
        hook: "class=\"tabs\"",
    },
    Contract {
        assignment: "workspaceTab: '.tabs [role=\"tab\"]',",
        source_path: "../fleet-ui/src/tabs.rs",
        source: TABS_RS,
        hook: "role=\"tab\"",
    },
    Contract {
        assignment: "drawerTab: '.sd-tabs [role=\"tab\"]',",
        source_path: "../fleet-ui/src/tabs.rs",
        source: TABS_RS,
        hook: "class=\"sd-tabs\"",
    },
    Contract {
        assignment: "drawerTab: '.sd-tabs [role=\"tab\"]',",
        source_path: "../fleet-ui/src/tabs.rs",
        source: TABS_RS,
        hook: "role=\"tab\"",
    },
    Contract {
        assignment: "modalClose: '.modal .m-hd button.x',",
        source_path: "../fleet-ui/src/modal/shell.rs",
        source: MODAL_SHELL_RS,
        hook: "class=\"m-hd\"",
    },
    Contract {
        assignment: "modalClose: '.modal .m-hd button.x',",
        source_path: "../fleet-ui/src/modal/shell.rs",
        source: MODAL_SHELL_RS,
        hook: "class=\"x\"",
    },
    Contract {
        assignment: "toastDismiss: '.toast button.x',",
        source_path: "../fleet-ui/src/toast/runtime.rs",
        source: TOAST_RUNTIME_RS,
        hook: "class=\"x\"",
    },
    Contract {
        assignment: "editorTool: '.editor-tools button.tool',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"editor-tools\"",
    },
    Contract {
        assignment: "editorTool: '.editor-tools button.tool',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"tool\"",
    },
    // The Share tool is a CopyButton in bare mode, whose whole markup is
    // fleet-ui's: if that ever stops being a native button, the
    // `button.tool` half of the selector matches one element instead of
    // two and the spec's propagation proof evaporates.
    Contract {
        assignment: "editorTool: '.editor-tools button.tool',",
        source_path: "../fleet-ui/src/copy_button.rs",
        source: COPY_BUTTON_RS,
        hook: "class=cls",
    },
    Contract {
        assignment: "modalCloseName: 'Close dialog',",
        source_path: "../fleet-ui/src/modal/shell.rs",
        source: MODAL_SHELL_RS,
        hook: "aria-label=\"Close dialog\"",
    },
    Contract {
        assignment: "toastDismissName: 'Dismiss notification',",
        source_path: "../fleet-ui/src/toast/runtime.rs",
        source: TOAST_RUNTIME_RS,
        hook: "aria-label=\"Dismiss notification\"",
    },
    Contract {
        assignment: "accountMenuName: 'Account',",
        source_path: "../fleet-ui/src/topbar.rs",
        source: TOPBAR_RS,
        hook: "menu_label=\"Account\"",
    },
    // -- the stretched row control and its neighbours (ADR-0029) ------
    // `tableRow` reaches four pages' rows, so it is pinned in all four:
    // a page that stops rendering `.tbl-row` would otherwise leave its
    // specs matching nothing and passing their `toHaveCount(0)` half.
    Contract {
        assignment: "listSheet: '.list-sheet',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "class=\"list-sheet\"",
    },
    Contract {
        assignment: "runDetail: '.run-detail',",
        source_path: "src/pages/runs.rs",
        source: RUNS_RS,
        hook: "panel_class=\"run-detail\"",
    },
    Contract {
        assignment: "runOpenNet: '.run-detail a.open-net',",
        source_path: "src/pages/runs.rs",
        source: RUNS_RS,
        hook: "class=\"open-net btn-sec\"",
    },
    Contract {
        assignment: "tableRow: '.tbl-body .tbl-row',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "class=\"tbl-body\"",
    },
    Contract {
        assignment: "tableRow: '.tbl-body .tbl-row',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "class=\"tbl-row\"",
    },
    Contract {
        assignment: "tableRow: '.tbl-body .tbl-row',",
        source_path: "src/pages/nets.rs",
        source: NETS_RS,
        hook: "class=\"tbl-row\"",
    },
    Contract {
        assignment: "tableRow: '.tbl-body .tbl-row',",
        source_path: "src/pages/runs.rs",
        source: RUNS_RS,
        hook: "class=\"tbl-row\"",
    },
    Contract {
        assignment: "tableRow: '.tbl-body .tbl-row',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "class=\"tbl-row\"",
    },
    // Same four pages for the control itself: the class is what carries
    // the stretching `::after`, so losing it on one page is losing that
    // page's whole-row pointer target.
    Contract {
        assignment: "rowStretch: '.row-stretch',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "class=\"row-stretch\"",
    },
    Contract {
        assignment: "rowStretch: '.row-stretch',",
        source_path: "src/pages/nets.rs",
        source: NETS_RS,
        hook: "class=\"row-stretch\"",
    },
    Contract {
        assignment: "rowStretch: '.row-stretch',",
        source_path: "src/pages/runs.rs",
        source: RUNS_RS,
        hook: "class=\"row-stretch\"",
    },
    Contract {
        assignment: "rowStretch: '.row-stretch',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "class=\"row-stretch\"",
    },
    Contract {
        assignment: "schemaQuickAction: '.row-act .qa',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "class=\"row-act\"",
    },
    Contract {
        assignment: "schemaQuickAction: '.row-act .qa',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "class=\"qa\"",
    },
    // Two files: the header band belongs to the page, the control
    // inside it to the shared helper.
    Contract {
        assignment: "tableSortControl: '.fleet-table thead .th.sortable button',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "<thead>",
    },
    Contract {
        assignment: "tableSortControl: '.fleet-table thead .th.sortable button',",
        source_path: "src/pages/nets.rs",
        source: NETS_RS,
        hook: "<thead>",
    },
    Contract {
        assignment: "tableSortControl: '.fleet-table thead .th.sortable button',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "class=\"fleet-table schema-table\"",
    },
    Contract {
        assignment: "tableSortControl: '.fleet-table thead .th.sortable button',",
        source_path: "src/pages/nets.rs",
        source: NETS_RS,
        hook: "class=\"fleet-table nets-table\"",
    },
    Contract {
        assignment: "tableSortControl: '.fleet-table thead .th.sortable button',",
        source_path: "src/components/sort_th.rs",
        source: SORT_TH_RS,
        hook: "class=\"th sortable\"",
    },
    Contract {
        assignment: "historySaveAsNet: 'button.link',",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "class=\"link\"",
    },
    // -- results table --------------------------------------------------
    Contract {
        assignment: "resultsRow: '.results-table tbody tr',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"results-table\"",
    },
    Contract {
        assignment: "resultsRow: '.results-table tbody tr',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "<tbody>",
    },
    Contract {
        assignment: "resultsExpandControl: '.results-table td.exp-col button.row-stretch',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"exp-col\"",
    },
    Contract {
        assignment: "resultsExpandControl: '.results-table td.exp-col button.row-stretch',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"row-stretch\"",
    },
    Contract {
        assignment: "resultsDetailCell: '.results-table td.detail',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"detail\"",
    },
    Contract {
        assignment: "resultsDetailTag: '.results-table td.detail button.tag',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"tag\"",
    },
    Contract {
        assignment: "resultsSelectedRow: '.results-table tbody tr.selected',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class:selected=",
    },
    // The panel id, which is also the target of the caret's
    // `aria-controls` and of the "Jump to details" link. Pinned in the
    // table rather than the page: the docked drawer is rendered beside
    // the rows it describes, not from the page body.
    Contract {
        assignment: "inspector: '#search-inspector',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "panel_id=\"search-inspector\"",
    },
    Contract {
        assignment: "inspectorTag: '#search-inspector button.tag',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"tag\"",
    },
    Contract {
        assignment: "resultsSortHeader: '.results-table th.sortable',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"sortable\"",
    },
    // The direction attribute the spec reads off that header. Pinned
    // separately: a `<th class="sortable">` with no `aria-sort` is
    // exactly mutation 19, and the selector alone cannot see it.
    Contract {
        assignment: "resultsSortHeader: '.results-table th.sortable',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "aria-sort=move ||",
    },
    Contract {
        assignment: "resultsSortControl: '.results-table th.sortable button.th-sort',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "class=\"th-sort\"",
    },
    // -- service drawer -------------------------------------------------
    Contract {
        assignment: "serviceFieldRow: '.sd-fields .sf-row',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "class=\"sd-fields\"",
    },
    // The row class is computed (`sf-row` / `sf-row open`), so the hook
    // is the literal inside that closure rather than an attribute.
    Contract {
        assignment: "serviceFieldRow: '.sd-fields .sf-row',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "\"sf-row\"",
    },
    // The service drawer's field row uses the shared `rowStretch`
    // selector, so its own control is pinned here rather than under a
    // second name.
    Contract {
        assignment: "rowStretch: '.row-stretch',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "class=\"row-stretch\"",
    },
    Contract {
        assignment: "serviceDegradedBadge: 'button.deg-btn',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "class=\"deg-btn\"",
    },
    Contract {
        assignment: "serviceFieldSortControl: '.sf-hd .th.sortable button',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "class=\"sf-hd\"",
    },
    Contract {
        assignment: "serviceFieldSortControl: '.sf-hd .th.sortable button',",
        source_path: "src/components/sort_th.rs",
        source: SORT_TH_RS,
        hook: "class=\"th sortable\"",
    },
    Contract {
        assignment: "serviceTopField: '.topfields .tf button.fn',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "class=\"topfields\"",
    },
    Contract {
        assignment: "serviceTopField: '.topfields .tf button.fn',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "class=\"tf\"",
    },
    Contract {
        assignment: "serviceTopField: '.topfields .tf button.fn',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "class=\"fn row-stretch\"",
    },
    // -- facet rail -----------------------------------------------------
    Contract {
        assignment: "facetGroup: '.facets .g',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"facets\"",
    },
    Contract {
        assignment: "facetGroup: '.facets .g',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"g\"",
    },
    Contract {
        assignment: "facetGroupHeader: 'button.g-hd',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"g-hd\"",
    },
    Contract {
        assignment: "facetValue: '.vals .v',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"vals\"",
    },
    Contract {
        assignment: "facetValue: '.vals .v',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"v\"",
    },
    Contract {
        assignment: "facetValueName: '.n',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"n\"",
    },
    Contract {
        assignment: "facetActions: '.act',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"act\"",
    },
    Contract {
        assignment: "facetOp: '.act button.op',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"op\"",
    },
    Contract {
        assignment: "facetMore: 'button.more',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"more\"",
    },
    Contract {
        assignment: "facetClear: '.facets .phead button.clear',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"phead\"",
    },
    Contract {
        assignment: "facetClear: '.facets .phead button.clear',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "class=\"clear\"",
    },
    Contract {
        assignment: "facetFilterInput: '.facets .inp-wrap input',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "<SearchInput value=needle",
    },
    Contract {
        assignment: "facetFilterInput: '.facets .inp-wrap input',",
        source_path: "../fleet-ui/src/search_input.rs",
        source: SEARCH_INPUT_RS,
        hook: "class=\"inp-wrap\"",
    },
    Contract {
        assignment: "scopeStrip: '.scope',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"scope\"",
    },
    Contract {
        assignment: "scopeFacts: '.scope-facts',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"scope-facts\"",
    },
    Contract {
        assignment: "scopeCount: '.scope-count',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"scope-count\"",
    },
    Contract {
        assignment: "scopeExecution: '.scope-execution',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"scope-execution\"",
    },
    Contract {
        assignment: "scopeStarted: '.scope-started',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"scope-started\"",
    },
    Contract {
        assignment: "draftState: '.console-hd .draft',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"console-hd\"",
    },
    Contract {
        assignment: "draftState: '.console-hd .draft',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"draft dirty\"",
    },
    Contract {
        assignment: "viewControl: '.tabs button.action.view',",
        source_path: "src/pages/search.rs",
        source: include_str!("../src/pages/search.rs"),
        hook: "class=\"action view\"",
    },
    Contract {
        assignment: "viewPanel: '#search-view',",
        source_path: "src/pages/search.rs",
        source: include_str!("../src/pages/search.rs"),
        hook: "id=\"search-view\"",
    },
    Contract {
        assignment: "exactTable: '.results-table.exact',",
        source_path: "src/components/exact_table.rs",
        source: EXACT_TABLE_RS,
        hook: "class=\"results-table exact\"",
    },
    Contract {
        assignment: "groupSearch: '.results-table.exact button.grp-search',",
        source_path: "src/components/exact_table.rs",
        source: EXACT_TABLE_RS,
        hook: "class=\"grp-search\"",
    },
    Contract {
        assignment: "catChart: '.cat-chart',",
        source_path: "src/components/cat_chart.rs",
        source: CAT_CHART_RS,
        hook: "class=\"cat-chart\"",
    },
    // Two halves, since the class alone reaches both bypasses.
    Contract {
        assignment: "skipToQuery: 'a.skip-link[href=\"#search-query\"]',",
        source_path: "src/pages/search.rs",
        source: include_str!("../src/pages/search.rs"),
        hook: "class=\"skip-link\"",
    },
    Contract {
        assignment: "skipToQuery: 'a.skip-link[href=\"#search-query\"]',",
        source_path: "src/pages/search.rs",
        source: include_str!("../src/pages/search.rs"),
        hook: "href=\"#search-query\"",
    },
    Contract {
        assignment: "filterChip: '.meta-chips .chip',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"meta-chips\"",
    },
    Contract {
        assignment: "filterChip: '.meta-chips .chip',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"chip\"",
    },
    // -- chrome ---------------------------------------------------------
    Contract {
        assignment: "chipRemove: '.meta-chips .chip button.x',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"meta-chips\"",
    },
    Contract {
        assignment: "chipRemove: '.meta-chips .chip button.x',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "class=\"x\"",
    },
    Contract {
        assignment: "statusLabel: '.statusbar .status-label',",
        source_path: "src/components/status_bar.rs",
        source: STATUS_BAR_RS,
        hook: "class=\"strong status-label\"",
    },
    Contract {
        assignment: "footerCount: '.statusbar .grp.count',",
        source_path: "src/components/status_bar.rs",
        source: STATUS_BAR_RS,
        hook: "class=\"grp count\"",
    },
    Contract {
        assignment: "intervalChip: '.interval-chips button.interval-chip',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"interval-chips\"",
    },
    // The chip's class is computed, so the hook is the literal the
    // closure writes when the preset is not the chosen one.
    Contract {
        assignment: "intervalChip: '.interval-chips button.interval-chip',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "\"interval-chip\"",
    },
    Contract {
        assignment: "netRename: '.sd-ttl button.name',",
        source_path: "../fleet-ui/src/drawer.rs",
        source: DRAWER_RS,
        hook: "class=\"sd-ttl\"",
    },
    Contract {
        assignment: "netRename: '.sd-ttl button.name',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"name\"",
    },
    Contract {
        assignment: "netRenameInput: '.sd-ttl input.name-edit',",
        source_path: "../fleet-ui/src/drawer.rs",
        source: DRAWER_RS,
        hook: "class=\"sd-ttl\"",
    },
    Contract {
        assignment: "netRenameInput: '.sd-ttl input.name-edit',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"name-edit\"",
    },
    Contract {
        assignment: "netRunRow: '.sd-body .run-preview-table > tbody > .tbl-row',",
        source_path: "../fleet-ui/src/drawer.rs",
        source: DRAWER_RS,
        hook: "class=\"sd-body\"",
    },
    Contract {
        assignment: "netRunRow: '.sd-body .run-preview-table > tbody > .tbl-row',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"fleet-table run-preview-table\"",
    },
    Contract {
        assignment: "netRunRow: '.sd-body .run-preview-table > tbody > .tbl-row',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "</tr></thead><tbody>",
    },
    Contract {
        assignment: "netRunRow: '.sd-body .run-preview-table > tbody > .tbl-row',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"tbl-row\"",
    },
    Contract {
        assignment: "netRunPreview: '.sd-body .run-preview',",
        source_path: "../fleet-ui/src/drawer.rs",
        source: DRAWER_RS,
        hook: "class=\"sd-body\"",
    },
    Contract {
        assignment: "netRunPreview: '.sd-body .run-preview',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"run-preview\"",
    },
    // -- range dialog ---------------------------------------------------
    Contract {
        assignment: "rangeDialog: '.dr-pop[role=\"dialog\"]',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"dr-pop\"",
    },
    Contract {
        assignment: "rangeDialog: '.dr-pop[role=\"dialog\"]',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "role=\"dialog\"",
    },
    Contract {
        assignment: "rangeScrim: '.daterange .scrim',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"daterange\"",
    },
    Contract {
        assignment: "rangeScrim: '.daterange .scrim',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"scrim\"",
    },
    Contract {
        assignment: "segmentedOption: '.dr-pop .seg-opt',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "class=\"dr-pop\"",
    },
    Contract {
        assignment: "segmentedOption: '.dr-pop .seg-opt',",
        source_path: "../fleet-ui/src/segmented/component.rs",
        source: SEGMENTED_COMPONENT_RS,
        hook: "class=\"seg-opt\"",
    },
    Contract {
        assignment: "dateRangeFromLabel: '.dr-pop .fld:has(.dr-from) label[for]',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "for=ids.get_value().0",
    },
    Contract {
        assignment: "dateRangeToLabel: '.dr-pop .fld:has(.dr-to) label[for]',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "for=ids.get_value().1",
    },
    // -- accessible names that are Rust format strings ------------------
    Contract {
        assignment: "sortName: 'Sort by {label}',",
        source_path: "src/sort_label.rs",
        source: SORT_LABEL_RS,
        hook: "format!(\"Sort by {label}\")",
    },
    Contract {
        assignment: "sortNameAscending: 'Sort by {label}, ascending',",
        source_path: "src/sort_label.rs",
        source: SORT_LABEL_RS,
        hook: "format!(\"Sort by {label}, ascending\")",
    },
    Contract {
        assignment: "sortNameDescending: 'Sort by {label}, descending',",
        source_path: "src/sort_label.rs",
        source: SORT_LABEL_RS,
        hook: "format!(\"Sort by {label}, descending\")",
    },
    Contract {
        assignment: "resultsExpandName: 'Show details for result {}',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "\"Show details for result {}\"",
    },
    Contract {
        assignment: "resultsSortName: 'Sort by {column}',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "format!(\"Sort by {name}\")",
    },
    Contract {
        assignment: "schemaServiceHeader: 'Service',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "\"Service\"",
    },
    Contract {
        assignment: "schemaEventsHeader: 'Events',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "\"Events\"",
    },
    Contract {
        assignment: "serviceFieldHeader: 'Field',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "\"Field\"",
    },
    Contract {
        assignment: "serviceCardinalityHeader: 'Cardinality',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "\"Cardinality\"",
    },
    Contract {
        assignment: "resultsTagName: 'Include {field_for_label} = {value_for_label}',",
        source_path: "src/components/results_table.rs",
        source: RESULTS_TABLE_RS,
        hook: "\"Include {field_for_label} = {value_for_label}\"",
    },
    Contract {
        assignment: "facetIncludeName: 'Include {field} = {v}',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "format!(\"Include {field} = {v}\")",
    },
    Contract {
        assignment: "facetExcludeName: 'Exclude {field} = {v}',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "format!(\"Exclude {field} = {v}\")",
    },
    Contract {
        assignment: "facetMoreName: 'Show {extra} more values for {field}',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "format!(\"Show {extra} more values for {field}\")",
    },
    Contract {
        assignment: "facetGroupName: '{field}, {total} values',",
        source_path: "src/components/facet_sidebar.rs",
        source: FACET_SIDEBAR_RS,
        hook: "format!(\"{field}, {total} values\")",
    },
    Contract {
        assignment: "chipRemoveName: 'Remove filter {} = {}',",
        source_path: "src/components/meta_strip.rs",
        source: META_STRIP_RS,
        hook: "\"Remove filter {} = {}\"",
    },
    Contract {
        assignment: "schemaSearchName: 'Search {name_label_search}',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "format!(\"Search {name_label_search}\")",
    },
    Contract {
        assignment: "schemaTailName: 'Live tail {name_label_tail}',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "format!(\"Live tail {name_label_tail}\")",
    },
    Contract {
        assignment: "netRenameName: 'Rename {name}',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "aria-label=move || format!(\"Rename {}\", saved.get().map_or_else(|| \"deleted net\".to_string(), |n| n.name))",
    },
    Contract {
        assignment: "netRenameInputName: 'New name for {name}',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "aria-label=move || format!(\"New name for {}\", saved.get().map_or_else(|| \"deleted net\".to_string(), |n| n.name))",
    },
    Contract {
        assignment: "rangeDialogName: 'Time range',",
        source_path: "../fleet-ui/src/range_dialog.rs",
        source: RANGE_DIALOG_RS,
        hook: "aria-label=\"Time range\"",
    },
    Contract {
        assignment: "addScheduleButton: '+ Add Schedule',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "\"+ Add Schedule\"",
    },
    // -- the schedule form's window controls ----------------------------
    Contract {
        assignment: "windowOption: '[role=\"group\"][aria-labelledby=\"net-window-label\"] .seg-opt',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "aria-labelledby=\"net-window-label\"",
    },
    Contract {
        assignment: "windowOption: '[role=\"group\"][aria-labelledby=\"net-window-label\"] .seg-opt',",
        source_path: "../fleet-ui/src/segmented/component.rs",
        source: SEGMENTED_COMPONENT_RS,
        hook: "class=\"seg-opt\"",
    },
    Contract {
        assignment: "windowSpanInput: '#net-window-span',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "id=\"net-window-span\"",
    },
    Contract {
        assignment: "windowSpanHint: '#net-window-span-help',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "id=\"net-window-span-help\"",
    },
    Contract {
        assignment: "windowLagInput: '#net-lag',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "id=\"net-lag\"",
    },
    Contract {
        assignment: "windowLagHint: '#net-lag-help',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "id=\"net-lag-help\"",
    },
    Contract {
        assignment: "scheduleSaveError: '.net-sched .error-banner',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"net-sched\"",
    },
    Contract {
        assignment: "scheduleSaveError: '.net-sched .error-banner',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"error-banner\" role=\"alert\"",
    },
    // -- the stored run preview's local pager ---------------------------
    Contract {
        assignment: "previewRow: 'table tbody tr',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "<tbody>",
    },
    // The preview mounts fleet-ui's OffsetPager for its own footer, so a
    // spec reads that footer through `resultsFooter` scoped to the
    // preview rather than through a selector of its own.
    Contract {
        assignment: "previewScroll: '.preview-scroll',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"preview-scroll\"",
    },
    Contract {
        assignment: "previewCap: '.preview-cap',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "class=\"preview-cap\"",
    },
    // -- the schedule window form's copy --------------------------------
    Contract {
        assignment: "windowLabel: 'Each run covers',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "\"Each run covers\"",
    },
    Contract {
        assignment: "windowOptionQuery: 'Query text',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "SegmentedOption::new(\"query\", \"Query text\")",
    },
    Contract {
        assignment: "windowOptionSinceLast: 'Since last run',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "SegmentedOption::new(\"since_last\", \"Since last run\")",
    },
    Contract {
        assignment: "windowOptionFixed: 'Fixed span',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "SegmentedOption::new(\"fixed\", \"Fixed span\")",
    },
    Contract {
        assignment: "intervalLabel: 'Run every',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "label=\"Run every\"",
    },
    Contract {
        assignment: "maxRunsLabel: 'Keep schedule running for',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "for=\"net-max-runs\">\"Keep schedule running for\"",
    },
    Contract {
        assignment: "windowSpanLabel: 'Trailing span',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "for=\"net-window-span\">\"Trailing span\"",
    },
    Contract {
        assignment: "windowSpanHint: 'At least 60 seconds.',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "\"At least 60 seconds.\"",
    },
    Contract {
        assignment: "windowLagLabel: 'Late-arrival lag',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "for=\"net-lag\">\"Late-arrival lag\"",
    },
    Contract {
        assignment: "windowQueryHint: 'Runs the saved text as written.',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "Runs the saved text as written.",
    },
    Contract {
        assignment: "windowSinceLastHint: \"Each run covers from the previous run's covered point to the run time.\",",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "Each run covers from the previous run's covered point to the run time.",
    },
    Contract {
        assignment: "windowFixedHint: 'Each run covers the trailing span below, measured from the run time.',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "Each run covers the trailing span below, measured from the run time.",
    },
    Contract {
        assignment: "windowRemovalHint: 'Saving removes the window and lag.',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "Saving removes the window and lag.",
    },
    Contract {
        assignment: "windowRescheduleHint: 'Changing the window or interval may run the schedule immediately.',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "Changing the window or interval may run the schedule immediately.",
    },
    Contract {
        assignment: "windowLagHintText: 'Moves both bounds back by this much. Blank is none.',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "Moves both bounds back by this much. Blank is none.",
    },
    Contract {
        assignment: "netRunAction: '⏱ Run',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "\"⏱ Run\"",
    },
    Contract {
        assignment: "netTriggerAction: 'Trigger run',",
        source_path: "src/pages/nets.rs",
        source: NETS_RS,
        hook: "aria-label=\"Trigger run\"",
    },
    Contract {
        assignment: "previewCapLine: 'This run stored {n} rows; {fetched} were fetched. Paging covers the fetched rows.',",
        source_path: "src/schedule_edit.rs",
        source: SCHEDULE_EDIT_RS,
        hook: "\"This run stored {n} rows; {fetched} were fetched. Paging covers the fetched rows.\"",
    },
    Contract {
        assignment: "aggregateFetchRows: 20000,",
        source_path: "src/fetch_plan.rs",
        source: include_str!("../src/fetch_plan.rs"),
        hook: "pub const AGGREGATE_FETCH_ROWS: usize = 20_000;",
    },
    Contract {
        assignment: "apiStatusText: 'server returned {0}',",
        source_path: "src/api/mod.rs",
        source: API_MOD_RS,
        hook: "#[error(\"server returned {0}\")]",
    },
    // -- the DSL shapes the stub dispatches on --------------------------
    // `e2e_wire_fixture_contract.rs` proves these against the builders by
    // CALLING them, which is the stronger half; these rows keep the
    // selector sheet's own copy honest about which source writes them.
    Contract {
        assignment: "queryShapeTopValues: '| top 10 ',",
        source_path: "src/drawer_query.rs",
        source: DRAWER_QUERY_RS,
        hook: "| top 10 ",
    },
    // Two rows, because `| stats dc(` is assembled: the stage comes from
    // the query's format string and `dc(` from the per-field expression,
    // so no single line of that file carries the substring.
    Contract {
        assignment: "queryShapeCardinality: '| stats dc(',",
        source_path: "src/drawer_query.rs",
        source: DRAWER_QUERY_RS,
        hook: "last=7d | stats {}",
    },
    Contract {
        assignment: "queryShapeCardinality: '| stats dc(',",
        source_path: "src/drawer_query.rs",
        source: DRAWER_QUERY_RS,
        hook: "dc({rendered}) as {alias}",
    },
    Contract {
        assignment: "queryShapeTimechart: '| timechart span=1h count()',",
        source_path: "src/components/service_drawer.rs",
        source: SERVICE_DRAWER_RS,
        hook: "| timechart span=1h count()",
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

/// `src` with its comments removed, so a hook can only be satisfied by
/// something the component actually renders.
///
/// A module doc that explains a contract quotes the markup it describes
/// (`menu.rs`'s doc names `role="menuitem"` in prose), so a raw
/// `contains` let the guard stay green after the attribute was deleted
/// from the button: the prose alone answered for it.
///
/// Block comments are removed first, counting nesting the way rustc
/// does; then any line whose first non-space characters are `//` goes,
/// which covers `//`, `///` and `//!` alike. A trailing comment after
/// code on the same line survives on purpose: stripping it would need
/// to know where string literals end, and `'https://…'` inside a
/// literal is exactly the kind of hook this file pins. String literals
/// stay in for the same reason — `title="Close (Esc)"` is real markup.
/// The block scan does not know string literals either, so a source
/// carrying a literal `/*` would over-strip; none of the files below
/// has one, and a hook that vanished for that reason fails loudly.
fn comment_stripped(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut kept: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if depth > 0 && bytes[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
        } else {
            if depth == 0 {
                // Byte-wise, so a multi-byte character outside a comment
                // is copied through unchanged; inside one every byte but
                // the newline is dropped, which cannot split a character.
                kept.push(bytes[i]);
            } else if bytes[i] == b'\n' {
                // Keep the newline so line-comment stripping below still
                // sees real lines.
                kept.push(b'\n');
            }
            i += 1;
        }
    }
    let out = String::from_utf8(kept).expect("dropping whole comment spans keeps the rest valid");
    out.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn comment_stripped_hides_prose_and_keeps_markup() {
    let doc_comment = "/// role=\"menuitem\"\nfn f() {}\n";
    let inner_doc = "//! role=\"menuitem\"\nfn f() {}\n";
    let line_comment = "// role=\"menuitem\"\nfn f() {}\n";
    let block_comment = "/* role=\"menuitem\" */\nfn f() {}\n";
    let markup = "view! { <button role=\"menuitem\">\"x\"</button> }\n";
    for (label, src) in [
        ("///", doc_comment),
        ("//!", inner_doc),
        ("//", line_comment),
        ("/* */", block_comment),
    ] {
        assert!(
            !comment_stripped(src).contains("role=\"menuitem\""),
            "a hook living only in a {label} comment must not satisfy the \
             drift guard — prose is not markup"
        );
    }
    assert!(
        comment_stripped(markup).contains("role=\"menuitem\""),
        "a hook in real markup must survive stripping"
    );
    // Nesting, and a hook that shares a line with a trailing comment.
    assert!(
        !comment_stripped("/* a /* b */ role=\"menuitem\" */\n").contains("role=\"menuitem\""),
        "nested block comments must be stripped whole, as rustc reads them"
    );
    assert!(
        comment_stripped("<button role=\"menuitem\"> // why\n").contains("role=\"menuitem\""),
        "a trailing comment after code must not take the code with it"
    );
}

#[test]
fn every_source_hook_still_exists() {
    for c in CONTRACTS {
        assert!(
            comment_stripped(c.source).contains(c.hook),
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
                    // A NUMBER is an entry too. `TIMING` mirrors Rust
                    // constants the specs wait out, and a predicate that
                    // only recognised quoted values left those invisible
                    // to the very mechanism that catches an unpinned
                    // entry — the one place a silent mirror hurts most,
                    // since a wrong wait passes either way.
                    && (rest.starts_with('\'')
                        || rest.starts_with('"')
                        || rest.starts_with(|ch: char| ch.is_ascii_digit()))
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

/// Every check key the health fixtures carry resolves to a friendly
/// name in `CHECK_NAMES`.
///
/// The page renders an unknown key verbatim, which is the honest
/// fallback but also a silent one: a daemon check the map never grew a
/// name for would read as a raw identifier forever. The fixtures are the
/// set of keys the suite exercises, so they are the set this holds the
/// map to. Lives here rather than in `pages/health.rs` because that
/// module is wasm-only and a `#[cfg(test)]` inside it never runs.
#[test]
fn every_fixture_check_key_has_a_friendly_name() {
    for (name, json) in [
        (
            "health-ok.json",
            include_str!("../e2e/harness/wire/health-ok.json"),
        ),
        (
            "health-unavailable.json",
            include_str!("../e2e/harness/wire/health-unavailable.json"),
        ),
    ] {
        let report: trawl_api::HealthResponse = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("{name} is not a HealthResponse: {e}"));
        let checks = report
            .checks
            .unwrap_or_else(|| panic!("{name} carries no checks"));
        assert!(!checks.is_empty(), "{name} carries no checks");
        for key in checks.keys() {
            assert!(
                HEALTH_RS.contains(&format!("(\"{key}\", \"")),
                "health fixture {name} carries the check `{key}`, which has no \
                 entry in CHECK_NAMES in src/pages/health.rs — it would render \
                 as a raw identifier.",
            );
        }
    }
}

// ---- key uniqueness ---------------------------------------------------
//
// A duplicate key inside one object literal is silent in both languages
// that read these files. TypeScript's checker rejects it, but the specs
// run through a transpiler that only strips types, so the LAST spelling
// wins and the earlier one disappears without a word. That is exactly
// what two seats writing the same sort-header pins produced: two
// `schemaEventsHeader` keys in `COPY`, two `fieldFirstAlphabetically`
// in `CORPUS`, and a `CONTRACTS` table with both copies pinned.
//
// The same NAME in two different objects is fine and deliberate:
// `CORPUS.netId` mirrors `POPULATED.netId` because `corpus` is
// `populated` plus data.

const FIXTURES_TS: &str = include_str!("../e2e/fixtures.ts");

/// `ts` with every string literal and comment blanked to spaces, so a
/// brace inside `'Remove filter {} = {}'` cannot be read as structure.
/// Line breaks survive, because the key scan is line-based.
fn blanked(ts: &str) -> String {
    let chars: Vec<char> = ts.chars().collect();
    let mut out = String::with_capacity(ts.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != c {
                    if chars[i] == '\\' {
                        out.push(' ');
                        i += 1;
                        if i < chars.len() {
                            out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                            i += 1;
                        }
                        continue;
                    }
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                if i < chars.len() {
                    out.push(' ');
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                out.push_str("  ");
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                out.push_str("  ");
                i = (i + 2).min(chars.len());
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The keys declared directly inside one object literal, in source
/// order, repeats included. `decl` is the text up to and including that
/// object's opening brace.
fn top_level_keys(ts: &str, decl: &str) -> Vec<String> {
    let blank = blanked(ts);
    let start = blank
        .find(decl)
        .unwrap_or_else(|| panic!("no `{decl}` in the file"))
        + decl.len();
    let mut depth = 1usize;
    let mut keys = Vec::new();
    for line in blank[start..].lines() {
        if depth == 1 {
            let trimmed = line.trim_start();
            if let Some((key, _)) = trimmed.split_once(':')
                && !key.is_empty()
                && key
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                keys.push(key.to_owned());
            }
        }
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return keys;
                    }
                }
                _ => {}
            }
        }
    }
    panic!("`{decl}` is never closed");
}

fn assert_keys_unique(file: &str, object: &str, keys: &[String]) {
    // Non-vacuous: an object the scan cannot find yields nothing, and
    // a uniqueness check over nothing passes forever. `TIMING` really
    // does hold one key, so one is the floor rather than two.
    assert!(
        !keys.is_empty(),
        "{file}'s {object} yielded no keys — the scan found the wrong \
         object, or the declaration was renamed",
    );
    let mut seen = std::collections::BTreeSet::new();
    let dupes: Vec<&String> = keys.iter().filter(|k| !seen.insert(*k)).collect();
    assert!(
        dupes.is_empty(),
        "{file}'s {object} declares {dupes:?} more than once. The \
         transpiler keeps the LAST spelling and drops the earlier one \
         without a word, so one of the two is already dead.",
    );
}

#[test]
fn every_selector_and_pin_key_is_declared_once() {
    for (file, ts, object, decl) in [
        (
            "e2e/selectors.ts",
            SELECTORS_TS,
            "SEL",
            "export const SEL = {",
        ),
        (
            "e2e/selectors.ts",
            SELECTORS_TS,
            "TIMING",
            "export const TIMING = {",
        ),
        (
            "e2e/selectors.ts",
            SELECTORS_TS,
            "COPY",
            "export const COPY = {",
        ),
        (
            "e2e/fixtures.ts",
            FIXTURES_TS,
            "POPULATED",
            "export const POPULATED = {",
        ),
        (
            "e2e/fixtures.ts",
            FIXTURES_TS,
            "CORPUS",
            "export const CORPUS = {",
        ),
        (
            "e2e/fixtures.ts",
            FIXTURES_TS,
            "SCHEDULE",
            "export const SCHEDULE = {",
        ),
    ] {
        assert_keys_unique(file, object, &top_level_keys(ts, decl));
    }
}

#[test]
fn the_key_scan_reads_nesting_strings_and_comments() {
    let src = "export const A = {\n  one: 'a { b }',\n  // two: 'commented out',\n  \
               nested: { one: 1, deep: { one: 2 } },\n  two: `x`,\n} as const;\n\
               export const B = {\n  one: 'shared name, different object',\n} as const;\n";
    // Braces inside a string, a commented-out key and a nested object's
    // own keys are all invisible to the top-level scan.
    assert_eq!(
        top_level_keys(src, "export const A = {"),
        vec!["one".to_owned(), "nested".to_owned(), "two".to_owned()],
    );
    // The same name in a second object is not a duplicate.
    assert_eq!(
        top_level_keys(src, "export const B = {"),
        vec!["one".to_owned()]
    );

    let dup = "export const A = {\n  one: 'x',\n  two: 'y',\n  one: 'z',\n} as const;\n";
    let keys = top_level_keys(dup, "export const A = {");
    assert_eq!(keys.len(), 3, "the scan must see both spellings: {keys:?}");
    let panicked =
        std::panic::catch_unwind(|| assert_keys_unique("fixture.ts", "A", &keys)).is_err();
    assert!(panicked, "a key declared twice in one object must fail");
}
