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
const HISTORY_RS: &str = include_str!("../src/pages/history.rs");
const LAYOUT_RS: &str = include_str!("../src/pages/layout.rs");
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
const RAIL_RS: &str = include_str!("../../fleet-ui/src/rail.rs");
const SECTION_RS: &str = include_str!("../src/state/section.rs");
const DRAWER_RS: &str = include_str!("../../fleet-ui/src/drawer.rs");
const FIELD_CASE_DRAWER_RS: &str = include_str!("../src/components/field_case_drawer.rs");
const REPIN_FLOW_RS: &str = include_str!("../src/repin_flow.rs");
const TOPBAR_RS: &str = include_str!("../../fleet-ui/src/topbar.rs");
const MENU_RS: &str = include_str!("../../fleet-ui/src/menu.rs");
const TABS_RS: &str = include_str!("../../fleet-ui/src/tabs.rs");
const ACTIONS_MENU_RS: &str = include_str!("../../fleet-ui/src/actions_menu.rs");
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
const DRAWER_QUERY_RS: &str = include_str!("../src/drawer_query.rs");
const SEARCH_INPUT_RS: &str = include_str!("../../fleet-ui/src/search_input.rs");
const SEGMENTED_COMPONENT_RS: &str = include_str!("../../fleet-ui/src/segmented/component.rs");

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
        hook: "format!(\"toast {}\", t.kind.as_class())",
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
        assignment: "toastAny: '.toast',",
        source_path: "../fleet-ui/src/toast/runtime.rs",
        source: TOAST_RUNTIME_RS,
        hook: "format!(\"toast {}\", t.kind.as_class())",
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
        assignment: "userMenuItem: '.user-menu [role=\"menuitem\"]',",
        source_path: "../fleet-ui/src/menu.rs",
        source: MENU_RS,
        hook: "role=\"menuitem\"",
    },
    Contract {
        assignment: "actionsMenuTrigger: '.actions-wrap button.btn-icon',",
        source_path: "../fleet-ui/src/actions_menu.rs",
        source: ACTIONS_MENU_RS,
        hook: "class=\"actions-wrap\"",
    },
    Contract {
        assignment: "actionsMenuTrigger: '.actions-wrap button.btn-icon',",
        source_path: "../fleet-ui/src/actions_menu.rs",
        source: ACTIONS_MENU_RS,
        hook: "class=\"btn-icon\"",
    },
    Contract {
        assignment: "actionsMenuItem: '.actions-menu [role=\"menuitem\"]',",
        source_path: "../fleet-ui/src/actions_menu.rs",
        source: ACTIONS_MENU_RS,
        hook: "panel_class=\"actions-menu\"",
    },
    Contract {
        assignment: "actionsMenuItem: '.actions-menu [role=\"menuitem\"]',",
        source_path: "../fleet-ui/src/menu.rs",
        source: MENU_RS,
        hook: "role=\"menuitem\"",
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
    // One string, two positions: the trigger's own name and the name of
    // the panel it opens.
    Contract {
        assignment: "actionsName: 'Actions',",
        source_path: "../fleet-ui/src/actions_menu.rs",
        source: ACTIONS_MENU_RS,
        hook: "aria-label=\"Actions\"",
    },
    Contract {
        assignment: "actionsName: 'Actions',",
        source_path: "../fleet-ui/src/actions_menu.rs",
        source: ACTIONS_MENU_RS,
        hook: "menu_label=\"Actions\"",
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
        assignment: "tableSortControl: '.tbl-hd .th.sortable button',",
        source_path: "src/pages/schema.rs",
        source: SCHEMA_RS,
        hook: "class=\"tbl-hd\"",
    },
    Contract {
        assignment: "tableSortControl: '.tbl-hd .th.sortable button',",
        source_path: "src/pages/nets.rs",
        source: NETS_RS,
        hook: "class=\"tbl-hd\"",
    },
    Contract {
        assignment: "tableSortControl: '.tbl-hd .th.sortable button',",
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
        assignment: "themeControl: '.statusbar button.grp.clickable',",
        source_path: "src/components/status_bar.rs",
        source: STATUS_BAR_RS,
        hook: "class=\"statusbar\"",
    },
    Contract {
        assignment: "themeControl: '.statusbar button.grp.clickable',",
        source_path: "src/components/status_bar.rs",
        source: STATUS_BAR_RS,
        hook: "class=\"grp clickable\"",
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
    // -- range dialog ---------------------------------------------------
    Contract {
        assignment: "rangeDialog: '.dr-pop[role=\"dialog\"]',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"dr-pop\"",
    },
    Contract {
        assignment: "rangeDialog: '.dr-pop[role=\"dialog\"]',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "role=\"dialog\"",
    },
    Contract {
        assignment: "rangeScrim: '.daterange .scrim',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"daterange\"",
    },
    Contract {
        assignment: "rangeScrim: '.daterange .scrim',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"scrim\"",
    },
    Contract {
        assignment: "segmentedOption: '.dr-pop .seg-opt',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"dr-pop\"",
    },
    Contract {
        assignment: "segmentedOption: '.dr-pop .seg-opt',",
        source_path: "../fleet-ui/src/segmented/component.rs",
        source: SEGMENTED_COMPONENT_RS,
        hook: "class=\"seg-opt\"",
    },
    Contract {
        assignment: "dateRangeFromLabel: 'label[for=\"dr-from-input\"]',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "for=\"dr-from-input\"",
    },
    Contract {
        assignment: "dateRangeToLabel: 'label[for=\"dr-to-input\"]',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "for=\"dr-to-input\"",
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
        assignment: "themeSwitchName: 'Theme {current}: switch to {next} theme',",
        source_path: "src/components/status_bar.rs",
        source: STATUS_BAR_RS,
        hook: "\"Theme {current}: switch to {next} theme\"",
    },
    Contract {
        assignment: "netRenameName: 'Rename {name}',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "format!(\"Rename {name}\")",
    },
    Contract {
        assignment: "rangeDialogName: 'Time range',",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "aria-label=\"Time range\"",
    },
    Contract {
        assignment: "addScheduleButton: '+ Add Schedule',",
        source_path: "src/components/net_drawer.rs",
        source: NET_DRAWER_RS,
        hook: "\"+ Add Schedule\"",
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
        hook: "dc({field}) as {field}",
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
