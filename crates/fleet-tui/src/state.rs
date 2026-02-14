//! Application state (tabs, focus, queries).

use fleet_client::QueryResponse;
use std::time::Instant;

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

    /// Clear all text.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
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
    /// Vertical scroll offset in results pane.
    pub scroll_offset: usize,
    /// Current query status.
    pub status: TabStatus,
}

impl Tab {
    /// Create a new tab with the given ID.
    pub fn new(id: usize) -> Self {
        Self {
            id,
            editor: SimpleEditor::new(),
            result: None,
            scroll_offset: 0,
            status: TabStatus::Idle,
        }
    }

    /// Clear the editor and reset state.
    pub fn clear(&mut self) {
        self.editor.clear();
        self.result = None;
        self.scroll_offset = 0;
        self.status = TabStatus::Idle;
    }
}
