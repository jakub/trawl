//! Application state (tabs, focus, queries).

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use fleet_client::{PaginationMeta, QueryResponse};
use fleet_engine::value::{Column, QueryResult, Value};

/// Which pane has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Query editor is focused.
    Editor,
    /// Results table is focused.
    Results,
}

/// Active sidebar overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sidebar {
    /// Help overlay (F1).
    Help,
    /// Schema browser (F2).
    Schema,
    /// Query history (F3).
    History,
    /// Saved queries (F4).
    Saved,
}

/// Active popup overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Popup {
    /// Confirm deletion of saved query.
    ConfirmDelete {
        /// ID of the saved query to delete.
        saved_id: i64,
        /// Name of the query being deleted.
        name: String,
    },
    /// Text input for saving current query.
    SaveQuery {
        /// Current input text.
        input: String,
    },
}

/// Chart visualization mode for results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Variants used when chart rendering is implemented
pub enum ChartView {
    /// Tabular view (default).
    Table,
    /// Sparkline view (compact ascii-art).
    Sparkline,
    /// Bar chart view (categorical).
    BarChart,
    /// Line chart view (continuous).
    LineChart,
}

impl ChartView {
    /// Cycle to the next view mode.
    #[allow(dead_code)] // Used when 'v' keybinding is implemented
    pub fn next(self) -> Self {
        match self {
            // Only cycle between Table and Sparkline for now
            // (bar/line charts not yet implemented)
            Self::Table => Self::Sparkline,
            Self::Sparkline | Self::BarChart | Self::LineChart => Self::Table,
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
    },
}

/// Simple text editor for DSL queries.
#[derive(Debug, Clone)]
pub struct SimpleEditor {
    /// Lines of text.
    pub lines: Vec<String>,
    /// Cursor position (row, column).
    pub cursor: (usize, usize),
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
        }
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
        let (row, col) = self.cursor;
        self.lines[row].insert(col, ch);
        self.cursor.1 += 1;
    }

    /// Insert a newline at the cursor position.
    pub fn insert_newline(&mut self) {
        let (row, col) = self.cursor;
        let current_line = self.lines[row].clone();
        let (before, after) = current_line.split_at(col);
        before.clone_into(&mut self.lines[row]);
        self.lines.insert(row + 1, after.to_owned());
        self.cursor = (row + 1, 0);
    }

    /// Delete character before cursor (backspace).
    pub fn delete_char_before(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            self.lines[row].remove(col - 1);
            self.cursor.1 -= 1;
        } else if row > 0 {
            // Join with previous line
            let current = self.lines.remove(row);
            let prev_len = self.lines[row - 1].len();
            self.lines[row - 1].push_str(&current);
            self.cursor = (row - 1, prev_len);
        }
    }

    /// Delete character at cursor (delete key).
    pub fn delete_char_at(&mut self) {
        let (row, col) = self.cursor;
        if col < self.lines[row].len() {
            self.lines[row].remove(col);
        } else if row < self.lines.len() - 1 {
            // Join with next line
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
    }

    /// Move cursor up.
    pub fn move_up(&mut self) {
        if self.cursor.0 > 0 {
            self.cursor.0 -= 1;
            self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
        }
    }

    /// Move cursor down.
    pub fn move_down(&mut self) {
        if self.cursor.0 < self.lines.len() - 1 {
            self.cursor.0 += 1;
            self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
        }
    }

    /// Move cursor left.
    pub fn move_left(&mut self) {
        if self.cursor.1 > 0 {
            self.cursor.1 -= 1;
        } else if self.cursor.0 > 0 {
            self.cursor.0 -= 1;
            self.cursor.1 = self.lines[self.cursor.0].len();
        }
    }

    /// Move cursor right.
    pub fn move_right(&mut self) {
        if self.cursor.1 < self.lines[self.cursor.0].len() {
            self.cursor.1 += 1;
        } else if self.cursor.0 < self.lines.len() - 1 {
            self.cursor.0 += 1;
            self.cursor.1 = 0;
        }
    }

    /// Move cursor to start of line.
    pub fn move_to_line_start(&mut self) {
        self.cursor.1 = 0;
    }

    /// Move cursor to end of line.
    pub fn move_to_line_end(&mut self) {
        self.cursor.1 = self.lines[self.cursor.0].len();
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
                return (row - 1, self.lines[row - 1].len());
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
        let line_len = self.lines[row].len();
        if col >= line_len {
            // Wrap to start of next line
            if row < self.lines.len() - 1 {
                return (row + 1, 0);
            }
            return (row, line_len);
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

    /// Move cursor one word to the left.
    pub fn move_word_left(&mut self) {
        let (row, col) = self.cursor;
        self.cursor = self.find_word_boundary_left(row, col);
    }

    /// Move cursor one word to the right.
    pub fn move_word_right(&mut self) {
        let (row, col) = self.cursor;
        self.cursor = self.find_word_boundary_right(row, col);
    }

    /// Clear all text.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
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

/// A single tab in the TUI.
#[derive(Debug)]
pub struct Tab {
    /// Unique tab ID.
    #[allow(dead_code)] // Used for tab identification in future features
    pub id: usize,
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
}

impl Tab {
    /// Create a new tab with the given ID.
    pub fn new(id: usize) -> Self {
        Self {
            id,
            editor: SimpleEditor::new(),
            result: None,
            scroll_offset: 0,
            horizontal_scroll_offset: 0,
            status: TabStatus::Idle,
            chart_view: ChartView::Table,
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
