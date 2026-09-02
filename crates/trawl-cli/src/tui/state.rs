// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Application state (tabs, focus, queries).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use ratatui::layout::Rect;
use trawl_client::{PaginationMeta, QueryResponse};
use trawl_engine::value::{Column, QueryResult, Value};

/// Cached layout areas from the last render frame, used for mouse hit-testing.
///
/// Rewritten by every draw, so a resize cannot leave hit-testing on a stale layout.
#[derive(Debug, Clone, Default)]
pub struct LayoutAreas {
    /// Tab bar row at the top.
    pub tab_bar: Rect,
    /// Query editor pane (only on Query tab).
    pub editor: Option<Rect>,
    /// Results table pane (only on Query tab).
    pub results: Option<Rect>,
    /// Full-width panel content (History/Schema/Saved/Dashboard tabs).
    pub panel: Option<Rect>,
    /// Status bar at the bottom (reserved for future click handling).
    #[allow(dead_code)]
    pub status: Rect,
    /// Active popup overlay area (if any).
    pub popup: Option<Rect>,
    /// Column header x-ranges for mouse click detection: `(x_start, x_end, original_col_index)`.
    pub column_header_ranges: Vec<(u16, u16, usize)>,
}

/// State for vim-style `/` search within results.
#[derive(Debug, Clone)]
pub struct ResultsSearch {
    /// Search query text.
    pub query: String,
    /// Whether the search input bar is active (accepting typed chars).
    pub input_active: bool,
    /// Matching cell positions: (`row_index`, `col_index`).
    pub matches: Vec<(usize, usize)>,
    /// Index into `matches` for the current highlighted match.
    pub current_match: usize,
}

impl ResultsSearch {
    /// Create a new empty search.
    pub fn new() -> Self {
        Self {
            query: String::new(),
            input_active: true,
            matches: Vec::new(),
            current_match: 0,
        }
    }

    /// Recompute matches against the given result data.
    ///
    /// If a `ColumnConfig` is provided, hidden columns are skipped — searching
    /// invisible data is confusing.
    pub fn update_matches(&mut self, result: &QueryResult, config: Option<&ColumnConfig>) {
        self.matches.clear();
        if self.query.is_empty() {
            return;
        }
        let needle = self.query.to_lowercase();
        for (row_idx, row) in result.rows.iter().enumerate() {
            for (col_idx, value) in row.iter().enumerate() {
                // Skip hidden columns.
                if let Some(cfg) = config
                    && cfg.columns.get(col_idx).is_some_and(|e| e.hidden)
                {
                    continue;
                }
                let display = match value {
                    Value::Null => "NULL".to_owned(),
                    Value::Boolean(b) => b.to_string(),
                    Value::Integer(i) => i.to_string(),
                    Value::Float(f) => format!("{f:.2}"),
                    Value::String(s) => s.clone(),
                    Value::Array(_) => value.to_string(),
                };
                if display.to_lowercase().contains(&needle) {
                    self.matches.push((row_idx, col_idx));
                }
            }
        }
        if self.current_match >= self.matches.len() {
            self.current_match = 0;
        }
    }

    /// Navigate to the next match, wrapping around.
    pub fn next_match(&mut self) {
        if !self.matches.is_empty() {
            self.current_match = (self.current_match + 1) % self.matches.len();
        }
    }

    /// Navigate to the previous match, wrapping around.
    pub fn prev_match(&mut self) {
        if !self.matches.is_empty() {
            self.current_match = (self.current_match + self.matches.len() - 1) % self.matches.len();
        }
    }
}

/// Per-column metadata for visibility, pinning, and width overrides.
#[derive(Debug, Clone)]
pub struct ColumnEntry {
    /// User-set width override. `None` means auto-compute.
    pub width_override: Option<u16>,
    /// Whether this column is hidden.
    pub hidden: bool,
    /// Whether this column is pinned to the left.
    pub pinned: bool,
}

/// Column configuration for the results table (per-result, reset on new query).
#[derive(Debug, Clone)]
pub struct ColumnConfig {
    /// Per-column entries, indexed by original column position.
    pub columns: Vec<ColumnEntry>,
    /// Column-mode cursor (original column index). `None` = not in column mode.
    pub selected: Option<usize>,
}

impl ColumnConfig {
    /// Initialize config from result columns: all visible, none pinned, no overrides.
    pub fn init(columns: &[trawl_engine::value::Column]) -> Self {
        Self {
            columns: columns
                .iter()
                .map(|_| ColumnEntry {
                    width_override: None,
                    hidden: false,
                    pinned: false,
                })
                .collect(),
            selected: None,
        }
    }

    /// Indices of pinned, non-hidden columns in original order.
    pub fn pinned_indices(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, e)| e.pinned && !e.hidden)
            .map(|(i, _)| i)
            .collect()
    }

    /// Indices of non-pinned, non-hidden columns in original order.
    pub fn scrollable_indices(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.pinned && !e.hidden)
            .map(|(i, _)| i)
            .collect()
    }

    /// Display order: pinned first, then scrollable.
    pub fn display_order(&self) -> Vec<usize> {
        let mut order = self.pinned_indices();
        order.extend(self.scrollable_indices());
        order
    }

    /// Number of visible (non-hidden) columns.
    pub fn visible_count(&self) -> usize {
        self.columns.iter().filter(|e| !e.hidden).count()
    }

    /// Move cursor left in display order.
    pub fn move_cursor_left(&mut self) {
        let order = self.display_order();
        if let Some(cur) = self.selected
            && let Some(pos) = order.iter().position(|&i| i == cur)
            && pos > 0
        {
            self.selected = Some(order[pos - 1]);
        }
    }

    /// Move cursor right in display order.
    pub fn move_cursor_right(&mut self) {
        let order = self.display_order();
        if let Some(cur) = self.selected
            && let Some(pos) = order.iter().position(|&i| i == cur)
            && pos + 1 < order.len()
        {
            self.selected = Some(order[pos + 1]);
        }
    }

    /// Toggle pin on the selected column.
    pub fn toggle_pin_selected(&mut self) {
        if let Some(idx) = self.selected
            && let Some(entry) = self.columns.get_mut(idx)
        {
            entry.pinned = !entry.pinned;
        }
    }

    /// Hide the selected column, moving the cursor to the first visible column.
    pub fn hide_selected(&mut self) {
        if let Some(idx) = self.selected
            && self.visible_count() > 1
        {
            if let Some(entry) = self.columns.get_mut(idx) {
                entry.hidden = true;
            }
            let order = self.display_order();
            self.selected = order.into_iter().next();
        }
    }

    /// Adjust selected column width by `delta` (positive = widen, negative = narrow).
    #[allow(clippy::cast_possible_wrap)] // Column widths are always < 120, safe for i32
    pub fn adjust_selected_width(&mut self, delta: i32) {
        if let Some(idx) = self.selected
            && let Some(entry) = self.columns.get_mut(idx)
        {
            let current = i32::from(entry.width_override.unwrap_or(20));
            #[allow(clippy::cast_sign_loss)]
            let new_width = (current + delta).clamp(4, 120) as u16;
            entry.width_override = Some(new_width);
        }
    }

    /// Reset selected column width to auto-compute.
    pub fn reset_selected_width(&mut self) {
        if let Some(idx) = self.selected
            && let Some(entry) = self.columns.get_mut(idx)
        {
            entry.width_override = None;
        }
    }
}

/// Which pane has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Query editor is focused.
    Editor,
    /// Results table is focused.
    Results,
    /// Panel content is focused (History/Schema/Saved tab).
    Panel,
}

/// Top-level navigation tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainTab {
    /// Query editor + results.
    Query,
    /// Query execution history.
    History,
    /// Schema browser.
    Schema,
    /// Saved queries.
    Saved,
    /// Server dashboard (admin only).
    Dashboard,
}

/// State for the admin dashboard tab (polling, caching, permissions).
pub struct DashboardState {
    /// Whether the connected user has admin privileges.
    pub is_admin: bool,
    /// Cached dashboard snapshot (polled every ~1s when tab is active).
    pub cache: Option<trawl_client::DashboardSnapshot>,
    /// Last poll error (cleared on success, shown as staleness indicator).
    pub last_error: Option<String>,
    /// Whether a dashboard request is currently in-flight.
    inflight: bool,
    /// When the inflight flag was set (for timeout detection).
    inflight_since: Option<Instant>,
    /// Counter for dashboard polling (every 10 ticks = ~1s at 100ms poll).
    poll_counter: u32,
}

impl DashboardState {
    pub fn new() -> Self {
        Self {
            is_admin: false,
            cache: None,
            last_error: None,
            inflight: false,
            inflight_since: None,
            poll_counter: 0,
        }
    }

    /// Mark a poll as in-flight.
    pub fn set_inflight(&mut self) {
        self.inflight = true;
        self.inflight_since = Some(Instant::now());
    }

    /// Clear the in-flight flag (call when result arrives).
    pub fn clear_inflight(&mut self) {
        self.inflight = false;
        self.inflight_since = None;
    }

    /// Reset the inflight guard if it's been stuck longer than the timeout.
    /// Returns `true` if the guard was reset (caller should proceed with poll).
    pub fn check_inflight_timeout(&mut self, timeout: std::time::Duration) -> bool {
        if self.inflight {
            if self
                .inflight_since
                .is_some_and(|since| since.elapsed() > timeout)
            {
                tracing::warn!("dashboard poll timed out — resetting inflight guard");
                self.clear_inflight();
                true
            } else {
                false
            }
        } else {
            true
        }
    }

    /// Increment the poll counter, returning `true` when it's time to poll.
    pub fn tick(&mut self) -> bool {
        self.poll_counter += 1;
        if self.poll_counter >= 10 {
            self.poll_counter = 0;
            true
        } else {
            false
        }
    }
}

/// Schema browser state.
///
/// Populated from a single `schema_services()` API call at startup, so the
/// tree is fully navigable immediately with no per-service async loading.
#[derive(Debug, Clone)]
pub struct SchemaBrowser {
    /// Per-service schema from the API response.
    pub services: Vec<trawl_api::ServiceSchema>,
    /// Well-known fields plus anything in more than 80% of services (computed client-side).
    pub common_fields: Vec<CommonField>,
    /// Which services are expanded (by name).
    pub expanded: HashSet<String>,
    /// Cursor index in the flattened visible list.
    pub selected: usize,
    /// Vertical scroll offset.
    pub scroll: usize,
    /// Filter text (type-to-filter).
    pub filter: String,
    /// Whether the filter input is currently accepting keystrokes.
    pub filter_active: bool,
}

impl SchemaBrowser {
    pub fn new(services: Vec<trawl_api::ServiceSchema>) -> Self {
        let common_fields = compute_common_fields(&services);
        Self {
            services,
            common_fields,
            expanded: HashSet::new(),
            selected: 0,
            scroll: 0,
            filter: String::new(),
            filter_active: false,
        }
    }
}

/// A field present across many services (shown once at tree top).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CommonField {
    /// Field name.
    pub name: String,
    /// Data type.
    pub data_type: String,
    /// How many services contain this field.
    pub service_count: usize,
    /// Aggregated null count across services.
    pub null_count: u64,
    /// Aggregated total count across services.
    pub total_count: u64,
    /// Global min value.
    pub min_value: Option<String>,
    /// Global max value.
    pub max_value: Option<String>,
}

/// What is selected in the detail pane (right side).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Unused: ui::schema resolves selection with its own local enum
pub enum DetailSelection {
    /// Nothing selected.
    None,
    /// Common fields header.
    CommonHeader,
    /// A specific common field.
    CommonField { name: String },
    /// A service header row.
    Service { name: String },
    /// A field within a specific service.
    ServiceField { service: String, field: String },
}

/// Envelope fields exempt from the frequency threshold, in leading display order.
///
/// Mirrors `trawl_api::value::WELL_KNOWN_LOG_FIELDS`; a test asserts parity.
const WELL_KNOWN_FIELDS: &[&str] = &["_time", "env", "service", "host", "_severity", "message"];

/// Compute common fields from the service list.
///
/// A field is "common" if some service reports it and it is either well-known
/// or present in more than 80% of services. Well-known fields come first, then
/// threshold-promoted fields alphabetically.
pub fn compute_common_fields(services: &[trawl_api::ServiceSchema]) -> Vec<CommonField> {
    use std::collections::HashMap;

    if services.is_empty() {
        return Vec::new();
    }

    // Count field occurrences and aggregate stats.
    let mut field_stats: HashMap<String, CommonField> = HashMap::new();
    let threshold = (services.len() * 80) / 100;

    for svc in services {
        for col in &svc.columns {
            let entry = field_stats
                .entry(col.name.clone())
                .or_insert_with(|| CommonField {
                    name: col.name.clone(),
                    data_type: col.data_type.clone(),
                    service_count: 0,
                    null_count: 0,
                    total_count: 0,
                    min_value: None,
                    max_value: None,
                });
            entry.service_count += 1;
            entry.null_count += col.null_count;
            entry.total_count += col.total_count;
            // Update global min/max (lexicographic).
            if let Some(ref v) = col.min_value
                && entry.min_value.as_ref().is_none_or(|cur| v < cur)
            {
                entry.min_value = Some(v.clone());
            }
            if let Some(ref v) = col.max_value
                && entry.max_value.as_ref().is_none_or(|cur| v > cur)
            {
                entry.max_value = Some(v.clone());
            }
        }
    }

    let mut common = Vec::new();

    // Well-known fields first.
    for &wk in WELL_KNOWN_FIELDS {
        if let Some(field) = field_stats.remove(wk) {
            common.push(field);
        }
    }

    // Then threshold-promoted fields (alphabetical).
    let mut promoted: Vec<_> = field_stats
        .into_values()
        .filter(|f| f.service_count > threshold)
        .collect();
    promoted.sort_by(|a, b| a.name.cmp(&b.name));
    common.extend(promoted);

    common
}

/// Focus state for the Saved tab's two-pane layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SavedFocus {
    /// Left pane: saved query list.
    #[default]
    List,
    /// Right pane: detail view with run history sub-list.
    Detail,
    /// Right pane: run results table (replaces detail on Enter).
    RunResults,
}

/// State for the saved tab detail pane (right side).
#[derive(Debug, Clone)]
pub struct SavedDetailState {
    /// ID of the saved query whose runs are loaded.
    pub saved_id: i64,
    /// Report runs for the selected saved query.
    pub runs: Vec<trawl_api::ReportRunSummary>,
    /// Total runs on the server (for display).
    pub total_runs: usize,
    /// Currently selected run in the list.
    pub run_selected: usize,
    /// Scroll offset for the run list.
    #[allow(dead_code)] // The run list renderer centers on `run_selected` instead
    pub run_scroll: usize,
    /// Loaded result data for viewing a specific run.
    pub result: Option<trawl_api::value::QueryResult>,
    /// Scroll offset for result rows.
    pub result_scroll: usize,
    /// Whether runs are currently being fetched.
    pub loading: bool,
}

/// Panel state for non-Query tabs (schema, history, saved).
#[derive(Debug, Clone)]
pub struct PanelState {
    /// Schema browser state (populated from API, or `None` before first load).
    pub schema: Option<SchemaBrowser>,
    /// Selected index in the history list.
    pub history_selected: usize,
    /// Selected index in the saved queries list.
    pub saved_selected: usize,
    /// Focus state for the saved tab two-pane layout.
    pub saved_focus: SavedFocus,
    /// Detail pane state for the selected saved query (runs + result).
    pub saved_detail: Option<SavedDetailState>,
}

impl PanelState {
    pub fn new() -> Self {
        Self {
            schema: None,
            history_selected: 0,
            saved_selected: 0,
            saved_focus: SavedFocus::default(),
            saved_detail: None,
        }
    }
}

/// Active popup overlay.
#[derive(Debug, Clone)]
pub enum Popup {
    /// Help overlay.
    Help {
        /// Vertical scroll offset.
        scroll: usize,
    },
    /// Confirm deletion of saved query.
    ConfirmDelete {
        /// ID of the saved query to delete.
        saved_id: i64,
        /// Name of the query being deleted.
        name: String,
    },
    /// Text input for saving current query.
    SaveQuery {
        /// Single-line editor for the query name.
        editor: SimpleEditor,
    },
    /// Detail view for a single result row.
    EventDetail {
        /// Index of the row being viewed.
        row_index: usize,
        /// Vertical scroll offset within the detail view.
        scroll: usize,
    },
    /// Error message popup (dismissed by Esc/Enter).
    Error {
        /// Error message to display.
        message: String,
    },
    /// Text input for setting a schedule interval on a saved query.
    SetSchedule {
        /// ID of the saved query to schedule.
        saved_id: i64,
        /// Name of the query being scheduled.
        name: String,
        /// Single-line editor for the interval string.
        editor: SimpleEditor,
    },
    /// Column picker checklist for toggling visibility and pinning.
    ColumnPicker {
        /// Selected row in the column list.
        selected: usize,
        /// Vertical scroll offset.
        scroll: usize,
    },
    /// Command palette (Ctrl+P) — fuzzy searchable action/query/field picker.
    CommandPalette {
        /// Filter input text.
        input: String,
        /// Cursor position within the input.
        cursor: usize,
        /// Index of the selected item in the filtered list.
        selected: usize,
        /// Vertical scroll offset in the item list.
        scroll: usize,
        /// Items for the current input: the full catalog, or one service's
        /// columns once the input contains a dot.
        items: Vec<super::palette::PaletteItem>,
        /// Filtered + scored results (recomputed on input change).
        filtered: Vec<super::palette::FilteredItem>,
        /// Ghost text completion suffix (shown dimmed after cursor).
        ghost: Option<String>,
    },
}

/// Chart visualization mode for results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartView {
    /// Tabular view (default).
    Table,
    /// Braille line chart with axes and legend.
    LineChart,
    /// Sparkline view (compact block-char).
    Sparkline,
    /// Horizontal bar chart for aggregation results.
    BarChart,
}

impl ChartView {
    /// Cycle to next view appropriate for the result shape.
    pub fn next_for(self, is_timechart: bool, is_bar_chartable: bool) -> Self {
        match self {
            // timechart cycle: Table → LineChart → Sparkline → Table
            Self::Table if is_timechart => Self::LineChart,
            Self::LineChart => Self::Sparkline,
            Self::Sparkline if is_timechart => Self::Table,
            // bar-chartable cycle: Table → BarChart → Table
            Self::Table if is_bar_chartable => Self::BarChart,
            // fallback (includes BarChart → Table)
            _ => Self::Table,
        }
    }
}

/// Status of a tab's current query.
#[derive(Debug, Clone)]
pub enum TabStatus {
    /// No query running.
    Idle,
    /// Query is executing.
    Running {
        /// When the query started.
        #[allow(dead_code)] // Reserved for elapsed-time display in status bar
        start: Instant,
    },
    /// Query completed successfully.
    Success {
        /// Execution duration in milliseconds.
        duration_ms: u64,
    },
    /// Query failed.
    Error {
        /// Error message.
        message: String,
        /// Structured error details with optional span info.
        details: Vec<trawl_client::ErrorDetail>,
    },
}

/// Number of spaces prepended to continuation (wrapped) visual lines.
const CONTINUATION_INDENT: usize = 2;

/// Soft-wrap map: maps logical lines to visual (wrapped) lines.
///
/// The first visual line of each logical line gets the full viewport width.
/// Continuation visual lines are indented by `CONTINUATION_INDENT` chars
/// and get `width - CONTINUATION_INDENT` chars of content.
#[derive(Debug, Clone)]
pub struct WrapMap {
    /// For each logical line: char offsets where visual line breaks occur.
    /// Always starts with 0. E.g. `[0, 40, 78]` means 3 visual lines.
    line_breaks: Vec<Vec<usize>>,
    /// Viewport width used for this computation.
    viewport_width: usize,
}

impl WrapMap {
    /// Compute wrap breaks for all lines at a given viewport width.
    pub fn new(lines: &[String], width: usize) -> Self {
        let line_breaks = lines
            .iter()
            .map(|l| Self::compute_breaks(l, width))
            .collect();
        Self {
            line_breaks,
            viewport_width: width,
        }
    }

    /// Recompute the wrap map (on resize or edit).
    pub fn rebuild(&mut self, lines: &[String], width: usize) {
        self.viewport_width = width;
        self.line_breaks = lines
            .iter()
            .map(|l| Self::compute_breaks(l, width))
            .collect();
    }

    /// Compute break offsets for a single line.
    ///
    /// First visual line gets `width` chars. Each continuation gets
    /// `width - CONTINUATION_INDENT` chars (minimum 1 to guarantee progress).
    fn compute_breaks(line: &str, width: usize) -> Vec<usize> {
        let mut breaks = vec![0usize];
        if width == 0 {
            return breaks;
        }

        let char_count = line.chars().count();
        if char_count <= width {
            return breaks;
        }

        // First visual line: chars 0..width
        let mut offset = width;
        breaks.push(offset);

        // Continuation lines get narrower columns
        let cont_width = width.saturating_sub(CONTINUATION_INDENT).max(1);
        while offset + cont_width < char_count {
            offset += cont_width;
            breaks.push(offset);
        }

        breaks
    }

    /// Map logical (row, col) to visual (vrow, vcol).
    ///
    /// `vcol` includes the continuation indent offset for wrapped lines.
    pub fn logical_to_visual(&self, row: usize, col: usize) -> (usize, usize) {
        // Sum visual lines from all preceding logical lines.
        let mut vrow: usize = 0;
        for r in 0..row.min(self.line_breaks.len()) {
            vrow += self.line_breaks[r].len();
        }

        let breaks = self
            .line_breaks
            .get(row)
            .map_or(&[0usize][..], |v| v.as_slice());

        // Find which visual sub-line the col falls in.
        let mut seg = 0;
        for (i, &brk) in breaks.iter().enumerate().skip(1) {
            if col >= brk {
                seg = i;
            } else {
                break;
            }
        }

        vrow += seg;
        let local_col = col - breaks[seg];
        let vcol = if seg == 0 {
            local_col
        } else {
            CONTINUATION_INDENT + local_col
        };

        (vrow, vcol)
    }

    /// Map visual (vrow, vcol) back to logical (row, col).
    pub fn visual_to_logical(&self, vrow: usize, vcol: usize) -> (usize, usize) {
        let mut remaining = vrow;

        for (row, breaks) in self.line_breaks.iter().enumerate() {
            let num_visual = breaks.len();
            if remaining < num_visual {
                // We're in this logical line, segment `remaining`.
                let seg = remaining;
                let base = breaks[seg];
                let local_col = if seg == 0 {
                    vcol
                } else {
                    vcol.saturating_sub(CONTINUATION_INDENT)
                };

                // Clamp to the extent of this visual segment.
                let seg_end = if seg + 1 < breaks.len() {
                    breaks[seg + 1]
                } else {
                    usize::MAX // will be clamped by caller
                };
                let col = base + local_col;
                return (row, col.min(seg_end));
            }
            remaining -= num_visual;
        }

        // Past the end of the map, so fall back to the start of the last logical line.
        let last_row = self.line_breaks.len().saturating_sub(1);
        (last_row, 0)
    }

    /// Total visual lines across all logical lines.
    pub fn total_visual_lines(&self) -> usize {
        self.line_breaks.iter().map(Vec::len).sum()
    }

    /// How many visual lines a single logical line occupies.
    #[allow(dead_code)] // Public API for future use (e.g. line numbers)
    pub fn visual_lines_for(&self, logical_row: usize) -> usize {
        self.line_breaks.get(logical_row).map_or(1, Vec::len)
    }

    /// Get the break offsets for a logical line.
    pub fn breaks_for(&self, logical_row: usize) -> &[usize] {
        self.line_breaks
            .get(logical_row)
            .map_or(&[0], |v| v.as_slice())
    }

    /// The continuation indent size.
    pub fn continuation_indent() -> usize {
        CONTINUATION_INDENT
    }

    /// Current viewport width.
    #[allow(dead_code)] // Public API for future use (e.g. dynamic resize detection)
    pub fn viewport_width(&self) -> usize {
        self.viewport_width
    }
}

/// A snapshot of editor state for undo/redo.
#[derive(Debug, Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    cursor: (usize, usize),
}

/// Categories of edit operations for undo grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    /// Character insertion (grouped until word boundary).
    Insert,
    /// Character deletion.
    Delete,
    /// Newline insertion.
    Newline,
    /// Paste or bulk insert.
    Paste,
    /// Any other operation.
    Other,
}

/// Undo/redo history stack.
#[derive(Debug, Clone)]
struct UndoStack {
    /// History of snapshots.
    history: Vec<EditorSnapshot>,
    /// Current position in history (points to the "current" state).
    position: usize,
    /// Maximum number of snapshots to retain.
    max_size: usize,
}

impl UndoStack {
    fn new(max_size: usize) -> Self {
        Self {
            history: Vec::new(),
            position: 0,
            max_size,
        }
    }

    /// Push a new snapshot, discarding any redo history.
    fn push(&mut self, snapshot: EditorSnapshot) {
        // Truncate any forward history (we branched).
        self.history.truncate(self.position);
        self.history.push(snapshot);
        self.position = self.history.len();

        // Cap memory usage.
        if self.history.len() > self.max_size {
            let excess = self.history.len() - self.max_size;
            self.history.drain(..excess);
            self.position = self.history.len();
        }
    }

    /// Undo: return the previous snapshot (if any).
    fn undo(&mut self) -> Option<&EditorSnapshot> {
        if self.position > 0 {
            self.position -= 1;
            Some(&self.history[self.position])
        } else {
            None
        }
    }

    /// Redo: return the next snapshot (if any).
    fn redo(&mut self) -> Option<&EditorSnapshot> {
        if self.position + 1 < self.history.len() {
            self.position += 1;
            Some(&self.history[self.position])
        } else {
            None
        }
    }
}

/// Simple text editor for DSL queries.
#[derive(Debug, Clone)]
pub struct SimpleEditor {
    /// Lines of text.
    pub lines: Vec<String>,
    /// Cursor position (row, column) in logical coordinates.
    pub cursor: (usize, usize),
    /// Desired column for vertical movement (sticky column).
    /// When wrapping is active, this is in *visual* column coordinates.
    desired_col: Option<usize>,
    /// Vertical scroll offset. When wrapping is active, this is in
    /// *visual line* units (not logical line units).
    pub scroll_row: usize,
    /// Horizontal scroll offset. Always 0 when wrapping is active.
    pub scroll_col: usize,
    /// Selection anchor position. If `Some`, marks start of selection;
    /// cursor is the other end.
    pub selection_anchor: Option<(usize, usize)>,
    /// Undo/redo history.
    undo_stack: UndoStack,
    /// Last edit kind for undo grouping.
    last_edit_kind: Option<EditKind>,
    /// Single-line mode: disables newline insertion.
    pub single_line: bool,
    /// Soft-wrap map. `None` for single-line editors or before first render.
    pub wrap_map: Option<WrapMap>,
}

impl Default for SimpleEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl SimpleEditor {
    /// Create a new empty editor.
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
            desired_col: None,
            scroll_row: 0,
            scroll_col: 0,
            selection_anchor: None,
            undo_stack: UndoStack::new(200),
            last_edit_kind: None,
            single_line: false,
            wrap_map: None,
        }
    }

    /// Create a new single-line editor (for popup inputs, search bars, etc.).
    pub fn new_single_line() -> Self {
        let mut editor = Self::new();
        editor.single_line = true;
        editor
    }

    /// Rebuild the wrap map for the current lines at the given viewport width.
    ///
    /// Skipped for single-line editors. Call this from the render path
    /// whenever the viewport width is known.
    pub fn update_wrap_map(&mut self, width: usize) {
        if self.single_line {
            return;
        }
        if let Some(ref mut wm) = self.wrap_map {
            wm.rebuild(&self.lines, width);
        } else {
            self.wrap_map = Some(WrapMap::new(&self.lines, width));
        }
    }

    /// Cursor position in visual coordinates (accounting for wrapping).
    ///
    /// Falls back to logical coordinates when no wrap map is available.
    pub fn visual_cursor(&self) -> (usize, usize) {
        if let Some(ref wm) = self.wrap_map {
            wm.logical_to_visual(self.cursor.0, self.cursor.1)
        } else {
            self.cursor
        }
    }

    /// Handle a key event for simple popup inputs.
    /// Returns true if the key was consumed.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        match key.code {
            crossterm::event::KeyCode::Char(ch) => {
                self.insert_char(ch);
                true
            }
            crossterm::event::KeyCode::Backspace => {
                self.delete_char_before();
                true
            }
            crossterm::event::KeyCode::Delete => {
                self.delete_char_at();
                true
            }
            crossterm::event::KeyCode::Left => {
                self.clear_selection();
                self.move_left();
                true
            }
            crossterm::event::KeyCode::Right => {
                self.clear_selection();
                self.move_right();
                true
            }
            crossterm::event::KeyCode::Home => {
                self.clear_selection();
                self.move_to_line_start();
                true
            }
            crossterm::event::KeyCode::End => {
                self.clear_selection();
                self.move_to_line_end();
                true
            }
            _ => false,
        }
    }

    /// Convert a char offset to a byte offset within a string.
    ///
    /// Returns `s.len()` when `char_idx` is at or past the end,
    /// which is correct for "one past the last char" positions.
    fn char_to_byte(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map_or(s.len(), |(byte_idx, _)| byte_idx)
    }

    /// Count the number of characters in a string (not bytes).
    fn char_count(s: &str) -> usize {
        s.chars().count()
    }

    /// Get the current line.
    #[allow(dead_code)] // May be used in future features
    pub fn current_line(&self) -> &str {
        &self.lines[self.cursor.0]
    }

    /// Get all text as a single string (joined by newlines).
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, ch: char) {
        self.maybe_snapshot(EditKind::Insert);
        self.delete_selection();
        let (row, col) = self.cursor;
        let byte_idx = Self::char_to_byte(&self.lines[row], col);
        self.lines[row].insert(byte_idx, ch);
        self.cursor.1 += 1;
    }

    /// Insert a newline at the cursor position.
    pub fn insert_newline(&mut self) {
        if self.single_line {
            return;
        }
        self.maybe_snapshot(EditKind::Newline);
        self.delete_selection();
        let (row, col) = self.cursor;
        let byte_idx = Self::char_to_byte(&self.lines[row], col);
        let current_line = self.lines[row].clone();
        let (before, after) = current_line.split_at(byte_idx);
        before.clone_into(&mut self.lines[row]);
        self.lines.insert(row + 1, after.to_owned());
        self.cursor = (row + 1, 0);
    }

    /// Delete character before cursor (backspace).
    pub fn delete_char_before(&mut self) {
        self.maybe_snapshot(EditKind::Delete);
        if self.delete_selection() {
            return;
        }
        let (row, col) = self.cursor;
        if col > 0 {
            let byte_idx = Self::char_to_byte(&self.lines[row], col - 1);
            self.lines[row].remove(byte_idx);
            self.cursor.1 -= 1;
        } else if row > 0 {
            // Join with previous line
            let current = self.lines.remove(row);
            let prev_len = Self::char_count(&self.lines[row - 1]);
            self.lines[row - 1].push_str(&current);
            self.cursor = (row - 1, prev_len);
        }
    }

    /// Delete character at cursor (delete key).
    pub fn delete_char_at(&mut self) {
        self.maybe_snapshot(EditKind::Delete);
        if self.delete_selection() {
            return;
        }
        let (row, col) = self.cursor;
        let line_chars = Self::char_count(&self.lines[row]);
        if col < line_chars {
            let byte_idx = Self::char_to_byte(&self.lines[row], col);
            self.lines[row].remove(byte_idx);
        } else if row < self.lines.len() - 1 {
            // Join with next line
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
    }

    /// Move cursor up one visual line (wrap-aware, uses sticky column).
    pub fn move_up(&mut self) {
        if let Some(ref wm) = self.wrap_map {
            let (vrow, vcol) = wm.logical_to_visual(self.cursor.0, self.cursor.1);
            let target_vcol = self.desired_col.unwrap_or(vcol);
            if self.desired_col.is_none() {
                self.desired_col = Some(target_vcol);
            }
            if vrow == 0 {
                return;
            }
            let (new_row, new_col) = wm.visual_to_logical(vrow - 1, target_vcol);
            let line_chars = Self::char_count(&self.lines[new_row]);
            self.cursor = (new_row, new_col.min(line_chars));
        } else if self.cursor.0 > 0 {
            let target_col = self.desired_col.unwrap_or(self.cursor.1);
            self.cursor.0 -= 1;
            let line_chars = Self::char_count(&self.lines[self.cursor.0]);
            self.cursor.1 = target_col.min(line_chars);
            if self.desired_col.is_none() {
                self.desired_col = Some(target_col);
            }
        }
    }

    /// Move cursor down one visual line (wrap-aware, uses sticky column).
    pub fn move_down(&mut self) {
        if let Some(ref wm) = self.wrap_map {
            let (vrow, vcol) = wm.logical_to_visual(self.cursor.0, self.cursor.1);
            let target_vcol = self.desired_col.unwrap_or(vcol);
            if self.desired_col.is_none() {
                self.desired_col = Some(target_vcol);
            }
            let total = wm.total_visual_lines();
            if vrow + 1 >= total {
                return;
            }
            let (new_row, new_col) = wm.visual_to_logical(vrow + 1, target_vcol);
            let line_chars = Self::char_count(&self.lines[new_row]);
            self.cursor = (new_row, new_col.min(line_chars));
        } else if self.cursor.0 < self.lines.len() - 1 {
            let target_col = self.desired_col.unwrap_or(self.cursor.1);
            self.cursor.0 += 1;
            let line_chars = Self::char_count(&self.lines[self.cursor.0]);
            self.cursor.1 = target_col.min(line_chars);
            if self.desired_col.is_none() {
                self.desired_col = Some(target_col);
            }
        }
    }

    /// Move cursor left (clears sticky column).
    pub fn move_left(&mut self) {
        self.desired_col = None;
        if self.cursor.1 > 0 {
            self.cursor.1 -= 1;
        } else if self.cursor.0 > 0 {
            self.cursor.0 -= 1;
            self.cursor.1 = Self::char_count(&self.lines[self.cursor.0]);
        }
    }

    /// Move cursor right (clears sticky column).
    pub fn move_right(&mut self) {
        self.desired_col = None;
        if self.cursor.1 < Self::char_count(&self.lines[self.cursor.0]) {
            self.cursor.1 += 1;
        } else if self.cursor.0 < self.lines.len() - 1 {
            self.cursor.0 += 1;
            self.cursor.1 = 0;
        }
    }

    /// Move cursor to start of line (clears sticky column).
    pub fn move_to_line_start(&mut self) {
        self.desired_col = None;
        self.cursor.1 = 0;
    }

    /// Move cursor to end of line (clears sticky column).
    pub fn move_to_line_end(&mut self) {
        self.desired_col = None;
        self.cursor.1 = Self::char_count(&self.lines[self.cursor.0]);
    }

    /// Whether a character is a "word" character (alphanumeric or underscore).
    fn is_word_char(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    /// Find the word boundary to the left of a given position.
    ///
    /// Skips non-word chars, then skips word chars (standard word-left).
    /// Wraps to previous line if at column 0.
    pub fn find_word_boundary_left(&self, row: usize, col: usize) -> (usize, usize) {
        if col == 0 {
            // Wrap to end of previous line
            if row > 0 {
                return (row - 1, Self::char_count(&self.lines[row - 1]));
            }
            return (0, 0);
        }

        let line: Vec<char> = self.lines[row].chars().collect();
        let mut c = col;

        // Skip non-word chars
        while c > 0 && !Self::is_word_char(line[c - 1]) {
            c -= 1;
        }
        // Skip word chars
        while c > 0 && Self::is_word_char(line[c - 1]) {
            c -= 1;
        }

        (row, c)
    }

    /// Find the word boundary to the right of a given position.
    ///
    /// Skips word chars, then skips non-word chars (standard word-right).
    /// Wraps to next line if at end of line.
    pub fn find_word_boundary_right(&self, row: usize, col: usize) -> (usize, usize) {
        let line_chars = Self::char_count(&self.lines[row]);
        if col >= line_chars {
            // Wrap to start of next line
            if row < self.lines.len() - 1 {
                return (row + 1, 0);
            }
            return (row, line_chars);
        }

        let line: Vec<char> = self.lines[row].chars().collect();
        let mut c = col;

        // Skip word chars
        while c < line.len() && Self::is_word_char(line[c]) {
            c += 1;
        }
        // Skip non-word chars
        while c < line.len() && !Self::is_word_char(line[c]) {
            c += 1;
        }

        (row, c)
    }

    /// Move cursor one word to the left (clears sticky column).
    pub fn move_word_left(&mut self) {
        self.desired_col = None;
        let (row, col) = self.cursor;
        self.cursor = self.find_word_boundary_left(row, col);
    }

    /// Move cursor one word to the right (clears sticky column).
    pub fn move_word_right(&mut self) {
        self.desired_col = None;
        let (row, col) = self.cursor;
        self.cursor = self.find_word_boundary_right(row, col);
    }

    /// Insert arbitrary text at cursor, handling newlines by splitting lines.
    ///
    /// Used by paste and bracketed paste operations.
    /// Does raw insertion (no per-character undo snapshots).
    pub fn insert_text(&mut self, text: &str) {
        self.maybe_snapshot(EditKind::Paste);
        self.delete_selection();
        for ch in text.chars() {
            if ch == '\n' {
                let (row, col) = self.cursor;
                let byte_idx = Self::char_to_byte(&self.lines[row], col);
                let current_line = self.lines[row].clone();
                let (before, after) = current_line.split_at(byte_idx);
                before.clone_into(&mut self.lines[row]);
                self.lines.insert(row + 1, after.to_owned());
                self.cursor = (row + 1, 0);
            } else if ch != '\r' {
                let (row, col) = self.cursor;
                let byte_idx = Self::char_to_byte(&self.lines[row], col);
                self.lines[row].insert(byte_idx, ch);
                self.cursor.1 += 1;
            }
        }
    }

    /// Accept a ghost-text completion: replace `replace_len` chars before
    /// the cursor with `text`, then position cursor at `cursor_offset`
    /// within the inserted text (or at the end if `None`).
    pub fn replace_at_cursor(
        &mut self,
        replace_len: usize,
        text: &str,
        cursor_offset: Option<usize>,
    ) {
        self.save_snapshot();
        self.selection_anchor = None;

        let (row, col) = self.cursor;
        let start_col = col.saturating_sub(replace_len);

        // Delete the prefix chars.
        let byte_start = Self::char_to_byte(&self.lines[row], start_col);
        let byte_end = Self::char_to_byte(&self.lines[row], col);
        self.lines[row].drain(byte_start..byte_end);

        // Insert the replacement text.
        self.lines[row].insert_str(byte_start, text);

        // Position cursor.
        let insert_char_len = text.chars().count();
        self.cursor.1 = match cursor_offset {
            Some(offset) => start_col + offset,
            None => start_col + insert_char_len,
        };
    }

    /// Delete from cursor to word boundary left (Ctrl+W).
    pub fn delete_word_before(&mut self) {
        self.maybe_snapshot(EditKind::Other);
        let (row, col) = self.cursor;
        let (new_row, new_col) = self.find_word_boundary_left(row, col);

        if new_row == row {
            // Same line: remove chars between new_col and col
            let byte_start = Self::char_to_byte(&self.lines[row], new_col);
            let byte_end = Self::char_to_byte(&self.lines[row], col);
            self.lines[row].drain(byte_start..byte_end);
            self.cursor.1 = new_col;
        } else {
            // Crossed line boundary: delete from start of current line + join with prev
            let byte_col = Self::char_to_byte(&self.lines[row], col);
            let current = self.lines[row][byte_col..].to_owned();
            self.lines.remove(row);
            let byte_new_col = Self::char_to_byte(&self.lines[new_row], new_col);
            self.lines[new_row].truncate(byte_new_col);
            self.lines[new_row].push_str(&current);
            self.cursor = (new_row, new_col);
        }
    }

    /// Delete from cursor to word boundary right (Ctrl+Delete / Alt+D).
    pub fn delete_word_after(&mut self) {
        self.maybe_snapshot(EditKind::Other);
        let (row, col) = self.cursor;
        let (new_row, new_col) = self.find_word_boundary_right(row, col);

        if new_row == row {
            // Same line: remove chars between col and new_col
            let byte_start = Self::char_to_byte(&self.lines[row], col);
            let byte_end = Self::char_to_byte(&self.lines[row], new_col);
            self.lines[row].drain(byte_start..byte_end);
        } else {
            // Crossed line boundary: delete rest of current line + join with next
            let byte_col = Self::char_to_byte(&self.lines[row], col);
            self.lines[row].truncate(byte_col);
            let byte_new_col = Self::char_to_byte(&self.lines[new_row], new_col);
            let rest = self.lines[new_row][byte_new_col..].to_owned();
            self.lines.remove(new_row);
            self.lines[row].push_str(&rest);
        }
        // Cursor stays where it is
    }

    /// Delete from cursor to start of line (Ctrl+U).
    pub fn delete_to_line_start(&mut self) {
        self.maybe_snapshot(EditKind::Other);
        let (row, col) = self.cursor;
        let byte_idx = Self::char_to_byte(&self.lines[row], col);
        self.lines[row].drain(..byte_idx);
        self.cursor.1 = 0;
    }

    /// Delete from cursor to end of line (Ctrl+K).
    pub fn delete_to_line_end(&mut self) {
        self.maybe_snapshot(EditKind::Other);
        let (row, col) = self.cursor;
        let byte_idx = Self::char_to_byte(&self.lines[row], col);
        self.lines[row].truncate(byte_idx);
    }

    /// Start or extend selection from the current cursor position.
    ///
    /// If no selection is active, sets the anchor to the current cursor.
    pub fn start_selection(&mut self) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
    }

    /// Clear the active selection.
    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Place cursor at the given (row, col) position, clamping to valid bounds.
    ///
    /// Used by mouse click to position the cursor without needing to know
    /// line lengths or line counts externally.
    pub fn place_cursor(&mut self, row: usize, col: usize) {
        self.clear_selection();
        let row = row.min(self.lines.len().saturating_sub(1));
        let col = col.min(Self::char_count(&self.lines[row]));
        self.cursor = (row, col);
        self.desired_col = None;
    }

    /// Get the selection range in document order (start, end).
    pub fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.selection_anchor?;
        let cursor = self.cursor;
        if anchor <= cursor {
            Some((anchor, cursor))
        } else {
            Some((cursor, anchor))
        }
    }

    /// Extract the selected text as a string.
    pub fn selected_text(&self) -> Option<String> {
        let ((start_row, start_col), (end_row, end_col)) = self.selection_range()?;

        if start_row == end_row {
            // Single-line selection
            let byte_start = Self::char_to_byte(&self.lines[start_row], start_col);
            let byte_end = Self::char_to_byte(&self.lines[start_row], end_col);
            Some(self.lines[start_row][byte_start..byte_end].to_owned())
        } else {
            // Multi-line selection
            let mut result = String::new();
            let byte_start = Self::char_to_byte(&self.lines[start_row], start_col);
            result.push_str(&self.lines[start_row][byte_start..]);
            for row in (start_row + 1)..end_row {
                result.push('\n');
                result.push_str(&self.lines[row]);
            }
            result.push('\n');
            let byte_end = Self::char_to_byte(&self.lines[end_row], end_col);
            result.push_str(&self.lines[end_row][..byte_end]);
            Some(result)
        }
    }

    /// Delete the selected region. Returns `true` if a selection existed.
    pub fn delete_selection(&mut self) -> bool {
        let Some(((start_row, start_col), (end_row, end_col))) = self.selection_range() else {
            return false;
        };

        if start_row == end_row {
            // Single-line: just drain the range
            let byte_start = Self::char_to_byte(&self.lines[start_row], start_col);
            let byte_end = Self::char_to_byte(&self.lines[start_row], end_col);
            self.lines[start_row].drain(byte_start..byte_end);
        } else {
            // Multi-line: keep start of first line + end of last line
            let byte_end = Self::char_to_byte(&self.lines[end_row], end_col);
            let tail = self.lines[end_row][byte_end..].to_owned();
            let byte_start = Self::char_to_byte(&self.lines[start_row], start_col);
            self.lines[start_row].truncate(byte_start);
            self.lines[start_row].push_str(&tail);
            // Remove intermediate + last lines
            self.lines.drain((start_row + 1)..=end_row);
        }

        self.cursor = (start_row, start_col);
        self.selection_anchor = None;
        true
    }

    /// Adjust scroll offsets to keep cursor in viewport.
    ///
    /// When a wrap map is present, scrolling is in visual-line units and
    /// horizontal scrolling is disabled (wrapping handles it).
    /// Call after every cursor movement or content change.
    pub fn ensure_cursor_visible(&mut self, visible_rows: usize, visible_cols: usize) {
        if let Some(ref wm) = self.wrap_map {
            // Wrap-aware: scroll in visual line units, no horizontal scroll.
            self.scroll_col = 0;
            let (vrow, _vcol) = wm.logical_to_visual(self.cursor.0, self.cursor.1);
            if visible_rows > 0 {
                if vrow < self.scroll_row {
                    self.scroll_row = vrow;
                } else if vrow >= self.scroll_row + visible_rows {
                    self.scroll_row = vrow - visible_rows + 1;
                }
            }
        } else {
            let (row, col) = self.cursor;

            // Vertical scrolling (logical lines)
            if visible_rows > 0 {
                if row < self.scroll_row {
                    self.scroll_row = row;
                } else if row >= self.scroll_row + visible_rows {
                    self.scroll_row = row - visible_rows + 1;
                }
            }

            // Horizontal scrolling
            if visible_cols > 0 {
                if col < self.scroll_col {
                    self.scroll_col = col;
                } else if col >= self.scroll_col + visible_cols {
                    self.scroll_col = col - visible_cols + 1;
                }
            }
        }
    }

    /// Save a snapshot for undo if the edit kind changed or is a boundary.
    fn maybe_snapshot(&mut self, kind: EditKind) {
        let should_snapshot = match (self.last_edit_kind, kind) {
            // Always snapshot on kind change
            (Some(prev), cur) if prev != cur => true,
            // Always snapshot on newline, paste, other, or first edit
            (_, EditKind::Newline | EditKind::Paste | EditKind::Other) | (None, _) => true,
            // Same kind continues — check for word boundary on inserts
            (Some(EditKind::Insert), EditKind::Insert) => {
                // Snapshot at word boundaries (space/punctuation)
                let (row, col) = self.cursor;
                col > 0
                    && self.lines[row]
                        .chars()
                        .nth(col - 1)
                        .is_some_and(|ch| ch == ' ' || ch.is_ascii_punctuation())
            }
            _ => false,
        };

        if should_snapshot {
            self.undo_stack.push(EditorSnapshot {
                lines: self.lines.clone(),
                cursor: self.cursor,
            });
        }
        self.last_edit_kind = Some(kind);
    }

    /// Save a snapshot unconditionally (for paste/bulk operations).
    pub fn save_snapshot(&mut self) {
        self.undo_stack.push(EditorSnapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        });
        self.last_edit_kind = None;
    }

    /// Undo the last edit operation.
    pub fn undo(&mut self) {
        // Save the current state for redo (if at the tip of history).
        if self.undo_stack.position == self.undo_stack.history.len() {
            self.undo_stack.history.push(EditorSnapshot {
                lines: self.lines.clone(),
                cursor: self.cursor,
            });
            // Don't increment position — we want undo to go back from here.
        }

        if let Some(snapshot) = self.undo_stack.undo() {
            let snapshot = snapshot.clone();
            self.lines = snapshot.lines;
            self.cursor = snapshot.cursor;
            self.selection_anchor = None;
            self.last_edit_kind = None;
        }
    }

    /// Redo the last undone edit operation.
    pub fn redo(&mut self) {
        if let Some(snapshot) = self.undo_stack.redo() {
            let snapshot = snapshot.clone();
            self.lines = snapshot.lines;
            self.cursor = snapshot.cursor;
            self.selection_anchor = None;
            self.last_edit_kind = None;
        }
    }

    /// Clear all text.
    pub fn clear(&mut self) {
        self.save_snapshot();
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
        self.desired_col = None;
        self.scroll_row = 0;
        self.scroll_col = 0;
        self.selection_anchor = None;
    }
}

/// Rolling buffer for live-streamed log events.
///
/// Tracks columns via union of all seen field names and maintains
/// a bounded deque of rows. When new fields appear mid-stream,
/// existing rows are extended with `Value::Null`.
pub struct LiveBuffer {
    /// Ordered column names (insertion order preserved).
    column_names: Vec<String>,
    /// Column name -> index for O(1) lookup.
    column_index: HashMap<String, usize>,
    /// Bounded row buffer (newest at back).
    rows: VecDeque<Vec<Value>>,
    /// Max rows to retain.
    max_rows: usize,
}

impl LiveBuffer {
    /// Create a new empty buffer with the given capacity.
    pub fn new(max_rows: usize) -> Self {
        Self {
            column_names: Vec::new(),
            column_index: HashMap::new(),
            rows: VecDeque::new(),
            max_rows,
        }
    }

    /// Pre-seed column ordering from a DSL `fields`/`table` stage.
    ///
    /// Events that arrive will slot into these columns first (preserving
    /// user-specified order), with any extra fields appended afterward.
    pub fn with_column_order(mut self, columns: Vec<String>) -> Self {
        for (i, col) in columns.into_iter().enumerate() {
            self.column_index.insert(col.clone(), i);
            self.column_names.push(col);
        }
        self
    }

    /// Push a log event map into the buffer, extending the column union as needed.
    pub fn push_event(&mut self, event: &serde_json::Map<String, serde_json::Value>) {
        // Extend column set with any new fields.
        for key in event.keys() {
            if !self.column_index.contains_key(key) {
                let idx = self.column_names.len();
                self.column_names.push(key.clone());
                self.column_index.insert(key.clone(), idx);

                // Back-fill existing rows with Null for the new column.
                for row in &mut self.rows {
                    row.push(Value::Null);
                }
            }
        }

        // Build the row in column order.
        let mut row = vec![Value::Null; self.column_names.len()];
        for (key, json_val) in event {
            if let Some(&idx) = self.column_index.get(key) {
                row[idx] = serde_json::from_value(json_val.clone()).unwrap_or(Value::Null);
            }
        }

        self.rows.push_back(row);

        // Trim to capacity.
        while self.rows.len() > self.max_rows {
            self.rows.pop_front();
        }
    }

    /// Snapshot the current buffer state as a `QueryResponse` for rendering.
    pub fn to_query_response(&self) -> QueryResponse {
        let columns: Vec<Column> = self
            .column_names
            .iter()
            .map(|name| Column { name: name.clone() })
            .collect();

        let rows: Vec<Vec<Value>> = self.rows.iter().cloned().collect();
        let returned = rows.len();

        QueryResponse {
            result: QueryResult { columns, rows },
            truncated: false,
            pagination: PaginationMeta {
                limit: self.max_rows,
                offset: 0,
                returned,
            },
            // The SSE lane carries no incomplete-results notice (ADR-0011 slice C1).
            degraded_fields: Vec::new(),
            // …and no severity token rendering: the stream carries numbers,
            // and a live tail's columns are whatever the events bring.
            severity_columns: Vec::new(),
        }
    }

    /// Replace the entire buffer contents with an aggregation snapshot.
    ///
    /// Used when the server emits a `snapshot` event (aggregation mode).
    /// The buffer is cleared and rebuilt from the given columns and rows.
    pub fn replace_with_snapshot(
        &mut self,
        columns: &[String],
        rows: &[serde_json::Map<String, serde_json::Value>],
    ) {
        self.column_names = columns.to_vec();
        self.column_index.clear();
        for (i, name) in columns.iter().enumerate() {
            self.column_index.insert(name.clone(), i);
        }
        self.rows.clear();
        for row_map in rows {
            let mut row = vec![Value::Null; self.column_names.len()];
            for (key, json_val) in row_map {
                if let Some(&idx) = self.column_index.get(key) {
                    row[idx] = serde_json::from_value(json_val.clone()).unwrap_or(Value::Null);
                }
            }
            self.rows.push_back(row);
        }
    }

    /// Number of buffered rows.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the buffer is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// The query tab state (editor + results).
#[derive(Debug)]
pub struct Tab {
    /// Query editor.
    pub editor: SimpleEditor,
    /// Last query result (if any).
    pub result: Option<QueryResponse>,
    /// Vertical scroll offset in results pane (row index).
    pub scroll_offset: usize,
    /// Horizontal scroll offset in results pane (column index).
    pub horizontal_scroll_offset: usize,
    /// Current query status.
    pub status: TabStatus,
    /// Chart visualization mode.
    pub chart_view: ChartView,
    /// Currently selected row in results (htop-style highlight bar).
    pub selected_row: Option<usize>,
    /// Visible row count from last render frame (updated by UI each frame).
    pub last_visible_rows: usize,
    /// Column configuration (visibility, pinning, width overrides). Reset on new result.
    pub column_config: Option<ColumnConfig>,
    /// Handle to the running query task (for cancellation).
    pub query_task: Option<tokio::task::JoinHandle<()>>,
    /// Real-time validation errors from local parsing (separate from execution errors).
    pub validation_errors: Vec<trawl_core::parser::ParseError>,
    /// Whether the editor has been modified since last validation.
    pub validation_dirty: bool,
    /// Timestamp of the last editor modification (for debounce).
    pub last_edit_time: Option<std::time::Instant>,
    /// Active ghost-text autocomplete suggestion (if any).
    pub ghost: Option<super::autocomplete::Completion>,
}

impl Tab {
    /// Create a new tab.
    pub fn new() -> Self {
        Self {
            editor: SimpleEditor::new(),
            result: None,
            scroll_offset: 0,
            horizontal_scroll_offset: 0,
            status: TabStatus::Idle,
            chart_view: ChartView::Table,
            selected_row: None,
            last_visible_rows: 15,
            column_config: None,
            query_task: None,
            validation_errors: Vec::new(),
            validation_dirty: false,
            last_edit_time: None,
            ghost: None,
        }
    }

    /// Clear the editor and reset state.
    pub fn clear(&mut self) {
        self.editor.clear();
        self.result = None;
        self.scroll_offset = 0;
        self.horizontal_scroll_offset = 0;
        self.status = TabStatus::Idle;
        self.chart_view = ChartView::Table;
        self.selected_row = None;
        self.last_visible_rows = 15;
        self.column_config = None;
        self.validation_errors.clear();
        self.validation_dirty = false;
        self.last_edit_time = None;
        self.ghost = None;
        // Abort any running query task.
        if let Some(handle) = self.query_task.take() {
            handle.abort();
        }
    }

    /// Mark the editor content as modified, triggering validation after debounce.
    pub fn mark_editor_dirty(&mut self) {
        self.validation_dirty = true;
        self.last_edit_time = Some(std::time::Instant::now());
    }

    /// Run debounced validation: parse the query locally and populate `validation_errors`.
    ///
    /// Returns `true` if validation was actually performed (debounce elapsed).
    pub fn maybe_validate(&mut self) -> bool {
        if !self.validation_dirty {
            return false;
        }

        // Debounce: wait 300ms after last edit.
        if let Some(last_edit) = self.last_edit_time
            && last_edit.elapsed() < std::time::Duration::from_millis(300)
        {
            return false;
        }

        let text = self.editor.text();
        if text.trim().is_empty() {
            self.validation_errors.clear();
            self.validation_dirty = false;
            return true;
        }

        match trawl_core::parser::parse(&text) {
            Ok(query) => match trawl_core::emitter::validate_pipeline(&query.pipeline) {
                Ok(()) => self.validation_errors.clear(),
                Err(e) => self.validation_errors = e.to_parse_errors(text.len()),
            },
            Err(errors) => self.validation_errors = errors,
        }
        self.validation_dirty = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(
        pairs: &[(&str, serde_json::Value)],
    ) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    // --- SimpleEditor tests ---

    #[test]
    fn editor_new_is_empty() {
        let editor = SimpleEditor::new();
        assert_eq!(editor.lines, vec![String::new()]);
        assert_eq!(editor.cursor, (0, 0));
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn editor_insert_chars() {
        let mut editor = SimpleEditor::new();
        for ch in "abc".chars() {
            editor.insert_char(ch);
        }
        assert_eq!(editor.text(), "abc");
        assert_eq!(editor.cursor, (0, 3));
    }

    #[test]
    fn editor_insert_newline_splits_line() {
        let mut editor = SimpleEditor::new();
        for ch in "hello".chars() {
            editor.insert_char(ch);
        }
        // Move cursor to col 2
        editor.cursor.1 = 2;
        editor.insert_newline();
        assert_eq!(editor.lines, vec!["he".to_owned(), "llo".to_owned()]);
        assert_eq!(editor.cursor, (1, 0));
    }

    #[test]
    fn editor_backspace_joins_lines() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello".to_owned(), "world".to_owned()];
        editor.cursor = (1, 0);
        editor.delete_char_before();
        assert_eq!(editor.lines, vec!["helloworld".to_owned()]);
        assert_eq!(editor.cursor, (0, 5));
    }

    #[test]
    fn editor_backspace_at_origin_is_noop() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('x');
        editor.cursor = (0, 0);
        editor.delete_char_before();
        assert_eq!(editor.text(), "x");
        assert_eq!(editor.cursor, (0, 0));
    }

    #[test]
    fn editor_delete_joins_lines() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello".to_owned(), "world".to_owned()];
        editor.cursor = (0, 5); // end of first line
        editor.delete_char_at();
        assert_eq!(editor.lines, vec!["helloworld".to_owned()]);
        assert_eq!(editor.cursor, (0, 5));
    }

    #[test]
    fn editor_delete_at_end_is_noop() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('x');
        // cursor already at (0, 1), which is end of last line
        editor.delete_char_at();
        assert_eq!(editor.text(), "x");
    }

    #[test]
    fn editor_move_up_clamps_col() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["short".to_owned(), "longer line".to_owned()];
        editor.cursor = (1, 11); // end of "longer line"
        editor.move_up();
        assert_eq!(editor.cursor, (0, 5)); // clamped to len of "short"
    }

    #[test]
    fn editor_move_down_clamps_col() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["longer line".to_owned(), "short".to_owned()];
        editor.cursor = (0, 11); // end of "longer line"
        editor.move_down();
        assert_eq!(editor.cursor, (1, 5)); // clamped to len of "short"
    }

    #[test]
    fn editor_move_left_wraps_to_prev_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["abc".to_owned(), "def".to_owned()];
        editor.cursor = (1, 0);
        editor.move_left();
        assert_eq!(editor.cursor, (0, 3)); // end of "abc"
    }

    #[test]
    fn editor_move_right_wraps_to_next_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["abc".to_owned(), "def".to_owned()];
        editor.cursor = (0, 3); // end of "abc"
        editor.move_right();
        assert_eq!(editor.cursor, (1, 0)); // start of "def"
    }

    #[test]
    fn editor_move_left_at_origin_is_noop() {
        let editor_before = SimpleEditor::new();
        let mut editor = SimpleEditor::new();
        editor.move_left();
        assert_eq!(editor.cursor, editor_before.cursor);
    }

    #[test]
    fn editor_move_right_at_end_is_noop() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('x');
        let cursor_before = editor.cursor;
        editor.move_right();
        assert_eq!(editor.cursor, cursor_before);
    }

    #[test]
    fn editor_home_end() {
        let mut editor = SimpleEditor::new();
        for ch in "hello".chars() {
            editor.insert_char(ch);
        }
        assert_eq!(editor.cursor.1, 5);
        editor.move_to_line_start();
        assert_eq!(editor.cursor.1, 0);
        editor.move_to_line_end();
        assert_eq!(editor.cursor.1, 5);
    }

    #[test]
    fn editor_clear() {
        let mut editor = SimpleEditor::new();
        for ch in "hello world".chars() {
            editor.insert_char(ch);
        }
        editor.clear();
        assert_eq!(editor.lines, vec![String::new()]);
        assert_eq!(editor.cursor, (0, 0));
    }

    #[test]
    fn editor_text_multiline() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec![
            "line one".to_owned(),
            "line two".to_owned(),
            "line three".to_owned(),
        ];
        assert_eq!(editor.text(), "line one\nline two\nline three");
    }

    // --- Word movement tests ---

    #[test]
    fn editor_word_boundary_left_skips_word() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        // From end of "world" (col 11)
        assert_eq!(editor.find_word_boundary_left(0, 11), (0, 6));
        // From start of "world" (col 6)
        assert_eq!(editor.find_word_boundary_left(0, 6), (0, 0));
    }

    #[test]
    fn editor_word_boundary_left_with_symbols() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["foo_bar::baz".to_owned()];
        // From col 12: no non-word chars to skip, then skip "baz" → col 9.
        assert_eq!(editor.find_word_boundary_left(0, 12), (0, 9));
        // From col 9: skip "::" → col 7, then skip "foo_bar" → col 0.
        assert_eq!(editor.find_word_boundary_left(0, 9), (0, 0));
    }

    #[test]
    fn editor_word_boundary_left_at_origin() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello".to_owned()];
        assert_eq!(editor.find_word_boundary_left(0, 0), (0, 0));
    }

    #[test]
    fn editor_word_boundary_left_wraps_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["abc".to_owned(), "def".to_owned()];
        assert_eq!(editor.find_word_boundary_left(1, 0), (0, 3));
    }

    #[test]
    fn editor_word_boundary_right_skips_word() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        // From col 0: skip word "hello" → col 5, skip non-word " " → col 6
        assert_eq!(editor.find_word_boundary_right(0, 0), (0, 6));
        // From col 6: skip word "world" → col 11, skip non-word: nothing → col 11
        assert_eq!(editor.find_word_boundary_right(0, 6), (0, 11));
    }

    #[test]
    fn editor_word_boundary_right_at_end() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello".to_owned()];
        assert_eq!(editor.find_word_boundary_right(0, 5), (0, 5));
    }

    #[test]
    fn editor_word_boundary_right_wraps_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["abc".to_owned(), "def".to_owned()];
        assert_eq!(editor.find_word_boundary_right(0, 3), (1, 0));
    }

    #[test]
    fn editor_move_word_left() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        editor.cursor = (0, 11);
        editor.move_word_left();
        assert_eq!(editor.cursor, (0, 6));
        editor.move_word_left();
        assert_eq!(editor.cursor, (0, 0));
    }

    #[test]
    fn editor_move_word_right() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        editor.cursor = (0, 0);
        editor.move_word_right();
        assert_eq!(editor.cursor, (0, 6));
        editor.move_word_right();
        assert_eq!(editor.cursor, (0, 11));
    }

    // --- Undo/redo tests ---

    #[test]
    fn editor_undo_restores_state() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('a');
        editor.insert_char('b');
        // Trigger snapshot by changing edit kind
        editor.insert_newline();
        editor.insert_char('c');
        // Undo should restore to before the newline
        editor.undo();
        // The exact state depends on snapshot boundaries, but we should
        // get back to something before the newline
        assert!(editor.text().len() < 4);
    }

    #[test]
    fn editor_undo_at_empty_is_noop() {
        let mut editor = SimpleEditor::new();
        editor.undo(); // Should not panic
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn editor_redo_after_undo() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("hello");
        editor.save_snapshot(); // Force snapshot
        editor.insert_text(" world");
        editor.undo();
        let after_undo = editor.text();
        editor.redo();
        let after_redo = editor.text();
        // After redo, we should be back to "hello world" (or close to it)
        assert!(after_redo.len() > after_undo.len());
    }

    #[test]
    fn editor_redo_at_end_is_noop() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('x');
        editor.redo(); // Should not panic
        assert_eq!(editor.text(), "x");
    }

    #[test]
    fn editor_clear_pushes_undo() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("hello world");
        editor.clear();
        assert_eq!(editor.text(), "");
        editor.undo();
        // Should restore the previous text
        assert!(!editor.text().is_empty());
    }

    // --- Selection tests ---

    #[test]
    fn editor_selection_range_none_by_default() {
        let editor = SimpleEditor::new();
        assert_eq!(editor.selection_range(), None);
    }

    #[test]
    fn editor_start_and_extend_selection() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        editor.cursor = (0, 5);
        editor.start_selection();
        editor.cursor = (0, 11);
        assert_eq!(editor.selection_range(), Some(((0, 5), (0, 11))));
    }

    #[test]
    fn editor_selection_range_reversed() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello".to_owned()];
        editor.cursor = (0, 5);
        editor.start_selection();
        editor.cursor = (0, 0);
        // Should return in document order
        assert_eq!(editor.selection_range(), Some(((0, 0), (0, 5))));
    }

    #[test]
    fn editor_selected_text_single_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        editor.selection_anchor = Some((0, 0));
        editor.cursor = (0, 5);
        assert_eq!(editor.selected_text(), Some("hello".to_owned()));
    }

    #[test]
    fn editor_selected_text_multiline() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello".to_owned(), "world".to_owned(), "foo".to_owned()];
        editor.selection_anchor = Some((0, 3));
        editor.cursor = (2, 2);
        assert_eq!(editor.selected_text(), Some("lo\nworld\nfo".to_owned()));
    }

    #[test]
    fn editor_delete_selection_single_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        editor.selection_anchor = Some((0, 5));
        editor.cursor = (0, 11);
        assert!(editor.delete_selection());
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.cursor, (0, 5));
        assert_eq!(editor.selection_anchor, None);
    }

    #[test]
    fn editor_delete_selection_multiline() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["aaa".to_owned(), "bbb".to_owned(), "ccc".to_owned()];
        editor.selection_anchor = Some((0, 1));
        editor.cursor = (2, 2);
        assert!(editor.delete_selection());
        assert_eq!(editor.text(), "ac");
        assert_eq!(editor.cursor, (0, 1));
    }

    #[test]
    fn editor_insert_char_replaces_selection() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello".to_owned()];
        editor.selection_anchor = Some((0, 1));
        editor.cursor = (0, 4);
        editor.insert_char('X');
        assert_eq!(editor.text(), "hXo");
    }

    #[test]
    fn editor_clear_selection() {
        let mut editor = SimpleEditor::new();
        editor.selection_anchor = Some((0, 0));
        editor.clear_selection();
        assert_eq!(editor.selection_anchor, None);
    }

    // --- Sticky column tests ---

    #[test]
    fn editor_sticky_column_preserved_across_short_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec![
            "long line here".to_owned(),
            "short".to_owned(),
            "another long one".to_owned(),
        ];
        editor.cursor = (0, 10); // col 10 in first line
        editor.move_down(); // to "short" — clamped to col 5
        assert_eq!(editor.cursor, (1, 5));
        editor.move_down(); // to "another long one" — sticky col restores to 10
        assert_eq!(editor.cursor, (2, 10));
    }

    #[test]
    fn editor_sticky_column_cleared_on_horizontal_move() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec![
            "long line here".to_owned(),
            "short".to_owned(),
            "another long one".to_owned(),
        ];
        editor.cursor = (0, 10);
        editor.move_down(); // col clamped to 5, desired_col = 10
        editor.move_left(); // clears desired_col, cursor at (1, 4)
        assert_eq!(editor.cursor, (1, 4));
        editor.move_down(); // no sticky col, uses current col 4
        assert_eq!(editor.cursor, (2, 4));
    }

    // --- insert_text tests ---

    #[test]
    fn editor_insert_text_single_line() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("hello world");
        assert_eq!(editor.text(), "hello world");
        assert_eq!(editor.cursor, (0, 11));
    }

    #[test]
    fn editor_insert_text_multiline() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("hello\nworld\nfoo");
        assert_eq!(editor.text(), "hello\nworld\nfoo");
        assert_eq!(editor.cursor, (2, 3));
    }

    #[test]
    fn editor_insert_text_strips_cr() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("hello\r\nworld");
        assert_eq!(editor.text(), "hello\nworld");
    }

    #[test]
    fn editor_insert_text_at_cursor() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["abcdef".to_owned()];
        editor.cursor = (0, 3);
        editor.insert_text("XYZ");
        assert_eq!(editor.text(), "abcXYZdef");
    }

    // --- Kill operation tests ---

    #[test]
    fn editor_delete_word_before() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        editor.cursor = (0, 11);
        editor.delete_word_before();
        assert_eq!(editor.text(), "hello ");
        assert_eq!(editor.cursor, (0, 6));
    }

    #[test]
    fn editor_delete_word_before_at_start() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello".to_owned()];
        editor.cursor = (0, 0);
        editor.delete_word_before();
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.cursor, (0, 0));
    }

    #[test]
    fn editor_delete_word_after() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        editor.cursor = (0, 0);
        editor.delete_word_after();
        assert_eq!(editor.text(), "world");
        assert_eq!(editor.cursor, (0, 0));
    }

    #[test]
    fn editor_delete_word_after_at_end() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello".to_owned()];
        editor.cursor = (0, 5);
        editor.delete_word_after();
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.cursor, (0, 5));
    }

    #[test]
    fn editor_delete_to_line_start() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        editor.cursor = (0, 6);
        editor.delete_to_line_start();
        assert_eq!(editor.text(), "world");
        assert_eq!(editor.cursor, (0, 0));
    }

    #[test]
    fn editor_delete_to_line_end() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        editor.cursor = (0, 5);
        editor.delete_to_line_end();
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.cursor, (0, 5));
    }

    // --- Multi-byte / UTF-8 tests ---

    #[test]
    fn editor_insert_multibyte_char() {
        let mut editor = SimpleEditor::new();
        // ∂ is U+2202, 3 bytes in UTF-8
        editor.insert_char('∂');
        assert_eq!(editor.text(), "∂");
        assert_eq!(editor.cursor, (0, 1)); // char offset, not byte
    }

    #[test]
    fn editor_insert_after_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('∂');
        editor.insert_char('x');
        assert_eq!(editor.text(), "∂x");
        assert_eq!(editor.cursor, (0, 2));
    }

    #[test]
    fn editor_backspace_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('a');
        editor.insert_char('∂');
        editor.insert_char('b');
        assert_eq!(editor.text(), "a∂b");
        // Delete the ∂
        editor.move_left();
        editor.delete_char_before();
        assert_eq!(editor.text(), "ab");
        assert_eq!(editor.cursor, (0, 1));
    }

    #[test]
    fn editor_delete_at_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('∂');
        editor.insert_char('x');
        editor.move_to_line_start();
        editor.delete_char_at();
        assert_eq!(editor.text(), "x");
        assert_eq!(editor.cursor, (0, 0));
    }

    #[test]
    fn editor_move_across_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("a∂b");
        assert_eq!(editor.cursor, (0, 3));
        editor.move_left();
        assert_eq!(editor.cursor, (0, 2));
        editor.move_left();
        assert_eq!(editor.cursor, (0, 1));
        editor.move_right();
        assert_eq!(editor.cursor, (0, 2));
        editor.move_to_line_end();
        assert_eq!(editor.cursor, (0, 3));
    }

    #[test]
    fn editor_newline_split_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("café");
        // Move cursor between 'f' and 'é'
        editor.move_left();
        editor.insert_newline();
        assert_eq!(editor.lines, vec!["caf", "é"]);
        assert_eq!(editor.cursor, (1, 0));
    }

    #[test]
    fn editor_select_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("a∂b");
        // Select "∂b" (chars 1..3)
        editor.cursor = (0, 1);
        editor.selection_anchor = Some((0, 3));
        assert_eq!(editor.selected_text().unwrap(), "∂b");
    }

    #[test]
    fn editor_delete_selection_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("hé∂lo");
        // Select "é∂" (chars 1..3)
        editor.cursor = (0, 1);
        editor.selection_anchor = Some((0, 3));
        editor.delete_selection();
        assert_eq!(editor.text(), "hlo");
        assert_eq!(editor.cursor, (0, 1));
    }

    #[test]
    fn editor_kill_line_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("∂∂∂abc");
        editor.cursor = (0, 3); // after the three ∂ chars
        editor.delete_to_line_end();
        assert_eq!(editor.text(), "∂∂∂");
        editor.delete_to_line_start();
        assert_eq!(editor.text(), "");
    }

    // --- LiveBuffer tests ---

    #[test]
    fn live_buffer_empty() {
        let buf = LiveBuffer::new(100);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        let resp = buf.to_query_response();
        assert!(resp.result.columns.is_empty());
        assert!(resp.result.rows.is_empty());
    }

    #[test]
    fn live_buffer_push_single_event() {
        let mut buf = LiveBuffer::new(100);
        let event = make_event(&[
            ("host", serde_json::json!("web-1")),
            ("level", serde_json::json!("error")),
        ]);
        buf.push_event(&event);
        assert_eq!(buf.len(), 1);

        let resp = buf.to_query_response();
        assert_eq!(resp.result.columns.len(), 2);
        assert_eq!(resp.result.rows.len(), 1);
    }

    #[test]
    fn live_buffer_column_union_extends_with_nulls() {
        let mut buf = LiveBuffer::new(100);

        // First event has {host, level}.
        buf.push_event(&make_event(&[
            ("host", serde_json::json!("web-1")),
            ("level", serde_json::json!("info")),
        ]));

        // Second event has {host, service} — new field appears.
        buf.push_event(&make_event(&[
            ("host", serde_json::json!("web-2")),
            ("service", serde_json::json!("nginx")),
        ]));

        let resp = buf.to_query_response();
        assert_eq!(resp.result.columns.len(), 3); // host, level, service

        // First row should have Null for the new "service" column.
        let first_row = &resp.result.rows[0];
        assert_eq!(first_row.len(), 3);
        assert_eq!(first_row[2], Value::Null); // service = Null

        // Second row should have Null for "level".
        let second_row = &resp.result.rows[1];
        assert_eq!(second_row[1], Value::Null); // level = Null
        assert_eq!(second_row[2], Value::String("nginx".to_owned()));
    }

    #[test]
    fn live_buffer_caps_at_max_rows() {
        let mut buf = LiveBuffer::new(3);

        for i in 0..5 {
            buf.push_event(&make_event(&[("n", serde_json::json!(i))]));
        }

        assert_eq!(buf.len(), 3);

        let resp = buf.to_query_response();
        // Should contain events 2, 3, 4 (oldest trimmed).
        assert_eq!(resp.result.rows[0][0], Value::Integer(2));
        assert_eq!(resp.result.rows[2][0], Value::Integer(4));
    }

    #[test]
    fn live_buffer_handles_mixed_value_types() {
        let mut buf = LiveBuffer::new(100);
        buf.push_event(&make_event(&[
            ("count", serde_json::json!(42)),
            ("rate", serde_json::json!(1.5)),
            ("active", serde_json::json!(true)),
            ("tag", serde_json::json!(null)),
        ]));

        let resp = buf.to_query_response();
        let row = &resp.result.rows[0];
        // serde_json::Map iterates in BTreeMap (alphabetical) order:
        // active, count, rate, tag
        assert_eq!(row[0], Value::Boolean(true));
        assert_eq!(row[1], Value::Integer(42));
        assert_eq!(row[2], Value::Float(1.5));
        assert_eq!(row[3], Value::Null);
    }

    // --- compute_common_fields tests ---

    fn make_service(name: &str, columns: &[(&str, &str)]) -> trawl_api::ServiceSchema {
        trawl_api::ServiceSchema {
            name: name.to_owned(),
            columns: columns
                .iter()
                .map(|(n, t)| trawl_api::ServiceColumnStats {
                    name: (*n).to_owned(),
                    data_type: (*t).to_owned(),
                    null_count: 0,
                    total_count: 1000,
                    min_value: None,
                    max_value: None,
                    compressed_bytes: 0,
                })
                .collect(),
            earliest_date: None,
            latest_date: None,
            file_count: 1,
            total_bytes: 1024,
            total_events: 1000,
            daily_event_counts: vec![],
            degraded_fields: Vec::new(),
        }
    }

    #[test]
    fn common_fields_empty_services() {
        let result = compute_common_fields(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn common_fields_well_known_always_included() {
        let services = vec![
            make_service(
                "nginx",
                &[
                    ("_time", "TIMESTAMP"),
                    ("host", "VARCHAR"),
                    ("status", "INTEGER"),
                ],
            ),
            make_service(
                "sshd",
                &[
                    ("_time", "TIMESTAMP"),
                    ("host", "VARCHAR"),
                    ("pid", "INTEGER"),
                ],
            ),
        ];
        let common = compute_common_fields(&services);
        let names: Vec<&str> = common.iter().map(|f| f.name.as_str()).collect();
        // _time and host are well-known and present in both services.
        assert!(names.contains(&"_time"));
        assert!(names.contains(&"host"));
        // status and pid are unique to one service each — not common.
        assert!(!names.contains(&"status"));
        assert!(!names.contains(&"pid"));
    }

    #[test]
    fn common_fields_well_known_order_preserved() {
        let services = vec![
            make_service(
                "a",
                &[
                    ("message", "VARCHAR"),
                    ("_time", "TIMESTAMP"),
                    ("host", "VARCHAR"),
                    ("service", "VARCHAR"),
                    ("_severity", "BIGINT"),
                ],
            ),
            make_service(
                "b",
                &[
                    ("message", "VARCHAR"),
                    ("_time", "TIMESTAMP"),
                    ("host", "VARCHAR"),
                    ("service", "VARCHAR"),
                    ("_severity", "BIGINT"),
                ],
            ),
        ];
        let common = compute_common_fields(&services);
        let names: Vec<&str> = common.iter().map(|f| f.name.as_str()).collect();
        // Well-known fields should come in the defined order.
        assert_eq!(names, &["_time", "service", "host", "_severity", "message"]);
    }

    #[test]
    fn well_known_fields_match_api_leading_order() {
        assert_eq!(
            WELL_KNOWN_FIELDS,
            trawl_api::value::WELL_KNOWN_LOG_FIELDS,
            "the TUI duplicate must mirror trawl_api"
        );
    }

    #[test]
    fn common_fields_threshold_promotes_frequent_fields() {
        // 5 services. A field in 5/5 (100%) should be promoted.
        // A field in 3/5 (60%) should NOT (below 80% threshold).
        let services = vec![
            make_service("a", &[("common_f", "VARCHAR"), ("rare_f", "VARCHAR")]),
            make_service("b", &[("common_f", "VARCHAR"), ("rare_f", "VARCHAR")]),
            make_service("c", &[("common_f", "VARCHAR"), ("rare_f", "VARCHAR")]),
            make_service("d", &[("common_f", "VARCHAR")]),
            make_service("e", &[("common_f", "VARCHAR")]),
        ];
        let common = compute_common_fields(&services);
        let names: Vec<&str> = common.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"common_f")); // 5/5 = 100% > 80%
        assert!(!names.contains(&"rare_f")); // 3/5 = 60% < 80%
    }

    // --- WrapMap tests ---

    #[test]
    fn wrap_map_short_line_no_breaks() {
        let lines = vec!["hello".to_owned()];
        let wm = WrapMap::new(&lines, 40);
        assert_eq!(wm.total_visual_lines(), 1);
        assert_eq!(wm.breaks_for(0), &[0]);
    }

    #[test]
    fn wrap_map_exact_width_no_break() {
        // A line exactly `width` chars should NOT wrap.
        let lines = vec!["a".repeat(40)];
        let wm = WrapMap::new(&lines, 40);
        assert_eq!(wm.total_visual_lines(), 1);
    }

    #[test]
    fn wrap_map_one_char_over_wraps() {
        // 41 chars at width 40 → 2 visual lines.
        let lines = vec!["a".repeat(41)];
        let wm = WrapMap::new(&lines, 40);
        assert_eq!(wm.total_visual_lines(), 2);
        assert_eq!(wm.breaks_for(0), &[0, 40]);
    }

    #[test]
    fn wrap_map_multiple_wraps() {
        // width=10, continuation_indent=2, so cont lines hold 8 chars.
        // 30 chars: first 10, then 8, 8, 4 → 4 visual lines.
        let lines = vec!["a".repeat(30)];
        let wm = WrapMap::new(&lines, 10);
        assert_eq!(wm.total_visual_lines(), 4);
        assert_eq!(wm.breaks_for(0), &[0, 10, 18, 26]);
    }

    #[test]
    fn wrap_map_logical_to_visual_first_line() {
        let lines = vec!["a".repeat(30)];
        let wm = WrapMap::new(&lines, 10);

        // Col 0 → vrow 0, vcol 0
        assert_eq!(wm.logical_to_visual(0, 0), (0, 0));
        // Col 5 → vrow 0, vcol 5
        assert_eq!(wm.logical_to_visual(0, 5), (0, 5));
        // Col 10 → vrow 1, vcol CONTINUATION_INDENT + 0 = 2
        assert_eq!(wm.logical_to_visual(0, 10), (1, 2));
        // Col 15 → vrow 1, vcol 2 + 5 = 7
        assert_eq!(wm.logical_to_visual(0, 15), (1, 7));
        // Col 18 → vrow 2, vcol 2 + 0 = 2
        assert_eq!(wm.logical_to_visual(0, 18), (2, 2));
    }

    #[test]
    fn wrap_map_visual_to_logical_round_trip() {
        let lines = vec!["a".repeat(30)];
        let wm = WrapMap::new(&lines, 10);

        // Check a few positions round-trip through both mappings.
        for col in [0, 5, 10, 15, 18, 25, 29] {
            let (vrow, vcol) = wm.logical_to_visual(0, col);
            let (row, col_back) = wm.visual_to_logical(vrow, vcol);
            assert_eq!(row, 0);
            assert_eq!(col_back, col, "round-trip failed for col={col}");
        }
    }

    #[test]
    fn wrap_map_multi_logical_lines() {
        let lines = vec![
            "a".repeat(15), // wraps at width 10: 2 visual lines
            "b".repeat(5),  // fits: 1 visual line
        ];
        let wm = WrapMap::new(&lines, 10);
        assert_eq!(wm.total_visual_lines(), 3);

        // Second logical line starts at visual row 2.
        assert_eq!(wm.logical_to_visual(1, 0), (2, 0));
        assert_eq!(wm.logical_to_visual(1, 3), (2, 3));
    }

    #[test]
    fn wrap_map_visual_to_logical_second_line() {
        let lines = vec![
            "a".repeat(15), // 2 visual lines (width 10)
            "b".repeat(5),  // 1 visual line
        ];
        let wm = WrapMap::new(&lines, 10);

        // vrow 2 → logical line 1
        let (row, col) = wm.visual_to_logical(2, 3);
        assert_eq!(row, 1);
        assert_eq!(col, 3);
    }

    #[test]
    fn wrap_map_zero_width_no_panic() {
        let lines = vec!["hello".to_owned()];
        let wm = WrapMap::new(&lines, 0);
        assert_eq!(wm.total_visual_lines(), 1);
    }

    #[test]
    fn wrap_map_empty_lines() {
        let lines = vec![String::new(), String::new()];
        let wm = WrapMap::new(&lines, 40);
        assert_eq!(wm.total_visual_lines(), 2);
    }

    #[test]
    fn editor_wrap_move_up_within_same_logical_line() {
        // A long line that wraps. Moving up from continuation → first visual line.
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["a".repeat(30)];
        editor.update_wrap_map(10);
        // Place cursor at col 15 (visual line 1).
        editor.cursor = (0, 15);
        editor.move_up();
        // Should move to visual line 0, maintaining visual column.
        // vcol for col 15 = 2 + 5 = 7. Target vrow 0, vcol 7 → logical col 7.
        assert_eq!(editor.cursor, (0, 7));
    }

    #[test]
    fn editor_wrap_move_down_within_same_logical_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["a".repeat(30)];
        editor.update_wrap_map(10);
        // Place cursor at col 5 (visual line 0, vcol 5).
        editor.cursor = (0, 5);
        editor.move_down();
        // Should move to visual line 1. Target vcol 5 → logical col 10 + (5 - 2) = 13.
        assert_eq!(editor.cursor, (0, 13));
    }

    #[test]
    fn editor_wrap_ensure_cursor_visible_scrolls_visual() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["a".repeat(100)]; // many visual lines at width 10
        editor.update_wrap_map(10);
        // Place cursor at end.
        editor.cursor = (0, 99);
        editor.ensure_cursor_visible(5, 10);
        // scroll_row should be in visual line units, scroll_col always 0.
        assert_eq!(editor.scroll_col, 0);
        let (vrow, _) = editor.visual_cursor();
        assert!(editor.scroll_row <= vrow);
        assert!(editor.scroll_row + 5 > vrow);
    }
}
