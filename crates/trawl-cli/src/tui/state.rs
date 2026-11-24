// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use ratatui::layout::Rect;
use trawl_api::zone::DisplayZone;
use trawl_client::{PaginationMeta, QueryResponse};
use trawl_engine::value::{Column, ColumnType, QueryResult, TypeFold, Value};

use super::driver::ExecuteWaiter;

#[derive(Debug, Clone, Default)]
pub struct LayoutAreas {
    pub tab_bar: Rect,
    pub editor: Option<Rect>,
    pub results: Option<Rect>,
    pub panel: Option<Rect>,
    #[allow(dead_code)]
    pub status: Rect,
    pub popup: Option<Rect>,
    /// Column header x-ranges for mouse click detection: `(x_start, x_end, original_col_index)`.
    pub column_header_ranges: Vec<(u16, u16, usize)>,
}

#[derive(Debug, Clone)]
pub struct ResultsSearch {
    pub query: String,
    pub input_active: bool,
    /// Matching cell positions: (`row_index`, `col_index`).
    pub matches: Vec<(usize, usize)>,
    pub current_match: usize,
}

impl ResultsSearch {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            input_active: true,
            matches: Vec::new(),
            current_match: 0,
        }
    }

    pub fn update_matches(
        &mut self,
        result: &QueryResult,
        config: Option<&ColumnConfig>,
        zone: DisplayZone,
    ) {
        self.matches.clear();
        if self.query.is_empty() {
            return;
        }
        let needle = self.query.to_lowercase();
        for (row_idx, row) in result.rows.iter().enumerate() {
            for (col_idx, value) in row.iter().enumerate() {
                if let Some(cfg) = config
                    && cfg.columns.get(col_idx).is_some_and(|e| e.hidden)
                {
                    continue;
                }
                // The text the grid shows, so a hit is a hit the reader sees.
                let Some(column) = result.columns.get(col_idx) else {
                    continue;
                };
                let display = crate::tui::ui::results::cell_text(&column.ty, value, zone);
                if display.to_lowercase().contains(&needle) {
                    self.matches.push((row_idx, col_idx));
                }
            }
        }
        if self.current_match >= self.matches.len() {
            self.current_match = 0;
        }
    }

    pub fn next_match(&mut self) {
        if !self.matches.is_empty() {
            self.current_match = (self.current_match + 1) % self.matches.len();
        }
    }

    pub fn prev_match(&mut self) {
        if !self.matches.is_empty() {
            self.current_match = (self.current_match + self.matches.len() - 1) % self.matches.len();
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColumnEntry {
    /// User-set width override. `None` means auto-compute.
    pub width_override: Option<u16>,
    pub hidden: bool,
    pub pinned: bool,
}

#[derive(Debug, Clone)]
pub struct ColumnConfig {
    /// Per-column entries, indexed by original column position.
    pub columns: Vec<ColumnEntry>,
    /// Column-mode cursor (original column index). `None` = not in column mode.
    pub selected: Option<usize>,
}

impl ColumnConfig {
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

    pub fn pinned_indices(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, e)| e.pinned && !e.hidden)
            .map(|(i, _)| i)
            .collect()
    }

    pub fn scrollable_indices(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.pinned && !e.hidden)
            .map(|(i, _)| i)
            .collect()
    }

    pub fn display_order(&self) -> Vec<usize> {
        let mut order = self.pinned_indices();
        order.extend(self.scrollable_indices());
        order
    }

    pub fn visible_count(&self) -> usize {
        self.columns.iter().filter(|e| !e.hidden).count()
    }

    pub fn move_cursor_left(&mut self) {
        let order = self.display_order();
        if let Some(cur) = self.selected
            && let Some(pos) = order.iter().position(|&i| i == cur)
            && pos > 0
        {
            self.selected = Some(order[pos - 1]);
        }
    }

    pub fn move_cursor_right(&mut self) {
        let order = self.display_order();
        if let Some(cur) = self.selected
            && let Some(pos) = order.iter().position(|&i| i == cur)
            && pos + 1 < order.len()
        {
            self.selected = Some(order[pos + 1]);
        }
    }

    pub fn toggle_pin_selected(&mut self) {
        if let Some(idx) = self.selected
            && let Some(entry) = self.columns.get_mut(idx)
        {
            entry.pinned = !entry.pinned;
        }
    }

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

    pub fn reset_selected_width(&mut self) {
        if let Some(idx) = self.selected
            && let Some(entry) = self.columns.get_mut(idx)
        {
            entry.width_override = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Editor,
    Results,
    Panel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainTab {
    Query,
    History,
    Schema,
    Saved,
    Dashboard,
}

pub struct DashboardState {
    pub is_admin: bool,
    pub cache: Option<trawl_client::DashboardSnapshot>,
    pub last_error: Option<String>,
    inflight: bool,
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

    pub fn set_inflight(&mut self) {
        self.inflight = true;
        self.inflight_since = Some(Instant::now());
    }

    pub fn clear_inflight(&mut self) {
        self.inflight = false;
        self.inflight_since = None;
    }

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

#[derive(Debug, Clone)]
pub struct SchemaBrowser {
    pub services: Vec<trawl_api::ServiceSchema>,
    pub common_fields: Vec<CommonField>,
    pub expanded: HashSet<String>,
    pub selected: usize,
    pub scroll: usize,
    pub filter: String,
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

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CommonField {
    pub name: String,
    pub data_type: String,
    pub service_count: usize,
    pub null_count: u64,
    pub total_count: u64,
    pub min_value: Option<String>,
    pub max_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DetailSelection {
    None,
    CommonHeader,
    CommonField { name: String },
    Service { name: String },
    ServiceField { service: String, field: String },
}

/// Envelope fields exempt from the frequency threshold, in leading display order.
///
/// Mirrors `trawl_api::value::WELL_KNOWN_LOG_FIELDS`; a test asserts parity.
const WELL_KNOWN_FIELDS: &[&str] = &["_time", "env", "service", "host", "_severity", "message"];

pub fn compute_common_fields(services: &[trawl_api::ServiceSchema]) -> Vec<CommonField> {
    use std::collections::{HashMap, HashSet};

    if services.is_empty() {
        return Vec::new();
    }

    let mut field_stats: HashMap<String, CommonField> = HashMap::new();
    // Per field, whether its min / max bound was dropped for mixing shapes.
    let mut dropped: HashMap<String, [bool; 2]> = HashMap::new();
    // Fields some service types as a timestamp. Collected up front so the
    // shape rule does not depend on which service the merge meets first.
    let timestamp_typed: HashSet<&str> = services
        .iter()
        .flat_map(|svc| &svc.columns)
        .filter(|col| {
            trawl_core::schema::CanonicalType::from_catalog(&col.data_type)
                == Some(trawl_core::schema::CanonicalType::Timestamp)
        })
        .map(|col| col.name.as_str())
        .collect();
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
            let [min_dropped, max_dropped] = dropped.entry(col.name.clone()).or_default();
            let timestamp = timestamp_typed.contains(col.name.as_str());
            fold_common_bound(
                &mut entry.min_value,
                min_dropped,
                timestamp,
                col.min_value.as_ref(),
                |v, cur| v < cur,
            );
            fold_common_bound(
                &mut entry.max_value,
                max_dropped,
                timestamp,
                col.max_value.as_ref(),
                |v, cur| v > cur,
            );
        }
    }

    let mut common = Vec::new();

    for &wk in WELL_KNOWN_FIELDS {
        if let Some(field) = field_stats.remove(wk) {
            common.push(field);
        }
    }

    let mut promoted: Vec<_> = field_stats
        .into_values()
        .filter(|f| f.service_count > threshold)
        .collect();
    promoted.sort_by(|a, b| a.name.cmp(&b.name));
    common.extend(promoted);

    common
}

/// The bounds are text. Text of one timestamp shape orders as the instants do.
/// Text of two shapes does not: a UTC and a local sample name different
/// instants, and a timestamp beside an integer (one service mid-repin) is no
/// range at all.
fn fold_common_bound(
    bound: &mut Option<String>,
    dropped: &mut bool,
    timestamp: bool,
    candidate: Option<&String>,
    replaces: fn(&str, &str) -> bool,
) {
    let Some(v) = candidate else { return };
    if *dropped {
        return;
    }
    match bound {
        None => *bound = Some(v.clone()),
        Some(cur) => {
            let (a, b) = (instant_shape(v), instant_shape(cur));
            if timestamp && (a.is_some() || b.is_some()) && a != b {
                *bound = None;
                *dropped = true;
            } else if replaces(v, cur) {
                *bound = Some(v.clone());
            }
        }
    }
}

/// `Some(utc)` when `text` is a server-rendered timestamp sample:
/// `YYYY-MM-DDTHH:MM:SS.ffffff`, then `Z` exactly when UTC-adjusted.
fn instant_shape(text: &str) -> Option<bool> {
    const SHAPE: &[u8] = b"dddd-dd-ddTdd:dd:dd.dddddd";
    let (stem, utc) = match text.strip_suffix('Z') {
        Some(stem) => (stem, true),
        None => (text, false),
    };
    let fits = stem.len() == SHAPE.len()
        && stem.bytes().zip(SHAPE).all(|(b, &want)| match want {
            b'd' => b.is_ascii_digit(),
            _ => b == want,
        });
    fits.then_some(utc)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SavedFocus {
    #[default]
    List,
    Detail,
    RunResults,
}

/// Names one Saved-tab request, so its response completes only that request.
///
/// Two requests for the same saved query get different tokens: run history
/// changes between them, so an older answer must not finish a newer request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestToken(u64);

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedRun {
    pub run_id: i64,
    pub result: trawl_api::value::QueryResult,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedDetailState {
    pub saved_id: i64,
    pub runs: Vec<trawl_api::ReportRunSummary>,
    pub total_runs: usize,
    pub run_selected: usize,
    #[allow(dead_code)]
    pub run_scroll: usize,
    pub result: Option<LoadedRun>,
    pub result_scroll: usize,
    /// The runs or run-result request this detail waits on. Only a response
    /// carrying this token applies.
    pub pending: Option<RequestToken>,
}

#[derive(Debug, Clone)]
pub struct PanelState {
    pub schema: Option<SchemaBrowser>,
    pub history_selected: usize,
    pub saved_selected: usize,
    pub saved_focus: SavedFocus,
    pub saved_detail: Option<SavedDetailState>,
    next_request: u64,
}

impl PanelState {
    pub fn new() -> Self {
        Self {
            schema: None,
            history_selected: 0,
            saved_selected: 0,
            saved_focus: SavedFocus::default(),
            saved_detail: None,
            next_request: 0,
        }
    }

    pub fn next_token(&mut self) -> RequestToken {
        self.next_request += 1;
        RequestToken(self.next_request)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScheduleField {
    #[default]
    Interval,
    Window,
    Lag,
}

impl ScheduleField {
    pub fn next(self) -> Self {
        match self {
            Self::Interval => Self::Window,
            Self::Window => Self::Lag,
            Self::Lag => Self::Interval,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Interval => Self::Lag,
            Self::Window => Self::Interval,
            Self::Lag => Self::Window,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Popup {
    Help {
        scroll: usize,
    },
    ConfirmDelete {
        saved_id: i64,
        name: String,
    },
    SaveQuery {
        editor: SimpleEditor,
    },
    EventDetail {
        row_index: usize,
        scroll: usize,
    },
    Error {
        message: String,
    },
    /// Boxed because three editors would otherwise make every `Popup` this
    /// variant's size.
    SetSchedule(Box<ScheduleForm>),
    ColumnPicker {
        selected: usize,
        scroll: usize,
    },
    CommandPalette {
        input: String,
        cursor: usize,
        selected: usize,
        scroll: usize,
        items: Vec<super::palette::PaletteItem>,
        filtered: Vec<super::palette::FilteredItem>,
        ghost: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ScheduleForm {
    pub saved_id: i64,
    pub name: String,
    pub interval: SimpleEditor,
    /// The report window: `since_last`, a duration, or empty for query mode,
    /// where the saved DSL owns its own time bounds.
    pub window: SimpleEditor,
    /// The late-arrival allowance. Empty is `0s`.
    pub lag: SimpleEditor,
    /// The run cap of the schedule in force, `None` for unlimited. The form
    /// shows it but does not edit it, and sends it back unchanged, because
    /// the PUT replaces the whole schedule.
    pub max_runs: Option<u64>,
    /// Whether the schedule in force is enabled. Kept like `max_runs`, so an
    /// edit does not resume a paused schedule.
    pub enabled: bool,
    /// Whether the form was opened on a schedule in force. Without it, an
    /// unlimited enabled schedule and no schedule look the same.
    pub schedule_loaded: bool,
    pub focus: ScheduleField,
}

impl ScheduleForm {
    /// Prefilling matters: an operator who opens the popup to change the
    /// interval must not silently drop the window the schedule covers.
    pub fn new(
        saved_id: i64,
        name: String,
        schedule: Option<&trawl_api::ScheduleResponse>,
    ) -> Self {
        let mut interval = SimpleEditor::new_single_line();
        let mut window = SimpleEditor::new_single_line();
        let mut lag = SimpleEditor::new_single_line();
        let mut max_runs = None;
        let mut enabled = true;

        if let Some(sched) = schedule {
            max_runs = sched.max_runs;
            enabled = sched.enabled;
            interval.insert_text(&sched.interval);
            if let Some(ref w) = sched.window {
                window.insert_text(w);
            }
            if let Some(ref l) = sched.lag
                && sched.lag_secs != Some(0)
            {
                lag.insert_text(l);
            }
        }

        Self {
            saved_id,
            name,
            interval,
            window,
            lag,
            max_runs,
            enabled,
            schedule_loaded: schedule.is_some(),
            focus: ScheduleField::default(),
        }
    }

    pub fn focused_mut(&mut self) -> &mut SimpleEditor {
        match self.focus {
            ScheduleField::Interval => &mut self.interval,
            ScheduleField::Window => &mut self.window,
            ScheduleField::Lag => &mut self.lag,
        }
    }

    pub fn submission(&self) -> Option<trawl_api::SetScheduleRequest> {
        let interval = self.interval.text().trim().to_owned();
        if interval.is_empty() {
            return None;
        }

        let optional = |editor: &SimpleEditor| {
            let text = editor.text().trim().to_owned();
            (!text.is_empty()).then_some(text)
        };

        Some(trawl_api::SetScheduleRequest {
            interval,
            max_runs: self.max_runs,
            enabled: self.enabled,
            window: optional(&self.window),
            lag: optional(&self.lag),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartView {
    Table,
    LineChart,
    Sparkline,
    BarChart,
}

impl ChartView {
    pub fn next_for(self, is_timechart: bool, is_bar_chartable: bool) -> Self {
        match self {
            Self::Table if is_timechart => Self::LineChart,
            Self::LineChart => Self::Sparkline,
            Self::Sparkline if is_timechart => Self::Table,
            Self::Table if is_bar_chartable => Self::BarChart,
            _ => Self::Table,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TabStatus {
    Idle,
    Running {
        #[allow(dead_code)]
        start: Instant,
    },
    Success {
        duration_ms: u64,
    },
    Error {
        message: String,
        details: Vec<trawl_client::ErrorDetail>,
        /// The query text the failed run sent, which the details' spans
        /// index. `None` for an error raised before anything was sent: a
        /// format refusal or a cancel.
        query: Option<String>,
    },
}

const CONTINUATION_INDENT: usize = 2;

/// The first visual line of each logical line gets the full viewport width.
/// Continuation visual lines are indented by `CONTINUATION_INDENT` chars
/// and get `width - CONTINUATION_INDENT` chars of content.
#[derive(Debug, Clone)]
pub struct WrapMap {
    /// For each logical line: char offsets where visual line breaks occur.
    /// Always starts with 0. E.g. `[0, 40, 78]` means 3 visual lines.
    line_breaks: Vec<Vec<usize>>,
    viewport_width: usize,
}

impl WrapMap {
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

    pub fn rebuild(&mut self, lines: &[String], width: usize) {
        self.viewport_width = width;
        self.line_breaks = lines
            .iter()
            .map(|l| Self::compute_breaks(l, width))
            .collect();
    }

    fn compute_breaks(line: &str, width: usize) -> Vec<usize> {
        let mut breaks = vec![0usize];
        if width == 0 {
            return breaks;
        }

        let char_count = line.chars().count();
        if char_count <= width {
            return breaks;
        }

        let mut offset = width;
        breaks.push(offset);

        let cont_width = width.saturating_sub(CONTINUATION_INDENT).max(1);
        while offset + cont_width < char_count {
            offset += cont_width;
            breaks.push(offset);
        }

        breaks
    }

    /// `vcol` includes the continuation indent offset for wrapped lines.
    pub fn logical_to_visual(&self, row: usize, col: usize) -> (usize, usize) {
        let mut vrow: usize = 0;
        for r in 0..row.min(self.line_breaks.len()) {
            vrow += self.line_breaks[r].len();
        }

        let breaks = self
            .line_breaks
            .get(row)
            .map_or(&[0usize][..], |v| v.as_slice());

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

    pub fn visual_to_logical(&self, vrow: usize, vcol: usize) -> (usize, usize) {
        let mut remaining = vrow;

        for (row, breaks) in self.line_breaks.iter().enumerate() {
            let num_visual = breaks.len();
            if remaining < num_visual {
                let seg = remaining;
                let base = breaks[seg];
                let local_col = if seg == 0 {
                    vcol
                } else {
                    vcol.saturating_sub(CONTINUATION_INDENT)
                };

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

        let last_row = self.line_breaks.len().saturating_sub(1);
        (last_row, 0)
    }

    pub fn total_visual_lines(&self) -> usize {
        self.line_breaks.iter().map(Vec::len).sum()
    }

    #[allow(dead_code)]
    pub fn visual_lines_for(&self, logical_row: usize) -> usize {
        self.line_breaks.get(logical_row).map_or(1, Vec::len)
    }

    pub fn breaks_for(&self, logical_row: usize) -> &[usize] {
        self.line_breaks
            .get(logical_row)
            .map_or(&[0], |v| v.as_slice())
    }

    pub fn continuation_indent() -> usize {
        CONTINUATION_INDENT
    }

    #[allow(dead_code)]
    pub fn viewport_width(&self) -> usize {
        self.viewport_width
    }
}

#[derive(Debug, Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    cursor: (usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Newline,
    Paste,
    Other,
}

#[derive(Debug, Clone)]
struct UndoStack {
    history: Vec<EditorSnapshot>,
    position: usize,
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

    fn push(&mut self, snapshot: EditorSnapshot) {
        self.history.truncate(self.position);
        self.history.push(snapshot);
        self.position = self.history.len();

        if self.history.len() > self.max_size {
            let excess = self.history.len() - self.max_size;
            self.history.drain(..excess);
            self.position = self.history.len();
        }
    }

    fn undo(&mut self) -> Option<&EditorSnapshot> {
        if self.position > 0 {
            self.position -= 1;
            Some(&self.history[self.position])
        } else {
            None
        }
    }

    fn redo(&mut self) -> Option<&EditorSnapshot> {
        if self.position + 1 < self.history.len() {
            self.position += 1;
            Some(&self.history[self.position])
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct SimpleEditor {
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
    pub selection_anchor: Option<(usize, usize)>,
    undo_stack: UndoStack,
    last_edit_kind: Option<EditKind>,
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

    pub fn new_single_line() -> Self {
        let mut editor = Self::new();
        editor.single_line = true;
        editor
    }

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

    pub fn visual_cursor(&self) -> (usize, usize) {
        if let Some(ref wm) = self.wrap_map {
            wm.logical_to_visual(self.cursor.0, self.cursor.1)
        } else {
            self.cursor
        }
    }

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

    fn char_to_byte(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map_or(s.len(), |(byte_idx, _)| byte_idx)
    }

    fn char_count(s: &str) -> usize {
        s.chars().count()
    }

    #[allow(dead_code)]
    pub fn current_line(&self) -> &str {
        &self.lines[self.cursor.0]
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn insert_char(&mut self, ch: char) {
        self.maybe_snapshot(EditKind::Insert);
        self.delete_selection();
        let (row, col) = self.cursor;
        let byte_idx = Self::char_to_byte(&self.lines[row], col);
        self.lines[row].insert(byte_idx, ch);
        self.cursor.1 += 1;
    }

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
            let current = self.lines.remove(row);
            let prev_len = Self::char_count(&self.lines[row - 1]);
            self.lines[row - 1].push_str(&current);
            self.cursor = (row - 1, prev_len);
        }
    }

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
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
    }

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

    pub fn move_left(&mut self) {
        self.desired_col = None;
        if self.cursor.1 > 0 {
            self.cursor.1 -= 1;
        } else if self.cursor.0 > 0 {
            self.cursor.0 -= 1;
            self.cursor.1 = Self::char_count(&self.lines[self.cursor.0]);
        }
    }

    pub fn move_right(&mut self) {
        self.desired_col = None;
        if self.cursor.1 < Self::char_count(&self.lines[self.cursor.0]) {
            self.cursor.1 += 1;
        } else if self.cursor.0 < self.lines.len() - 1 {
            self.cursor.0 += 1;
            self.cursor.1 = 0;
        }
    }

    pub fn move_to_line_start(&mut self) {
        self.desired_col = None;
        self.cursor.1 = 0;
    }

    pub fn move_to_line_end(&mut self) {
        self.desired_col = None;
        self.cursor.1 = Self::char_count(&self.lines[self.cursor.0]);
    }

    fn is_word_char(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    pub fn find_word_boundary_left(&self, row: usize, col: usize) -> (usize, usize) {
        if col == 0 {
            if row > 0 {
                return (row - 1, Self::char_count(&self.lines[row - 1]));
            }
            return (0, 0);
        }

        let line: Vec<char> = self.lines[row].chars().collect();
        let mut c = col;

        while c > 0 && !Self::is_word_char(line[c - 1]) {
            c -= 1;
        }
        while c > 0 && Self::is_word_char(line[c - 1]) {
            c -= 1;
        }

        (row, c)
    }

    pub fn find_word_boundary_right(&self, row: usize, col: usize) -> (usize, usize) {
        let line_chars = Self::char_count(&self.lines[row]);
        if col >= line_chars {
            if row < self.lines.len() - 1 {
                return (row + 1, 0);
            }
            return (row, line_chars);
        }

        let line: Vec<char> = self.lines[row].chars().collect();
        let mut c = col;

        while c < line.len() && Self::is_word_char(line[c]) {
            c += 1;
        }
        while c < line.len() && !Self::is_word_char(line[c]) {
            c += 1;
        }

        (row, c)
    }

    pub fn move_word_left(&mut self) {
        self.desired_col = None;
        let (row, col) = self.cursor;
        self.cursor = self.find_word_boundary_left(row, col);
    }

    pub fn move_word_right(&mut self) {
        self.desired_col = None;
        let (row, col) = self.cursor;
        self.cursor = self.find_word_boundary_right(row, col);
    }

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

        let byte_start = Self::char_to_byte(&self.lines[row], start_col);
        let byte_end = Self::char_to_byte(&self.lines[row], col);
        self.lines[row].drain(byte_start..byte_end);

        self.lines[row].insert_str(byte_start, text);

        let insert_char_len = text.chars().count();
        self.cursor.1 = match cursor_offset {
            Some(offset) => start_col + offset,
            None => start_col + insert_char_len,
        };
    }

    pub fn delete_word_before(&mut self) {
        self.maybe_snapshot(EditKind::Other);
        let (row, col) = self.cursor;
        let (new_row, new_col) = self.find_word_boundary_left(row, col);

        if new_row == row {
            let byte_start = Self::char_to_byte(&self.lines[row], new_col);
            let byte_end = Self::char_to_byte(&self.lines[row], col);
            self.lines[row].drain(byte_start..byte_end);
            self.cursor.1 = new_col;
        } else {
            let byte_col = Self::char_to_byte(&self.lines[row], col);
            let current = self.lines[row][byte_col..].to_owned();
            self.lines.remove(row);
            let byte_new_col = Self::char_to_byte(&self.lines[new_row], new_col);
            self.lines[new_row].truncate(byte_new_col);
            self.lines[new_row].push_str(&current);
            self.cursor = (new_row, new_col);
        }
    }

    pub fn delete_word_after(&mut self) {
        self.maybe_snapshot(EditKind::Other);
        let (row, col) = self.cursor;
        let (new_row, new_col) = self.find_word_boundary_right(row, col);

        if new_row == row {
            let byte_start = Self::char_to_byte(&self.lines[row], col);
            let byte_end = Self::char_to_byte(&self.lines[row], new_col);
            self.lines[row].drain(byte_start..byte_end);
        } else {
            let byte_col = Self::char_to_byte(&self.lines[row], col);
            self.lines[row].truncate(byte_col);
            let byte_new_col = Self::char_to_byte(&self.lines[new_row], new_col);
            let rest = self.lines[new_row][byte_new_col..].to_owned();
            self.lines.remove(new_row);
            self.lines[row].push_str(&rest);
        }
    }

    pub fn delete_to_line_start(&mut self) {
        self.maybe_snapshot(EditKind::Other);
        let (row, col) = self.cursor;
        let byte_idx = Self::char_to_byte(&self.lines[row], col);
        self.lines[row].drain(..byte_idx);
        self.cursor.1 = 0;
    }

    pub fn delete_to_line_end(&mut self) {
        self.maybe_snapshot(EditKind::Other);
        let (row, col) = self.cursor;
        let byte_idx = Self::char_to_byte(&self.lines[row], col);
        self.lines[row].truncate(byte_idx);
    }

    pub fn start_selection(&mut self) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub fn place_cursor(&mut self, row: usize, col: usize) {
        self.clear_selection();
        let row = row.min(self.lines.len().saturating_sub(1));
        let col = col.min(Self::char_count(&self.lines[row]));
        self.cursor = (row, col);
        self.desired_col = None;
    }

    pub fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.selection_anchor?;
        let cursor = self.cursor;
        if anchor <= cursor {
            Some((anchor, cursor))
        } else {
            Some((cursor, anchor))
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let ((start_row, start_col), (end_row, end_col)) = self.selection_range()?;

        if start_row == end_row {
            let byte_start = Self::char_to_byte(&self.lines[start_row], start_col);
            let byte_end = Self::char_to_byte(&self.lines[start_row], end_col);
            Some(self.lines[start_row][byte_start..byte_end].to_owned())
        } else {
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

    pub fn delete_selection(&mut self) -> bool {
        let Some(((start_row, start_col), (end_row, end_col))) = self.selection_range() else {
            return false;
        };

        if start_row == end_row {
            let byte_start = Self::char_to_byte(&self.lines[start_row], start_col);
            let byte_end = Self::char_to_byte(&self.lines[start_row], end_col);
            self.lines[start_row].drain(byte_start..byte_end);
        } else {
            let byte_end = Self::char_to_byte(&self.lines[end_row], end_col);
            let tail = self.lines[end_row][byte_end..].to_owned();
            let byte_start = Self::char_to_byte(&self.lines[start_row], start_col);
            self.lines[start_row].truncate(byte_start);
            self.lines[start_row].push_str(&tail);
            self.lines.drain((start_row + 1)..=end_row);
        }

        self.cursor = (start_row, start_col);
        self.selection_anchor = None;
        true
    }

    pub fn ensure_cursor_visible(&mut self, visible_rows: usize, visible_cols: usize) {
        if let Some(ref wm) = self.wrap_map {
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

            if visible_rows > 0 {
                if row < self.scroll_row {
                    self.scroll_row = row;
                } else if row >= self.scroll_row + visible_rows {
                    self.scroll_row = row - visible_rows + 1;
                }
            }

            if visible_cols > 0 {
                if col < self.scroll_col {
                    self.scroll_col = col;
                } else if col >= self.scroll_col + visible_cols {
                    self.scroll_col = col - visible_cols + 1;
                }
            }
        }
    }

    fn maybe_snapshot(&mut self, kind: EditKind) {
        let should_snapshot = match (self.last_edit_kind, kind) {
            (Some(prev), cur) if prev != cur => true,
            (_, EditKind::Newline | EditKind::Paste | EditKind::Other) | (None, _) => true,
            (Some(EditKind::Insert), EditKind::Insert) => {
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

    pub fn save_snapshot(&mut self) {
        self.undo_stack.push(EditorSnapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        });
        self.last_edit_kind = None;
    }

    pub fn undo(&mut self) {
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

    pub fn redo(&mut self) {
        if let Some(snapshot) = self.undo_stack.redo() {
            let snapshot = snapshot.clone();
            self.lines = snapshot.lines;
            self.cursor = snapshot.cursor;
            self.selection_anchor = None;
            self.last_edit_kind = None;
        }
    }

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

/// One buffered live row: each cell beside the type its frame gave the
/// cell's column. The buffer types its columns from these, merged, rather
/// than re-guessing from the cells, in which a timestamp is only text.
struct LiveRow {
    cells: Vec<Value>,
    types: Vec<ColumnType>,
}

pub struct LiveBuffer {
    column_names: Vec<String>,
    column_index: HashMap<String, usize>,
    rows: VecDeque<LiveRow>,
    max_rows: usize,
    /// The caps the latest aggregation snapshot reported as having dropped
    /// input (ADR-0001). Empty for a pass-through stream.
    capped: Vec<trawl_api::stream::SnapshotCap>,
}

impl LiveBuffer {
    pub fn new(max_rows: usize) -> Self {
        Self {
            column_names: Vec::new(),
            column_index: HashMap::new(),
            rows: VecDeque::new(),
            max_rows,
            capped: Vec::new(),
        }
    }

    pub fn capped(&self) -> &[trawl_api::stream::SnapshotCap] {
        &self.capped
    }

    pub fn with_column_order(mut self, columns: Vec<String>) -> Self {
        for (i, col) in columns.into_iter().enumerate() {
            self.column_index.insert(col.clone(), i);
            self.column_names.push(col);
        }
        self
    }

    pub fn push_frame(&mut self, frame: &QueryResult) {
        self.append(frame);
        while self.rows.len() > self.max_rows {
            self.rows.pop_front();
        }
    }

    pub fn to_query_response(&self) -> QueryResponse {
        let rows: Vec<Vec<Value>> = self.rows.iter().map(|row| row.cells.clone()).collect();
        let columns: Vec<Column> = self
            .column_names
            .iter()
            .enumerate()
            .map(|(idx, name)| {
                let mut fold = TypeFold::new();
                for row in &self.rows {
                    fold.observe_typed(&row.types[idx], &row.cells[idx]);
                }
                Column::new(name.clone(), fold.finish().unwrap_or(ColumnType::String))
            })
            .collect();
        let returned = rows.len();

        QueryResponse {
            execution: None,
            result: QueryResult { columns, rows },
            pagination: PaginationMeta {
                limit: self.max_rows,
                offset: 0,
                returned,
                total: returned,
            },
            degraded_fields: Vec::new(),
            // no severity token rendering: the stream carries numbers, and a live
            // tail's columns are whatever the events bring.
            severity_columns: Vec::new(),
            histogram: None,
        }
    }

    /// A snapshot is the whole answer, so it is not trimmed.
    pub fn replace_with_snapshot(
        &mut self,
        snapshot: &QueryResult,
        capped: &[trawl_api::stream::SnapshotCap],
    ) {
        self.capped = capped.to_vec();
        self.column_names.clear();
        self.column_index.clear();
        self.rows.clear();
        self.append(snapshot);
    }

    fn append(&mut self, result: &QueryResult) {
        let places: Vec<usize> = result
            .columns
            .iter()
            .map(|column| self.column_place(&column.name))
            .collect();
        let width = self.column_names.len();
        for cells in &result.rows {
            let mut row = LiveRow {
                cells: vec![Value::Null; width],
                types: vec![ColumnType::String; width],
            };
            for ((column, &place), cell) in result.columns.iter().zip(&places).zip(cells) {
                row.cells[place] = cell.clone();
                row.types[place] = column.ty.clone();
            }
            self.rows.push_back(row);
        }
    }

    fn column_place(&mut self, name: &str) -> usize {
        if let Some(&place) = self.column_index.get(name) {
            return place;
        }
        let place = self.column_names.len();
        self.column_names.push(name.to_owned());
        self.column_index.insert(name.to_owned(), place);
        for row in &mut self.rows {
            row.cells.push(Value::Null);
            row.types.push(ColumnType::String);
        }
        place
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Names one run on the query tab. Only the current run's events reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunGeneration(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunSource {
    Batch,
    Live,
}

/// The query tab's run, separate from the displayed [`TabStatus`]: a refused
/// format replaces the status while the run is still in flight.
#[derive(Debug, Default)]
pub enum Execution {
    #[default]
    Idle,
    /// A one-shot query, with the driver execute it answers (if any).
    Batch {
        generation: RunGeneration,
        handle: tokio::task::JoinHandle<()>,
        reply: Option<ExecuteWaiter>,
    },
    Live {
        generation: RunGeneration,
        handle: tokio::task::JoinHandle<()>,
    },
}

impl Execution {
    pub fn is_batch(&self) -> bool {
        matches!(self, Self::Batch { .. })
    }

    pub fn current(&self) -> Option<(RunGeneration, RunSource)> {
        match self {
            Self::Idle => None,
            Self::Batch { generation, .. } => Some((*generation, RunSource::Batch)),
            Self::Live { generation, .. } => Some((*generation, RunSource::Live)),
        }
    }

    pub fn reply_pending(&self) -> bool {
        matches!(self, Self::Batch { reply: Some(reply), .. } if !reply.is_closed())
    }
}

#[derive(Debug)]
pub struct Tab {
    pub editor: SimpleEditor,
    pub result: Option<QueryResponse>,
    pub scroll_offset: usize,
    pub horizontal_scroll_offset: usize,
    pub status: TabStatus,
    pub chart_view: ChartView,
    pub selected_row: Option<usize>,
    /// Visible row count from last render frame (updated by UI each frame).
    pub last_visible_rows: usize,
    pub column_config: Option<ColumnConfig>,
    /// The tab's one run, batch or live. Only `App` starts and ends it.
    pub execution: Execution,
    /// Source of the next [`RunGeneration`]. [`Tab::clear`] keeps it, so a
    /// generation never repeats on the tab.
    next_run: u64,
    pub validation_errors: Vec<trawl_core::parser::ParseError>,
    /// Advice on the last draft that parsed and validated, kept apart from
    /// `validation_errors` because it never makes the draft invalid.
    /// Read it through [`Tab::draft_advice`], which checks it still
    /// describes the editor's text.
    pub advice: Option<DraftAdvice>,
    pub validation_dirty: bool,
    pub last_edit_time: Option<std::time::Instant>,
    pub ghost: Option<super::autocomplete::Completion>,
    /// The caps the result's live aggregate snapshot reported as having
    /// dropped input (ADR-0001). Replaced with every result, so it is empty
    /// for a batch result and for an uncapped snapshot.
    pub live_capped: Vec<trawl_api::stream::SnapshotCap>,
}

impl Tab {
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
            execution: Execution::Idle,
            next_run: 0,
            validation_errors: Vec::new(),
            advice: None,
            validation_dirty: false,
            last_edit_time: None,
            ghost: None,
            live_capped: Vec::new(),
        }
    }

    /// The run is `App::clear_tab`'s to end, so this leaves `execution` alone.
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
        self.advice = None;
        self.validation_dirty = false;
        self.last_edit_time = None;
        self.ghost = None;
        self.live_capped.clear();
    }

    pub fn next_generation(&mut self) -> RunGeneration {
        self.next_run += 1;
        RunGeneration(self.next_run)
    }

    pub fn mark_editor_dirty(&mut self) {
        self.validation_dirty = true;
        self.advice = None;
        self.last_edit_time = Some(std::time::Instant::now());
    }

    pub fn maybe_validate(&mut self) -> bool {
        if !self.validation_dirty {
            return false;
        }

        if let Some(last_edit) = self.last_edit_time
            && last_edit.elapsed() < std::time::Duration::from_millis(300)
        {
            return false;
        }

        let text = self.editor.text();
        self.advice = None;
        if text.trim().is_empty() {
            self.validation_errors.clear();
            self.validation_dirty = false;
            return true;
        }

        match trawl_core::parser::parse(&text) {
            Ok(query) => match trawl_core::emitter::validate_pipeline(&query.pipeline) {
                Ok(()) => {
                    self.validation_errors.clear();
                    let report = trawl_core::advice::advise(&query);
                    if !report.findings.is_empty() {
                        self.advice = Some(DraftAdvice { text, report });
                    }
                }
                Err(e) => self.validation_errors = e.to_parse_errors(text.len()),
            },
            Err(errors) => self.validation_errors = errors,
        }
        self.validation_dirty = false;
        true
    }

    /// Every edit marks the editor dirty, which clears the advice; the text
    /// comparison keeps stale advice off the screen if a path ever changes the
    /// text without doing so.
    #[must_use]
    pub fn draft_advice(&self) -> Option<&trawl_core::advice::AdviceReport> {
        let advice = self.advice.as_ref()?;
        (self.validation_errors.is_empty() && advice.text == self.editor.text())
            .then_some(&advice.report)
    }
}

#[derive(Debug, Clone)]
pub struct DraftAdvice {
    pub text: String,
    /// What `trawl_core::advice::advise` reported for it; never empty.
    pub report: trawl_core::advice::AdviceReport,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Value {
        Value::String(s.to_owned())
    }

    fn make_frame(cells: &[(&str, ColumnType, Value)]) -> QueryResult {
        QueryResult {
            columns: cells
                .iter()
                .map(|(name, ty, _)| Column::new(*name, ty.clone()))
                .collect(),
            rows: vec![cells.iter().map(|(_, _, cell)| cell.clone()).collect()],
        }
    }

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
        editor.cursor = (0, 5);
        editor.delete_char_at();
        assert_eq!(editor.lines, vec!["helloworld".to_owned()]);
        assert_eq!(editor.cursor, (0, 5));
    }

    #[test]
    fn editor_delete_at_end_is_noop() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('x');
        editor.delete_char_at();
        assert_eq!(editor.text(), "x");
    }

    #[test]
    fn editor_move_up_clamps_col() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["short".to_owned(), "longer line".to_owned()];
        editor.cursor = (1, 11);
        editor.move_up();
        assert_eq!(editor.cursor, (0, 5));
    }

    #[test]
    fn editor_move_down_clamps_col() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["longer line".to_owned(), "short".to_owned()];
        editor.cursor = (0, 11);
        editor.move_down();
        assert_eq!(editor.cursor, (1, 5));
    }

    #[test]
    fn editor_move_left_wraps_to_prev_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["abc".to_owned(), "def".to_owned()];
        editor.cursor = (1, 0);
        editor.move_left();
        assert_eq!(editor.cursor, (0, 3));
    }

    #[test]
    fn editor_move_right_wraps_to_next_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["abc".to_owned(), "def".to_owned()];
        editor.cursor = (0, 3);
        editor.move_right();
        assert_eq!(editor.cursor, (1, 0));
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

    #[test]
    fn editor_word_boundary_left_skips_word() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["hello world".to_owned()];
        assert_eq!(editor.find_word_boundary_left(0, 11), (0, 6));
        assert_eq!(editor.find_word_boundary_left(0, 6), (0, 0));
    }

    #[test]
    fn editor_word_boundary_left_with_symbols() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["foo_bar::baz".to_owned()];
        assert_eq!(editor.find_word_boundary_left(0, 12), (0, 9));
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
        assert_eq!(editor.find_word_boundary_right(0, 0), (0, 6));
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

    #[test]
    fn editor_undo_restores_state() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('a');
        editor.insert_char('b');
        // Trigger snapshot by changing edit kind
        editor.insert_newline();
        editor.insert_char('c');
        editor.undo();
        assert!(editor.text().len() < 4);
    }

    #[test]
    fn editor_undo_at_empty_is_noop() {
        let mut editor = SimpleEditor::new();
        editor.undo();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn editor_redo_after_undo() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("hello");
        editor.save_snapshot();
        editor.insert_text(" world");
        editor.undo();
        let after_undo = editor.text();
        editor.redo();
        let after_redo = editor.text();
        assert!(after_redo.len() > after_undo.len());
    }

    #[test]
    fn editor_redo_at_end_is_noop() {
        let mut editor = SimpleEditor::new();
        editor.insert_char('x');
        editor.redo();
        assert_eq!(editor.text(), "x");
    }

    #[test]
    fn editor_clear_pushes_undo() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("hello world");
        editor.clear();
        assert_eq!(editor.text(), "");
        editor.undo();
        assert!(!editor.text().is_empty());
    }

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

    #[test]
    fn editor_sticky_column_preserved_across_short_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec![
            "long line here".to_owned(),
            "short".to_owned(),
            "another long one".to_owned(),
        ];
        editor.cursor = (0, 10);
        editor.move_down();
        assert_eq!(editor.cursor, (1, 5));
        editor.move_down();
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
        editor.move_down();
        editor.move_left();
        assert_eq!(editor.cursor, (1, 4));
        editor.move_down();
        assert_eq!(editor.cursor, (2, 4));
    }

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
        editor.move_left();
        editor.insert_newline();
        assert_eq!(editor.lines, vec!["caf", "é"]);
        assert_eq!(editor.cursor, (1, 0));
    }

    #[test]
    fn editor_select_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("a∂b");
        editor.cursor = (0, 1);
        editor.selection_anchor = Some((0, 3));
        assert_eq!(editor.selected_text().unwrap(), "∂b");
    }

    #[test]
    fn editor_delete_selection_multibyte() {
        let mut editor = SimpleEditor::new();
        editor.insert_text("hé∂lo");
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
        editor.cursor = (0, 3);
        editor.delete_to_line_end();
        assert_eq!(editor.text(), "∂∂∂");
        editor.delete_to_line_start();
        assert_eq!(editor.text(), "");
    }

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
        buf.push_frame(&make_frame(&[
            ("host", ColumnType::String, text("web-1")),
            ("level", ColumnType::String, text("error")),
        ]));
        assert_eq!(buf.len(), 1);

        let resp = buf.to_query_response();
        assert_eq!(resp.result.columns.len(), 2);
        assert_eq!(resp.result.rows.len(), 1);
    }

    #[test]
    fn live_buffer_column_union_extends_with_nulls() {
        let mut buf = LiveBuffer::new(100);

        buf.push_frame(&make_frame(&[
            ("host", ColumnType::String, text("web-1")),
            ("level", ColumnType::String, text("info")),
        ]));

        buf.push_frame(&make_frame(&[
            ("host", ColumnType::String, text("web-2")),
            ("service", ColumnType::String, text("nginx")),
        ]));

        let resp = buf.to_query_response();
        assert_eq!(resp.result.columns.len(), 3);

        let first_row = &resp.result.rows[0];
        assert_eq!(first_row.len(), 3);
        assert_eq!(first_row[2], Value::Null);

        let second_row = &resp.result.rows[1];
        assert_eq!(second_row[1], Value::Null);
        assert_eq!(second_row[2], Value::String("nginx".to_owned()));
    }

    #[test]
    fn live_buffer_caps_at_max_rows() {
        let mut buf = LiveBuffer::new(3);

        for i in 0..5 {
            buf.push_frame(&make_frame(&[(
                "n",
                ColumnType::Integer,
                Value::Integer(i),
            )]));
        }

        assert_eq!(buf.len(), 3);

        let resp = buf.to_query_response();
        assert_eq!(resp.result.rows[0][0], Value::Integer(2));
        assert_eq!(resp.result.rows[2][0], Value::Integer(4));
    }

    /// The buffer's columns are the frames' types merged: a timestamp
    /// stays `timestamp` though its cells are text, a null cell (typed
    /// `string` by its frame for want of evidence) adds nothing, and two
    /// frames that disagree make the column `mixed`.
    #[test]
    fn live_buffer_types_columns_from_the_frames() {
        let mut buf = LiveBuffer::new(100);
        buf.push_frame(&make_frame(&[
            (
                "_time",
                ColumnType::Timestamp,
                text("2026-01-15T09:00:00.000000Z"),
            ),
            ("active", ColumnType::Boolean, Value::Boolean(true)),
            ("code", ColumnType::Integer, Value::Integer(404)),
            ("tag", ColumnType::String, Value::Null),
        ]));
        buf.push_frame(&make_frame(&[
            (
                "_time",
                ColumnType::Timestamp,
                text("2026-01-15T09:00:01.000000Z"),
            ),
            ("code", ColumnType::String, text("E_TIMEOUT")),
            ("tag", ColumnType::Float, Value::Float(1.5)),
        ]));

        let resp = buf.to_query_response();
        assert_eq!(
            resp.result.columns,
            vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("active", ColumnType::Boolean),
                Column::new("code", ColumnType::Mixed),
                Column::new("tag", ColumnType::Float),
            ]
        );
    }

    #[test]
    fn live_buffer_snapshot_replaces_everything() {
        let mut buf = LiveBuffer::new(1);
        buf.push_frame(&make_frame(&[("host", ColumnType::String, text("web-1"))]));

        let snapshot = QueryResult {
            columns: vec![
                Column::new("service", ColumnType::String),
                Column::new("total", ColumnType::Integer),
            ],
            rows: vec![
                vec![text("nginx"), Value::UInt(u64::MAX)],
                vec![text("api"), Value::Null],
            ],
        };
        let caps = [trawl_api::stream::SnapshotCap {
            cap: trawl_api::stream::SnapshotCapKind::Groups,
            limit: 10_000,
        }];
        buf.replace_with_snapshot(&snapshot, &caps);

        assert_eq!(buf.to_query_response().result, snapshot);
        assert_eq!(buf.capped(), caps);
    }

    #[test]
    fn live_buffer_keeps_big_integers_from_the_wire_exactly() {
        let mut buf = LiveBuffer::new(10);
        for id in [
            "9007199254740993",
            "9223372036854775808",
            "18446744073709551615",
        ] {
            let frame: QueryResult = serde_json::from_str(&format!(
                r#"{{"columns":[{{"name":"id","type":"integer"}}],"rows":[["{id}"]]}}"#
            ))
            .expect("a data frame decodes");
            buf.push_frame(&frame);
        }
        let exact = vec![
            vec![Value::Integer(9_007_199_254_740_993)],
            vec![Value::UInt(9_223_372_036_854_775_808)],
            vec![Value::UInt(u64::MAX)],
        ];
        let resp = buf.to_query_response();
        assert_eq!(
            resp.result.columns,
            vec![Column::new("id", ColumnType::Integer)]
        );
        assert_eq!(resp.result.rows, exact);

        let snapshot: trawl_api::stream::SnapshotFrame = serde_json::from_str(
            r#"{"columns":[{"name":"id","type":"integer"}],
                "rows":[["9007199254740993"],["9223372036854775808"],["18446744073709551615"]]}"#,
        )
        .expect("a snapshot frame decodes");
        buf.replace_with_snapshot(&snapshot.result, &snapshot.capped);
        assert_eq!(buf.to_query_response().result.rows, exact);
    }

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
        assert!(names.contains(&"_time"));
        assert!(names.contains(&"host"));
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
        assert!(names.contains(&"common_f"));
        assert!(!names.contains(&"rare_f"));
    }

    #[test]
    fn wrap_map_short_line_no_breaks() {
        let lines = vec!["hello".to_owned()];
        let wm = WrapMap::new(&lines, 40);
        assert_eq!(wm.total_visual_lines(), 1);
        assert_eq!(wm.breaks_for(0), &[0]);
    }

    #[test]
    fn wrap_map_exact_width_no_break() {
        let lines = vec!["a".repeat(40)];
        let wm = WrapMap::new(&lines, 40);
        assert_eq!(wm.total_visual_lines(), 1);
    }

    #[test]
    fn wrap_map_one_char_over_wraps() {
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

        assert_eq!(wm.logical_to_visual(0, 0), (0, 0));
        assert_eq!(wm.logical_to_visual(0, 5), (0, 5));
        // Col 10 → vrow 1, vcol CONTINUATION_INDENT + 0 = 2
        assert_eq!(wm.logical_to_visual(0, 10), (1, 2));
        assert_eq!(wm.logical_to_visual(0, 15), (1, 7));
        assert_eq!(wm.logical_to_visual(0, 18), (2, 2));
    }

    #[test]
    fn wrap_map_visual_to_logical_round_trip() {
        let lines = vec!["a".repeat(30)];
        let wm = WrapMap::new(&lines, 10);

        for col in [0, 5, 10, 15, 18, 25, 29] {
            let (vrow, vcol) = wm.logical_to_visual(0, col);
            let (row, col_back) = wm.visual_to_logical(vrow, vcol);
            assert_eq!(row, 0);
            assert_eq!(col_back, col, "round-trip failed for col={col}");
        }
    }

    #[test]
    fn wrap_map_multi_logical_lines() {
        let lines = vec!["a".repeat(15), "b".repeat(5)];
        let wm = WrapMap::new(&lines, 10);
        assert_eq!(wm.total_visual_lines(), 3);

        assert_eq!(wm.logical_to_visual(1, 0), (2, 0));
        assert_eq!(wm.logical_to_visual(1, 3), (2, 3));
    }

    #[test]
    fn wrap_map_visual_to_logical_second_line() {
        let lines = vec!["a".repeat(15), "b".repeat(5)];
        let wm = WrapMap::new(&lines, 10);

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
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["a".repeat(30)];
        editor.update_wrap_map(10);
        editor.cursor = (0, 15);
        editor.move_up();
        // vcol for col 15 = 2 + 5 = 7. Target vrow 0, vcol 7 → logical col 7.
        assert_eq!(editor.cursor, (0, 7));
    }

    #[test]
    fn editor_wrap_move_down_within_same_logical_line() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["a".repeat(30)];
        editor.update_wrap_map(10);
        editor.cursor = (0, 5);
        editor.move_down();
        // Should move to visual line 1. Target vcol 5 → logical col 10 + (5 - 2) = 13.
        assert_eq!(editor.cursor, (0, 13));
    }

    #[test]
    fn editor_wrap_ensure_cursor_visible_scrolls_visual() {
        let mut editor = SimpleEditor::new();
        editor.lines = vec!["a".repeat(100)];
        editor.update_wrap_map(10);
        editor.cursor = (0, 99);
        editor.ensure_cursor_visible(5, 10);
        assert_eq!(editor.scroll_col, 0);
        let (vrow, _) = editor.visual_cursor();
        assert!(editor.scroll_row <= vrow);
        assert!(editor.scroll_row + 5 > vrow);
    }

    #[test]
    fn search_matches_a_timestamp_as_the_grid_shows_it() {
        let result = QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("note", ColumnType::String),
            ],
            rows: vec![vec![
                Value::String("2026-01-15T04:30:01.250000Z".into()),
                Value::String("T10".into()),
            ]],
        };
        let india = "+05:30".parse::<DisplayZone>().unwrap();
        let mut search = ResultsSearch::new();
        search.query = "10:00:01.25 +05:30".to_owned();
        search.update_matches(&result, None, india);
        assert_eq!(search.matches, vec![(0, 0)]);
        search.query = "t10".to_owned();
        search.update_matches(&result, None, india);
        assert_eq!(search.matches, vec![(0, 1)], "only the cell that shows it");
        search.query = "04:30".to_owned();
        search.update_matches(&result, None, india);
        assert!(search.matches.is_empty(), "the UTC wire text is not shown");
        search.update_matches(&result, None, DisplayZone::Utc);
        assert_eq!(search.matches, vec![(0, 0)]);
    }

    fn validate_draft(tab: &mut Tab, text: &str) {
        tab.editor.clear();
        tab.editor.insert_text(text);
        tab.mark_editor_dirty();
        tab.last_edit_time = None;
        assert!(tab.maybe_validate(), "validation runs");
    }

    #[test]
    fn query_advice_lifecycle_tracks_the_draft() {
        let mut tab = Tab::new();

        validate_draft(&mut tab, r#""10.244.0.83" -service=coredns"#);
        let report = tab.draft_advice().expect("advice for the draft");
        assert_eq!(report.findings.len(), 1);
        assert_eq!(
            report.findings[0].kind,
            trawl_core::advice::AdviceKind::NegatedFieldShape
        );
        assert_eq!(report.findings[0].span, 14..30);
        assert!(tab.validation_errors.is_empty());

        tab.editor.insert_char(' ');
        tab.mark_editor_dirty();
        assert!(tab.draft_advice().is_none(), "a dirty edit clears advice");

        validate_draft(&mut tab, "service=trawld event_type in (a, b)");
        assert_eq!(
            tab.draft_advice().expect("in advice").findings[0].kind,
            trawl_core::advice::AdviceKind::SearchStageIn
        );
        tab.editor.clear();
        tab.editor.insert_text("service=trawld");
        assert!(
            tab.draft_advice().is_none(),
            "advice describes another text"
        );

        validate_draft(&mut tab, "service=trawld");
        assert!(tab.advice.is_none());

        validate_draft(&mut tab, "service=trawld event_type in (a, b)");
        assert!(tab.draft_advice().is_some());
        validate_draft(&mut tab, r#"* | where m matches "^\s+at ""#);
        assert!(tab.advice.is_none(), "a parse error leaves no advice");
        assert_eq!(tab.validation_errors.len(), 1);
        assert_eq!(tab.validation_errors[0].span, 22..24);

        validate_draft(&mut tab, "service=trawld event_type in (a, b)");
        assert!(tab.draft_advice().is_some());
        validate_draft(
            &mut tab,
            "service=trawld event_type in (a, b) | stats nosuchfn(x)",
        );
        assert!(tab.advice.is_none(), "a semantic error leaves no advice");
        assert!(!tab.validation_errors.is_empty());

        validate_draft(&mut tab, "service=trawld event_type in (a, b)");
        tab.validation_errors = vec![trawl_core::parser::ParseError {
            message: "boom".to_owned(),
            span: 0..1,
            label: None,
            hint: None,
        }];
        assert!(tab.draft_advice().is_none(), "errors win");
        tab.validation_errors.clear();
        assert!(tab.draft_advice().is_some());

        validate_draft(&mut tab, "   ");
        assert!(tab.advice.is_none(), "blank text leaves no advice");

        validate_draft(&mut tab, "service=trawld event_type in (a, b)");
        assert!(tab.draft_advice().is_some());
        tab.clear();
        assert!(tab.advice.is_none(), "Tab::clear leaves no advice");
    }
}
