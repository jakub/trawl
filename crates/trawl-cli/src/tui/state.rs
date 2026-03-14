//! Application state (tabs, focus, queries).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use trawl_client::{PaginationMeta, QueryResponse};
use trawl_engine::value::{Column, QueryResult, Value};

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
    pub fn update_matches(&mut self, result: &QueryResult) {
        self.matches.clear();
        if self.query.is_empty() {
            return;
        }
        let needle = self.query.to_lowercase();
        for (row_idx, row) in result.rows.iter().enumerate() {
            for (col_idx, value) in row.iter().enumerate() {
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
        // Clamp current_match
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
}

/// A profiled column from a service sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfiledColumn {
    /// Column name.
    pub name: String,
    /// `DuckDB` data type string (from schema cache).
    pub data_type: String,
    /// Number of non-null values in the sample.
    pub non_null_count: usize,
    /// Total rows sampled.
    pub total_rows: usize,
    /// Up to 8 distinct sample values (stringified).
    pub sample_values: Vec<String>,
}

impl ProfiledColumn {
    /// Population percentage (0-100).
    #[allow(dead_code)] // Used by sidebar renderer (not yet implemented).
    #[allow(clippy::cast_possible_truncation)] // .min(100) guarantees value fits in u8
    pub fn population_pct(&self) -> u8 {
        if self.total_rows == 0 {
            return 0;
        }
        ((self.non_null_count * 100) / self.total_rows).min(100) as u8
    }
}

/// Catalog summary for the schema browser header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSummary {
    /// Earliest date in partition directories.
    pub earliest_date: Option<String>,
    /// Latest date in partition directories.
    pub latest_date: Option<String>,
    /// Total parquet file size in bytes.
    pub total_bytes: u64,
    /// Total parquet file count.
    pub file_count: u64,
    /// Hot buffer event count.
    pub hot_buffer_events: Option<u64>,
}

/// Tree state for the schema browser.
#[derive(Debug, Clone)]
pub struct SchemaTree {
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

impl SchemaTree {
    pub fn new() -> Self {
        Self {
            expanded: HashSet::new(),
            selected: 0,
            scroll: 0,
            filter: String::new(),
            filter_active: false,
        }
    }
}

/// Panel state for non-Query tabs (schema, history, saved).
#[derive(Debug, Clone)]
pub struct PanelState {
    /// Schema tree navigation state.
    pub schema: SchemaTree,
    /// Selected index in the history list.
    pub history_selected: usize,
    /// Selected index in the saved queries list.
    pub saved_selected: usize,
    /// Catalog summary from enriched schema response.
    #[allow(dead_code)] // Used when reports panel is implemented.
    pub catalog: Option<CatalogSummary>,
}

impl PanelState {
    pub fn new(catalog: Option<CatalogSummary>) -> Self {
        Self {
            schema: SchemaTree::new(),
            history_selected: 0,
            saved_selected: 0,
            catalog,
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
}

/// Chart visualization mode for results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartView {
    /// Tabular view (default).
    Table,
    /// Sparkline view (compact ascii-art).
    Sparkline,
}

impl ChartView {
    /// Cycle to the next view mode.
    pub fn next(self) -> Self {
        match self {
            Self::Table => Self::Sparkline,
            Self::Sparkline => Self::Table,
        }
    }
}

/// Status of a tab's current query.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants used when query execution is implemented
pub enum TabStatus {
    /// No query running.
    Idle,
    /// Query is executing.
    Running {
        /// When the query started.
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
    /// Cursor position (row, column).
    pub cursor: (usize, usize),
    /// Desired column for vertical movement (sticky column).
    /// Set on horizontal movement, used by `move_up()`/`move_down()` to
    /// maintain column position across lines of varying length.
    desired_col: Option<usize>,
    /// Vertical scroll offset (first visible line).
    pub scroll_row: usize,
    /// Horizontal scroll offset (first visible column).
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
        }
    }

    /// Create a new single-line editor (for popup inputs, search bars, etc.).
    pub fn new_single_line() -> Self {
        let mut editor = Self::new();
        editor.single_line = true;
        editor
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

    /// Move cursor up (uses sticky column).
    pub fn move_up(&mut self) {
        if self.cursor.0 > 0 {
            let target_col = self.desired_col.unwrap_or(self.cursor.1);
            self.cursor.0 -= 1;
            let line_chars = Self::char_count(&self.lines[self.cursor.0]);
            self.cursor.1 = target_col.min(line_chars);
            if self.desired_col.is_none() {
                self.desired_col = Some(target_col);
            }
        }
    }

    /// Move cursor down (uses sticky column).
    pub fn move_down(&mut self) {
        if self.cursor.0 < self.lines.len() - 1 {
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
    /// Call after every cursor movement or content change.
    pub fn ensure_cursor_visible(&mut self, visible_rows: usize, visible_cols: usize) {
        let (row, col) = self.cursor;

        // Vertical scrolling
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
    /// Cached column widths (invalidated on new result).
    pub column_widths: Option<Vec<u16>>,
    /// Handle to the running query task (for cancellation).
    pub query_task: Option<tokio::task::JoinHandle<()>>,
    /// Real-time validation errors from local parsing (separate from execution errors).
    pub validation_errors: Vec<trawl_core::parser::ParseError>,
    /// Whether the editor has been modified since last validation.
    pub validation_dirty: bool,
    /// Timestamp of the last editor modification (for debounce).
    pub last_edit_time: Option<std::time::Instant>,
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
            column_widths: None,
            query_task: None,
            validation_errors: Vec::new(),
            validation_dirty: false,
            last_edit_time: None,
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
        self.column_widths = None;
        self.validation_errors.clear();
        self.validation_dirty = false;
        self.last_edit_time = None;
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
        // From end (col 12) — skips "baz", skips "::", lands at start of "foo_bar"? No.
        // Actually: col 12, skip non-word (none at 11, 'z' is word), skip word "baz" → col 9
        // Wait: col 12: chars[11] = 'z' (word), so skip word first? No, the algorithm is:
        // skip non-word first, then word. If at word char, skip nothing then skip word.
        // Let me trace: c=12, chars[11]='z' word → skip non-word: nothing. skip word: z,a,b → c=9
        assert_eq!(editor.find_word_boundary_left(0, 12), (0, 9));
        // From col 9: chars[8]=':' not word → skip non-word: ::, c=7. skip word: r,a,b,_,o,o,f → c=0
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
}
