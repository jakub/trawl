// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Schema tree view key handling — expand/collapse, navigation, field insertion.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::super::App;
use super::super::state::{Focus, MainTab};

impl App {
    /// Handle key events for the schema tree view.
    pub(crate) fn handle_schema_tree_key(&mut self, key: event::KeyEvent) {
        if self.panel.schema.is_none() {
            return;
        }

        // Compute visible node count for bounds (borrows self immutably).
        let node_count = self.visible_tree_node_count();

        // Dispatch Enter/Right/Left to their own methods (which re-borrow self).
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Enter) => {
                self.handle_tree_enter();
                return;
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.handle_tree_expand();
                return;
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.handle_tree_collapse();
                return;
            }
            _ => {}
        }

        // Navigation keys that only update cursor position.
        let schema = self.panel.schema.as_mut().unwrap();
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Up) => {
                schema.selected = schema.selected.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) if node_count > 0 => {
                schema.selected = (schema.selected + 1).min(node_count - 1);
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                schema.selected = schema.selected.saturating_sub(10);
            }
            (KeyModifiers::NONE, KeyCode::PageDown) if node_count > 0 => {
                schema.selected = (schema.selected + 10).min(node_count - 1);
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                schema.selected = 0;
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                schema.selected = node_count.saturating_sub(1);
            }
            _ => {}
        }
    }

    /// Count visible nodes in the flattened schema tree.
    ///
    /// Layout: common header + common fields, then per-service rows
    /// (each service + its unique fields when expanded).
    pub(crate) fn visible_tree_node_count(&self) -> usize {
        let Some(schema) = self.panel.schema.as_ref() else {
            return 0;
        };
        let filter = schema.filter.to_lowercase();
        let common_names: std::collections::HashSet<&str> = schema
            .common_fields
            .iter()
            .map(|f| f.name.as_str())
            .collect();

        let mut count = 0;

        // Common fields section.
        if !schema.common_fields.is_empty() {
            let any_common_match = filter.is_empty()
                || schema
                    .common_fields
                    .iter()
                    .any(|f| f.name.to_lowercase().contains(&filter));
            if any_common_match {
                count += 1; // header
                count += schema
                    .common_fields
                    .iter()
                    .filter(|f| filter.is_empty() || f.name.to_lowercase().contains(&filter))
                    .count();
            }
        }

        // Per-service rows.
        for svc in &schema.services {
            let unique_fields: Vec<_> = svc
                .columns
                .iter()
                .filter(|c| !common_names.contains(c.name.as_str()))
                .collect();

            if !filter.is_empty() {
                let svc_matches = svc.name.to_lowercase().contains(&filter);
                let fields_match = unique_fields
                    .iter()
                    .any(|c| c.name.to_lowercase().contains(&filter));
                if !svc_matches && !fields_match {
                    continue;
                }
            }

            count += 1; // service node
            if schema.expanded.contains(&svc.name) {
                count += unique_fields.len();
            }
        }
        count
    }

    /// Resolve the currently selected tree node into an action-relevant enum.
    ///
    /// Returns `(kind, name)` where kind is `common_header`, `common_field`,
    /// `service`, or `service_field`, plus the relevant name string.
    fn resolve_selected_node(&self) -> Option<(&str, String, Option<String>)> {
        let schema = self.panel.schema.as_ref()?;
        let filter = schema.filter.to_lowercase();
        let common_names: std::collections::HashSet<&str> = schema
            .common_fields
            .iter()
            .map(|f| f.name.as_str())
            .collect();

        let target = schema.selected;
        let mut idx = 0;

        // Common fields section.
        if !schema.common_fields.is_empty() {
            let visible_common: Vec<_> = schema
                .common_fields
                .iter()
                .filter(|f| filter.is_empty() || f.name.to_lowercase().contains(&filter))
                .collect();
            let any_common_match = !visible_common.is_empty() || filter.is_empty();

            if any_common_match && !visible_common.is_empty() {
                // Header row.
                if idx == target {
                    return Some(("common_header", String::new(), None));
                }
                idx += 1;
                for f in &visible_common {
                    if idx == target {
                        return Some(("common_field", f.name.clone(), None));
                    }
                    idx += 1;
                }
            } else if any_common_match {
                // Header row only (empty visible due to filter edge case).
                if idx == target {
                    return Some(("common_header", String::new(), None));
                }
                idx += 1;
            }
        }

        // Per-service rows.
        for svc in &schema.services {
            let unique_fields: Vec<_> = svc
                .columns
                .iter()
                .filter(|c| !common_names.contains(c.name.as_str()))
                .collect();

            if !filter.is_empty() {
                let svc_matches = svc.name.to_lowercase().contains(&filter);
                let fields_match = unique_fields
                    .iter()
                    .any(|c| c.name.to_lowercase().contains(&filter));
                if !svc_matches && !fields_match {
                    continue;
                }
            }

            if idx == target {
                return Some(("service", svc.name.clone(), None));
            }
            idx += 1;

            if schema.expanded.contains(&svc.name) {
                for col in &unique_fields {
                    if idx == target {
                        return Some(("service_field", col.name.clone(), Some(svc.name.clone())));
                    }
                    idx += 1;
                }
            }
        }
        None
    }

    /// Find the flat index of the parent service node for the currently selected node.
    #[allow(unused_assignments)] // last_service_idx initial value is a fallback, always overwritten in loop
    fn find_parent_service_index(&self) -> Option<usize> {
        let schema = self.panel.schema.as_ref()?;
        let filter = schema.filter.to_lowercase();
        let common_names: std::collections::HashSet<&str> = schema
            .common_fields
            .iter()
            .map(|f| f.name.as_str())
            .collect();

        let target = schema.selected;
        let mut idx = 0;
        let mut last_service_idx = 0;

        // Skip common fields section.
        if !schema.common_fields.is_empty() {
            let any_common_match = filter.is_empty()
                || schema
                    .common_fields
                    .iter()
                    .any(|f| f.name.to_lowercase().contains(&filter));
            if any_common_match {
                idx += 1; // header
                idx += schema
                    .common_fields
                    .iter()
                    .filter(|f| filter.is_empty() || f.name.to_lowercase().contains(&filter))
                    .count();
            }
        }

        for svc in &schema.services {
            let unique_fields: Vec<_> = svc
                .columns
                .iter()
                .filter(|c| !common_names.contains(c.name.as_str()))
                .collect();

            if !filter.is_empty() {
                let svc_matches = svc.name.to_lowercase().contains(&filter);
                let fields_match = unique_fields
                    .iter()
                    .any(|c| c.name.to_lowercase().contains(&filter));
                if !svc_matches && !fields_match {
                    continue;
                }
            }

            last_service_idx = idx;
            if idx == target {
                return Some(idx);
            }
            idx += 1;

            if schema.expanded.contains(&svc.name) {
                for _ in &unique_fields {
                    if idx == target {
                        return Some(last_service_idx);
                    }
                    idx += 1;
                }
            }
        }
        None
    }

    /// Handle Enter key on a tree node.
    fn handle_tree_enter(&mut self) {
        let Some((kind, name, _svc)) = self.resolve_selected_node() else {
            return;
        };
        match kind {
            "service" => {
                let schema = self.panel.schema.as_mut().unwrap();
                if schema.expanded.contains(&name) {
                    schema.expanded.remove(&name);
                } else {
                    schema.expanded.insert(name);
                }
            }
            "common_field" | "service_field" => {
                let Some(name) = trawl_core::parser::suggest::quote_dsl_field(&name) else {
                    return;
                };
                self.tab.editor.insert_text(&name);
                self.switch_to_main_tab(MainTab::Query);
                self.focus = Focus::Editor;
            }
            _ => {}
        }
    }

    /// Handle Right arrow on a tree node (expand service).
    fn handle_tree_expand(&mut self) {
        let Some((kind, name, _)) = self.resolve_selected_node() else {
            return;
        };
        if kind == "service" {
            let schema = self.panel.schema.as_mut().unwrap();
            schema.expanded.insert(name);
        }
    }

    /// Handle Left arrow on a tree node (collapse or jump to parent).
    fn handle_tree_collapse(&mut self) {
        let Some((kind, name, _)) = self.resolve_selected_node() else {
            return;
        };
        match kind {
            "service" => {
                let schema = self.panel.schema.as_mut().unwrap();
                schema.expanded.remove(&name);
            }
            "service_field" => {
                if let Some(parent_idx) = self.find_parent_service_index()
                    && let Some(schema) = self.panel.schema.as_mut()
                {
                    schema.selected = parent_idx;
                }
            }
            _ => {}
        }
    }
}
