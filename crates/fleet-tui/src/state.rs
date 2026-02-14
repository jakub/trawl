//! Application state (tabs, focus, queries).

use fleet_client::QueryResponse;
use std::time::Instant;
use tui_textarea::TextArea;

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

/// A single tab in the TUI.
#[derive(Debug)]
pub struct Tab {
    /// Unique tab ID.
    #[allow(dead_code)] // Used for tab management (upcoming)
    pub id: usize,
    /// Query editor (tui-textarea).
    pub editor: TextArea<'static>,
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
        let mut editor = TextArea::default();
        editor.set_placeholder_text("enter DSL query (ctrl+enter to execute)");

        Self {
            id,
            editor,
            result: None,
            scroll_offset: 0,
            status: TabStatus::Idle,
        }
    }

    /// Clear the editor and reset state.
    pub fn clear(&mut self) {
        self.editor = TextArea::default();
        self.editor
            .set_placeholder_text("enter DSL query (ctrl+enter to execute)");
        self.result = None;
        self.scroll_offset = 0;
        self.status = TabStatus::Idle;
    }
}
