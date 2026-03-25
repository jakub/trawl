// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Command palette types and fuzzy matching logic.
//!
//! Pure logic module — no I/O, no ratatui dependencies.

use std::collections::HashSet;

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32String};

use super::state::{MainTab, SchemaBrowser};

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
    Service,
}

impl PaletteCategory {
    /// Display name used as a section header.
    pub fn header(self) -> &'static str {
        match self {
            Self::Action => "Actions",
            Self::Tab => "Tabs",
            Self::SavedQuery => "Saved Queries",
            Self::History => "History",
            Self::Service => "Services",
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
    /// Drill into a service's columns (appends "service." to palette input).
    DrillService(String),
}

/// Built-in actions available from the command palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    ExecuteQuery,
    SaveQuery,
    FormatQuery,
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
            label: "Format Query".into(),
            detail: None,
            category: PaletteCategory::Action,
            shortcut: Some("Ctrl+F".into()),
            action: PaletteAction::RunAction(ActionKind::FormatQuery),
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
/// Also called to rebuild items when the palette input changes between
/// "all categories" mode and "service columns" mode.
pub fn build_palette_items(
    is_admin: bool,
    saved: Option<&trawl_client::ListSavedResponse>,
    history: Option<&trawl_client::HistoryResponse>,
    schema: Option<&SchemaBrowser>,
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

    // History (deduped by query text, most recent 5).
    if let Some(history) = history {
        let mut seen = HashSet::new();
        let mut count = 0;
        for entry in &history.entries {
            if count >= 5 {
                break;
            }
            if seen.insert(entry.query.as_str()) {
                items.push(PaletteItem {
                    label: entry.query.clone(),
                    detail: Some(format!("{}ms, {} rows", entry.duration_ms, entry.row_count)),
                    category: PaletteCategory::History,
                    shortcut: None,
                    action: PaletteAction::LoadQuery(entry.query.clone()),
                });
                count += 1;
            }
        }
    }

    // Services (one item per service with column count).
    if let Some(schema) = schema {
        for svc in &schema.services {
            items.push(PaletteItem {
                label: svc.name.clone(),
                detail: Some(format!("{} fields", svc.columns.len())),
                category: PaletteCategory::Service,
                shortcut: None,
                action: PaletteAction::DrillService(svc.name.clone()),
            });
        }
    }

    items
}

/// Build palette items for a specific service's columns.
///
/// Called when the palette input contains a dot (e.g. "nginx.").
pub fn build_service_column_items(schema: &SchemaBrowser, service_name: &str) -> Vec<PaletteItem> {
    let Some(svc) = schema.services.iter().find(|s| s.name == service_name) else {
        return Vec::new();
    };
    svc.columns
        .iter()
        .map(|col| PaletteItem {
            label: col.name.clone(),
            detail: Some(col.data_type.clone()),
            category: PaletteCategory::Service,
            shortcut: None,
            action: PaletteAction::InsertAtCursor(col.name.clone()),
        })
        .collect()
}

/// Compute ghost text completion suffix for the current palette input.
///
/// Returns the remaining characters that would complete the top fuzzy match,
/// but only when the match is a case-insensitive prefix match (not a fuzzy
/// mid-string hit).
pub fn compute_ghost_text(
    input: &str,
    items: &[PaletteItem],
    filtered: &[FilteredItem],
) -> Option<String> {
    if input.is_empty() || filtered.is_empty() {
        return None;
    }

    // Determine what we're completing: after-dot text matches column names,
    // pre-dot text matches service names.
    let (prefix, target_category) = if let Some(dot_pos) = input.find('.') {
        // Column mode: match text after the dot against column labels.
        let after_dot = &input[dot_pos + 1..];
        if after_dot.is_empty() {
            return None;
        }
        (after_dot, None) // category is Service for columns too, match any
    } else {
        // Service mode: only ghost-complete service names.
        (input, Some(PaletteCategory::Service))
    };

    let prefix_lower = prefix.to_lowercase();

    // Find the top filtered item that is a prefix match.
    for entry in filtered {
        let item = &items[entry.item_index];
        if target_category.is_some_and(|cat| item.category != cat) {
            continue;
        }
        let label_lower = item.label.to_lowercase();
        if label_lower.starts_with(&prefix_lower) && label_lower.len() > prefix_lower.len() {
            // Return the suffix preserving the original label's casing.
            return Some(item.label[prefix.len()..].to_string());
        }
    }

    None
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
        // Actions (10) + tabs (4, no dashboard) = 14
        assert_eq!(items.len(), 14);
        assert!(
            items.iter().all(
                |i| i.category == PaletteCategory::Action || i.category == PaletteCategory::Tab
            )
        );
    }

    #[test]
    fn admin_gets_dashboard_tab() {
        let items = build_palette_items(true, None, None, None);
        assert_eq!(items.len(), 15); // 10 actions + 5 tabs
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

    fn test_col(name: &str, data_type: &str) -> trawl_api::ServiceColumnStats {
        trawl_api::ServiceColumnStats {
            name: name.into(),
            data_type: data_type.into(),
            null_count: 0,
            total_count: 100,
            min_value: None,
            max_value: None,
            compressed_bytes: 0,
        }
    }

    fn test_schema() -> SchemaBrowser {
        SchemaBrowser::new(vec![
            trawl_api::ServiceSchema {
                name: "nginx".into(),
                columns: vec![
                    test_col("timestamp", "TIMESTAMP"),
                    test_col("host", "VARCHAR"),
                    test_col("status", "BIGINT"),
                ],
                earliest_date: None,
                latest_date: None,
                file_count: 1,
                total_bytes: 1024,
                total_events: 100,
                daily_event_counts: Vec::new(),
            },
            trawl_api::ServiceSchema {
                name: "trawld".into(),
                columns: vec![test_col("total_queries", "BIGINT")],
                earliest_date: None,
                latest_date: None,
                file_count: 1,
                total_bytes: 512,
                total_events: 50,
                daily_event_counts: Vec::new(),
            },
        ])
    }

    #[test]
    fn services_appear_in_palette() {
        let schema = test_schema();
        let items = build_palette_items(false, None, None, Some(&schema));
        let service_items: Vec<_> = items
            .iter()
            .filter(|i| i.category == PaletteCategory::Service)
            .collect();
        assert_eq!(service_items.len(), 2);
        assert_eq!(service_items[0].label, "nginx");
        assert_eq!(service_items[0].detail.as_deref(), Some("3 fields"));
        assert_eq!(service_items[1].label, "trawld");
        assert_eq!(service_items[1].detail.as_deref(), Some("1 fields"));
    }

    #[test]
    fn service_drill_builds_column_items() {
        let schema = test_schema();
        let items = build_service_column_items(&schema, "nginx");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].label, "timestamp");
        assert_eq!(items[0].detail.as_deref(), Some("TIMESTAMP"));
        assert!(matches!(items[0].action, PaletteAction::InsertAtCursor(_)));
    }

    #[test]
    fn service_drill_unknown_service_returns_empty() {
        let schema = test_schema();
        let items = build_service_column_items(&schema, "nonexistent");
        assert!(items.is_empty());
    }

    #[test]
    fn ghost_text_completes_service_name() {
        let schema = test_schema();
        let items = build_palette_items(false, None, None, Some(&schema));
        let filtered = refilter("ngi", &items);
        let ghost = compute_ghost_text("ngi", &items, &filtered);
        assert_eq!(ghost.as_deref(), Some("nx"));
    }

    #[test]
    fn ghost_text_completes_column_name() {
        let schema = test_schema();
        let items = build_service_column_items(&schema, "nginx");
        let filtered = refilter("sta", &items);
        let ghost = compute_ghost_text("nginx.sta", &items, &filtered);
        assert_eq!(ghost.as_deref(), Some("tus"));
    }

    #[test]
    fn ghost_text_empty_input_returns_none() {
        let items = build_palette_items(false, None, None, None);
        let filtered = refilter("", &items);
        let ghost = compute_ghost_text("", &items, &filtered);
        assert!(ghost.is_none());
    }

    #[test]
    fn ghost_text_no_match_returns_none() {
        let items = build_palette_items(false, None, None, None);
        let filtered = refilter("zzzznothing", &items);
        let ghost = compute_ghost_text("zzzznothing", &items, &filtered);
        assert!(ghost.is_none());
    }

    #[test]
    fn ghost_text_exact_match_returns_none() {
        let schema = test_schema();
        let items = build_palette_items(false, None, None, Some(&schema));
        let filtered = refilter("nginx", &items);
        let ghost = compute_ghost_text("nginx", &items, &filtered);
        // "nginx" fully matches "nginx" — no suffix to complete.
        assert!(ghost.is_none());
    }
}
