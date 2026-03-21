// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Command palette types and fuzzy matching logic.
//!
//! Pure logic module — no I/O, no ratatui dependencies.

use std::collections::HashSet;

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32String};

use super::state::MainTab;

/// A single item in the command palette.
#[derive(Debug, Clone)]
pub struct PaletteItem {
    /// Primary display text (e.g. "Execute Query", "nginx errors by host").
    pub label: String,
    /// Optional secondary text (query body, field data type).
    pub detail: Option<String>,
    /// Category for grouping and styling.
    pub category: PaletteCategory,
    /// Keyboard shortcut hint (right-aligned in the UI).
    pub shortcut: Option<String>,
    /// What happens when this item is selected.
    pub action: PaletteAction,
}

/// Item category — determines grouping order and visual styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaletteCategory {
    Action,
    Tab,
    SavedQuery,
    History,
    SchemaField,
}

impl PaletteCategory {
    /// Display name used as a section header.
    pub fn header(self) -> &'static str {
        match self {
            Self::Action => "Actions",
            Self::Tab => "Tabs",
            Self::SavedQuery => "Saved Queries",
            Self::History => "History",
            Self::SchemaField => "Schema Fields",
        }
    }
}

/// Action to execute when a palette item is selected.
#[derive(Debug, Clone)]
pub enum PaletteAction {
    /// Switch to a main tab.
    SwitchTab(MainTab),
    /// Load a query into the editor (clear first, insert, focus editor).
    LoadQuery(String),
    /// Insert text at the current cursor position in the editor.
    InsertAtCursor(String),
    /// Execute a built-in action.
    RunAction(ActionKind),
}

/// Built-in actions available from the command palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    ExecuteQuery,
    SaveQuery,
    ClearEditor,
    ToggleLiveMode,
    ToggleHelp,
    Quit,
    CycleChartView,
    OpenColumnPicker,
    EnterColumnMode,
}

/// A filtered item with its match score and highlighted character positions.
#[derive(Debug, Clone)]
pub struct FilteredItem {
    /// Index into the source `items` vec.
    pub item_index: usize,
    /// Match score from nucleo (higher = better match).
    pub score: u32,
    /// Character positions in the label that matched (for highlight rendering).
    pub match_positions: Vec<u32>,
}

/// Build the static action items (always available regardless of server state).
fn action_items() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            label: "Execute Query".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("Shift+Enter".into()),
            action: PaletteAction::RunAction(ActionKind::ExecuteQuery),
        },
        PaletteItem {
            label: "Save Query".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("Ctrl+S".into()),
            action: PaletteAction::RunAction(ActionKind::SaveQuery),
        },
        PaletteItem {
            label: "Clear Editor".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("Ctrl+L".into()),
            action: PaletteAction::RunAction(ActionKind::ClearEditor),
        },
        PaletteItem {
            label: "Toggle Live Mode".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("F9".into()),
            action: PaletteAction::RunAction(ActionKind::ToggleLiveMode),
        },
        PaletteItem {
            label: "Toggle Help".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("F1".into()),
            action: PaletteAction::RunAction(ActionKind::ToggleHelp),
        },
        PaletteItem {
            label: "Quit".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("Ctrl+Q".into()),
            action: PaletteAction::RunAction(ActionKind::Quit),
        },
        PaletteItem {
            label: "Cycle Chart View".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("v".into()),
            action: PaletteAction::RunAction(ActionKind::CycleChartView),
        },
        PaletteItem {
            label: "Open Column Picker".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("H".into()),
            action: PaletteAction::RunAction(ActionKind::OpenColumnPicker),
        },
        PaletteItem {
            label: "Enter Column Mode".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("c".into()),
            action: PaletteAction::RunAction(ActionKind::EnterColumnMode),
        },
    ]
}

/// Build tab-switching items.
fn tab_items(is_admin: bool) -> Vec<PaletteItem> {
    let mut items = vec![
        PaletteItem {
            label: "Go to Query".into(),
            detail: None,
            category: PaletteCategory::Tab,
            shortcut: Some("Alt+1".into()),
            action: PaletteAction::SwitchTab(MainTab::Query),
        },
        PaletteItem {
            label: "Go to History".into(),
            detail: None,
            category: PaletteCategory::Tab,
            shortcut: Some("Alt+2".into()),
            action: PaletteAction::SwitchTab(MainTab::History),
        },
        PaletteItem {
            label: "Go to Schema".into(),
            detail: None,
            category: PaletteCategory::Tab,
            shortcut: Some("Alt+3".into()),
            action: PaletteAction::SwitchTab(MainTab::Schema),
        },
        PaletteItem {
            label: "Go to Saved".into(),
            detail: None,
            category: PaletteCategory::Tab,
            shortcut: Some("Alt+4".into()),
            action: PaletteAction::SwitchTab(MainTab::Saved),
        },
    ];
    if is_admin {
        items.push(PaletteItem {
            label: "Go to Dashboard".into(),
            detail: None,
            category: PaletteCategory::Tab,
            shortcut: Some("Alt+5".into()),
            action: PaletteAction::SwitchTab(MainTab::Dashboard),
        });
    }
    items
}

/// Build the full palette item catalog from current app state.
///
/// Called once each time the palette is opened (not on every keystroke).
pub fn build_palette_items(
    is_admin: bool,
    saved: Option<&trawl_client::ListSavedResponse>,
    history: Option<&trawl_client::HistoryResponse>,
    schema: Option<&trawl_client::SchemaResponse>,
) -> Vec<PaletteItem> {
    let mut items = Vec::with_capacity(64);

    // Static items.
    items.extend(action_items());
    items.extend(tab_items(is_admin));

    // Saved queries.
    if let Some(saved) = saved {
        for sq in &saved.queries {
            items.push(PaletteItem {
                label: sq.name.clone(),
                detail: Some(sq.query.clone()),
                category: PaletteCategory::SavedQuery,
                shortcut: None,
                action: PaletteAction::LoadQuery(sq.query.clone()),
            });
        }
    }

    // History (deduped by query text, most recent first).
    if let Some(history) = history {
        let mut seen = HashSet::new();
        for entry in &history.entries {
            if seen.insert(entry.query.as_str()) {
                items.push(PaletteItem {
                    label: entry.query.clone(),
                    detail: Some(format!("{}ms, {} rows", entry.duration_ms, entry.row_count)),
                    category: PaletteCategory::History,
                    shortcut: None,
                    action: PaletteAction::LoadQuery(entry.query.clone()),
                });
            }
        }
    }

    // Schema fields.
    if let Some(schema) = schema {
        for col in &schema.columns {
            items.push(PaletteItem {
                label: col.name.clone(),
                detail: Some(col.data_type.clone()),
                category: PaletteCategory::SchemaField,
                shortcut: None,
                action: PaletteAction::InsertAtCursor(col.name.clone()),
            });
        }
    }

    items
}

/// Filter and score items against user input using fuzzy matching.
///
/// Returns all items when `input` is empty (in category order).
/// Returns scored + sorted items when `input` is non-empty.
pub fn refilter(input: &str, items: &[PaletteItem]) -> Vec<FilteredItem> {
    if input.is_empty() {
        // Show all items in category order (preserves insertion order from
        // `build_palette_items` which already groups by category).
        return items
            .iter()
            .enumerate()
            .map(|(i, _)| FilteredItem {
                item_index: i,
                score: 0,
                match_positions: Vec::new(),
            })
            .collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let atom = Atom::new(
        input,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );

    let mut results: Vec<FilteredItem> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            // Try matching against label first.
            let haystack = Utf32String::from(item.label.as_str());
            let mut indices = Vec::new();
            let label_score = atom.indices(haystack.slice(..), &mut matcher, &mut indices);

            // Also try matching against detail text (query body, field type).
            let detail_score = item.detail.as_ref().and_then(|d| {
                let detail_haystack = Utf32String::from(d.as_str());
                atom.score(detail_haystack.slice(..), &mut matcher)
            });

            // Take the best score. If label matched, keep its indices for
            // highlighting. If only detail matched, we highlight nothing in
            // the label (the match is in the secondary text).
            match (label_score, detail_score) {
                (Some(ls), Some(ds)) => {
                    let score = u32::from(ls).max(u32::from(ds));
                    if u32::from(ds) > u32::from(ls) {
                        indices.clear();
                    }
                    indices.sort_unstable();
                    indices.dedup();
                    Some(FilteredItem {
                        item_index: i,
                        score,
                        match_positions: indices,
                    })
                }
                (Some(ls), None) => {
                    indices.sort_unstable();
                    indices.dedup();
                    Some(FilteredItem {
                        item_index: i,
                        score: u32::from(ls),
                        match_positions: indices,
                    })
                }
                (None, Some(ds)) => Some(FilteredItem {
                    item_index: i,
                    score: u32::from(ds),
                    match_positions: Vec::new(),
                }),
                (None, None) => None,
            }
        })
        .collect();

    // Sort by score descending.
    results.sort_by(|a, b| b.score.cmp(&a.score));
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_items_always_present() {
        let items = build_palette_items(false, None, None, None);
        // Actions (9) + tabs (4, no dashboard) = 13
        assert_eq!(items.len(), 13);
        assert!(
            items.iter().all(
                |i| i.category == PaletteCategory::Action || i.category == PaletteCategory::Tab
            )
        );
    }

    #[test]
    fn admin_gets_dashboard_tab() {
        let items = build_palette_items(true, None, None, None);
        assert_eq!(items.len(), 14); // 9 actions + 5 tabs
        assert!(items.iter().any(|i| i.label == "Go to Dashboard"));
    }

    #[test]
    fn empty_filter_returns_all() {
        let items = build_palette_items(false, None, None, None);
        let filtered = refilter("", &items);
        assert_eq!(filtered.len(), items.len());
        // All scores should be 0 (unfiltered).
        assert!(filtered.iter().all(|f| f.score == 0));
    }

    #[test]
    fn fuzzy_filter_matches_label() {
        let items = build_palette_items(false, None, None, None);
        let filtered = refilter("exec", &items);
        assert!(!filtered.is_empty());
        // "Execute Query" should be the top match.
        let top = &items[filtered[0].item_index];
        assert_eq!(top.label, "Execute Query");
        assert!(!filtered[0].match_positions.is_empty());
    }

    #[test]
    fn fuzzy_filter_no_match() {
        let items = build_palette_items(false, None, None, None);
        let filtered = refilter("zzzznothing", &items);
        assert!(filtered.is_empty());
    }

    #[test]
    fn history_dedup() {
        let history = trawl_client::HistoryResponse {
            entries: vec![
                trawl_client::HistoryEntryResponse {
                    id: 1,
                    query: "level=error".into(),
                    executed_at: "2026-01-01T00:00:00Z".into(),
                    duration_ms: 42,
                    row_count: 10,
                    status: trawl_client::QueryStatus::Success,
                },
                trawl_client::HistoryEntryResponse {
                    id: 2,
                    query: "level=error".into(),
                    executed_at: "2026-01-01T00:01:00Z".into(),
                    duration_ms: 50,
                    row_count: 12,
                    status: trawl_client::QueryStatus::Success,
                },
            ],
            total: 2,
        };
        let items = build_palette_items(false, None, Some(&history), None);
        let history_items: Vec<_> = items
            .iter()
            .filter(|i| i.category == PaletteCategory::History)
            .collect();
        // Should be deduped to 1.
        assert_eq!(history_items.len(), 1);
    }

    #[test]
    fn detail_match_finds_saved_query_by_body() {
        let saved = trawl_client::ListSavedResponse {
            queries: vec![trawl_client::SavedQueryResponse {
                id: 1,
                name: "nginx errors".into(),
                query: "service=nginx level=error | stats count() by host".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                schedule: None,
            }],
        };
        let items = build_palette_items(false, Some(&saved), None, None);
        // Search for "stats count" — should match the query body in detail.
        let filtered = refilter("stats count", &items);
        let matched_labels: Vec<_> = filtered
            .iter()
            .map(|f| &items[f.item_index].label)
            .collect();
        assert!(
            matched_labels.contains(&&"nginx errors".to_string()),
            "should find saved query by its DSL body"
        );
    }
}
