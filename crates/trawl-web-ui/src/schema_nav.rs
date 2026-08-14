// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Schema page's URL and history decisions, as pure data-in/data-out
//! functions so the parts that are easy to get subtly wrong are testable
//! natively, off the browser.
//!
//! Two of the three exist because a query parameter is attacker-reachable
//! text:
//!
//! - [`sanitize_tab`] parses `?stab=` through a CLOSED vocabulary at READ
//!   time and hands back a `&'static str` from that table. The page
//!   re-concatenates the active tab into the field drill-in URL, so a raw
//!   `stab=fields%26field%3Dx` would smuggle a SECOND `field=` parameter
//!   into it — and `ParamsMap::get` takes the last. Nothing but a
//!   canonical spelling can ever be re-emitted.
//! - [`degraded_badge_id`] builds a focus target's element id from a row
//!   INDEX, never from the field name: names are client-chosen text and
//!   an id built from one would be interpolated straight into a DOM
//!   lookup.
//!
//! The third, [`back_nav`], is the history-shape decision: the case file's
//! back affordance may only POP an entry this app itself pushed.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// The service drawer's tab ids, in strip order — the same three
/// spellings `ServiceDrawer`'s `TabItem`s carry, and the only ones the
/// page will put back into a URL.
pub const SCHEMA_TABS: [&str; 3] = ["overview", "fields", "tail"];

/// The tab an absent, empty or unrecognised `?stab=` resolves to.
pub const DEFAULT_SCHEMA_TAB: &str = SCHEMA_TABS[0];

/// Resolve a raw `?stab=` value to one of [`SCHEMA_TABS`].
///
/// Matching is ASCII-case-insensitive and the CANONICAL spelling is what
/// comes back, so a `?stab=Fields` deep link still lands on the fields
/// tab while the value the page re-emits stays inside the vocabulary.
/// Anything else — empty, unknown, or a smuggled `&field=…` — is the
/// default tab.
#[must_use]
pub fn sanitize_tab(raw: Option<&str>) -> &'static str {
    let Some(raw) = raw else {
        return DEFAULT_SCHEMA_TAB;
    };
    SCHEMA_TABS
        .into_iter()
        .find(|tab| tab.eq_ignore_ascii_case(raw))
        .unwrap_or(DEFAULT_SCHEMA_TAB)
}

/// What the field case file's back affordance must do with the history
/// entry it is standing on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackNav {
    /// This app pushed the entry, so going back POPS it: the entry count
    /// is where it was before the drill-in, and one browser Back leaves
    /// the schema page rather than landing on a duplicate of it.
    Pop,
    /// We did not push it — a `?field=` deep link, or a forward
    /// navigation onto an entry from an earlier session of the page — so
    /// there is nothing of ours to pop and the entry is REPLACED with the
    /// return URL instead.
    Replace,
}

/// Decide [`BackNav`] for the case file currently showing `shown_field`.
///
/// `pushed_field` is the field whose entry this page pushed, if any.
/// Comparison is exact: both strings are the same spelling round-tripped
/// through our own URL encoder, and a case-insensitive match here would
/// pop an entry we did not push whenever a deep link differed only in
/// case from an earlier drill-in.
#[must_use]
pub fn back_nav(shown_field: &str, pushed_field: Option<&str>) -> BackNav {
    if pushed_field == Some(shown_field) {
        BackNav::Pop
    } else {
        BackNav::Replace
    }
}

/// Element id of the degraded badge for the field at `index` in the
/// service's column list — the focus-return target across the
/// case-file/service-drawer swap.
///
/// The index, not the name: it is the drawer's own position, so it is
/// stable across the fields table's client-side sorting, and it cannot
/// carry client-chosen text into an id.
#[must_use]
pub fn degraded_badge_id(index: usize) -> String {
    format!("sf-deg-{index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stab_resolves_only_to_the_closed_vocabulary() {
        assert_eq!(sanitize_tab(Some("overview")), "overview");
        assert_eq!(sanitize_tab(Some("fields")), "fields");
        assert_eq!(sanitize_tab(Some("tail")), "tail");
        // Absent / empty / unknown all fall back, never through.
        assert_eq!(sanitize_tab(None), DEFAULT_SCHEMA_TAB);
        assert_eq!(sanitize_tab(Some("")), DEFAULT_SCHEMA_TAB);
        assert_eq!(sanitize_tab(Some("   ")), DEFAULT_SCHEMA_TAB);
        assert_eq!(sanitize_tab(Some("settings")), DEFAULT_SCHEMA_TAB);
        // Case is forgiven on the way IN, canonicalised on the way OUT.
        assert_eq!(sanitize_tab(Some("Fields")), "fields");
        assert_eq!(sanitize_tab(Some("TAIL")), "tail");
    }

    #[test]
    fn stab_cannot_smuggle_a_second_parameter() {
        // The finding: the drill-in URL is built as
        // `?field=<f>&svc=<s>&stab=<tab>`, so a `stab` carrying its own
        // separators would append a second `field=` — and the last one
        // wins in `ParamsMap::get`.
        for hostile in [
            "fields&field=attacker",
            "fields%26field%3Dattacker",
            "fields&svc=other",
            "overview#fragment",
            "fields ",
            "../../etc/passwd",
            "<script>alert(1)</script>",
        ] {
            let resolved = sanitize_tab(Some(hostile));
            assert_eq!(
                resolved, DEFAULT_SCHEMA_TAB,
                "a non-vocabulary stab must not survive: {hostile}"
            );
            assert!(
                SCHEMA_TABS.contains(&resolved),
                "only a vocabulary spelling is ever re-emitted"
            );
        }
    }

    #[test]
    fn back_pops_only_the_entry_this_page_pushed() {
        // Drill-in: we pushed `duration`, so back pops it and the entry
        // count returns to what it was — no duplicate service entry.
        assert_eq!(back_nav("duration", Some("duration")), BackNav::Pop);
        // Deep link: nothing of ours on the stack to pop.
        assert_eq!(back_nav("duration", None), BackNav::Replace);
        // A different field's entry (drilled to A, forward-navigated to
        // B's older entry) is not ours to pop either.
        assert_eq!(back_nav("status", Some("duration")), BackNav::Replace);
        // Exact match only.
        assert_eq!(back_nav("Duration", Some("duration")), BackNav::Replace);
    }

    #[test]
    fn badge_ids_are_index_derived_and_distinct() {
        assert_eq!(degraded_badge_id(0), "sf-deg-0");
        assert_ne!(degraded_badge_id(0), degraded_badge_id(1));
        // Whatever the field is called, the id is [a-z0-9-].
        assert!(
            degraded_badge_id(12)
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        );
    }
}
