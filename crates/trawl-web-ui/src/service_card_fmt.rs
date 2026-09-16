// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure formatting helpers shared by `<ServiceCard/>` and
//! `<ServiceDrawer/>`. No I/O, no Leptos — just string munging.
//!
//! Ungated and top-level for the `tone_vocab` / `facets` reason: inside
//! the wasm32-gated `components` module its `mod tests` would never run
//! under `cargo nextest`. `components::service_card_fmt` still resolves
//! because the module is re-exported there, so every call site keeps its
//! path.
//! `today_yesterday_utc` is the one wasm32-only member (it reads the
//! browser clock) and carries its own gate.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// Compact human count: `"1.2M"`, `"32k"`, `"480"`.
#[must_use]
pub fn format_count(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // cosmetic label
    let f = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.1}B", f / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", f / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.0}k", f / 1_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", f / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Every digit, thousands-grouped: `"1,204,913"`, `"7"`, `"0"`.
///
/// The counterpart to [`format_count`] for a number an operator accepts
/// responsibility for: the values a repin would null, the rows carrying
/// the field, what a finished job actually rewrote. "1.2k values become
/// NULL" is not a fact anyone can act on, and grouping is what makes the
/// exact digits readable at a glance.
#[must_use]
pub fn format_exact(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.char_indices() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Compact bytes: `"1.2 GB"`, `"284 MB"`, `"18 KB"`, `"42 B"`.
#[must_use]
pub fn format_bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.1} GB", f / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.0} MB", f / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0} KB", f / 1_000.0)
    } else {
        format!("{n} B")
    }
}

/// Compact uptime: `"42s"`, `"12m"`, `"5h 12m"`, `"3d 4h"`. Zero
/// remainders are omitted (`"5h"`, not `"5h 0m"`).
#[must_use]
pub fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        return if rem_mins == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {rem_mins}m")
        };
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d {rem_hours}h")
    }
}

/// A service is "healthy" for display purposes when its most recent
/// `daily_event_counts` entry has a non-zero count AND matches either
/// `today` or `yesterday` (both formatted as `YYYY-MM-DD`). Pure —
/// the caller passes in the wall-clock dates.
#[must_use]
pub fn is_healthy(svc: &trawl_api::ServiceSchema, today: &str, yesterday: &str) -> bool {
    let Some(last) = svc.daily_event_counts.last() else {
        return false;
    };
    if last.count == 0 {
        return false;
    }
    last.date == today || last.date == yesterday
}

/// Today / yesterday as `YYYY-MM-DD UTC`, read from the browser's
/// wall-clock. Call once per render (e.g. at the top of `SchemaPage`)
/// and pass the pair into `is_healthy`. WASM-only.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn today_yesterday_utc() -> (String, String) {
    let now = js_sys::Date::new_0();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (y, m, d) = (
        now.get_utc_full_year(),
        now.get_utc_month() + 1,
        now.get_utc_date(),
    );
    let today = format!("{y:04}-{m:02}-{d:02}");
    let yday_ms = now.get_time() - 86_400_000.0;
    let yday = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(yday_ms));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (yy, ym, yd) = (
        yday.get_utc_full_year(),
        yday.get_utc_month() + 1,
        yday.get_utc_date(),
    );
    let yesterday = format!("{yy:04}-{ym:02}-{yd:02}");
    (today, yesterday)
}

/// How many of this service's fields the catalog currently calls
/// degraded.
///
/// Reads the server-stamped list and nothing else. The count is not
/// client-derivable: a service is on the list only when
/// `field_conflict_stats` holds evidence that this service conflicted on
/// the field, so intersecting `columns` with an install-wide degraded set
/// would badge every service that merely carries the column.
#[must_use]
pub fn degraded_count(svc: &trawl_api::ServiceSchema) -> usize {
    svc.degraded_fields.len()
}

/// Whether this service's named column carries a degraded pin — the
/// single membership test behind the fields-tab badge, for the reason in
/// [`degraded_count`].
#[must_use]
pub fn is_degraded_column(svc: &trawl_api::ServiceSchema, field: &str) -> bool {
    svc.degraded_fields.iter().any(|f| f == field)
}

/// Non-null percentage as a rounded integer in `0..=100`.
#[must_use]
pub fn cov_pct(null: u64, total: u64) -> u32 {
    if total == 0 {
        return 0;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    {
        ((total - null) as f64 / total as f64 * 100.0).round() as u32
    }
}

/// Map a `DuckDB` type string onto a `.tp-*` pill class + short label
/// used on field pills across the Schema page.
#[must_use]
pub fn type_pill(data_type: &str) -> (&'static str, String) {
    let t = data_type.to_ascii_uppercase();
    if t == "VARCHAR" || t == "STRING" || t == "CHAR" {
        return ("tp-keyword", "STR".into());
    }
    if t == "BIGINT" || t == "HUGEINT" || t == "UBIGINT" {
        return ("tp-bigint", "BIGINT".into());
    }
    if t == "INTEGER" || t == "INT" || t == "SMALLINT" || t == "TINYINT" {
        return ("tp-integer", "INT".into());
    }
    if t == "DOUBLE" || t == "FLOAT" || t == "REAL" || t == "DECIMAL" {
        return ("tp-float", "NUM".into());
    }
    if t == "BOOLEAN" || t == "BOOL" {
        return ("tp-bool", "BOOL".into());
    }
    if t.starts_with("TIMESTAMP") || t == "DATE" || t == "TIME" {
        return ("tp-timestamp", "TS".into());
    }
    if t.contains("TEXT") {
        return ("tp-text", "TXT".into());
    }
    ("tp-keyword", t)
}

/// Coarser type-bucket mapping used by the donut: returns
/// `(display_label, css_color, pill_class)`. The three string slices
/// are `'static` so the donut can reference them in a SVG stroke
/// without owning a `String`.
#[must_use]
pub fn type_bucket(data_type: &str) -> (&'static str, &'static str, &'static str) {
    let t = data_type.to_ascii_uppercase();
    if t == "VARCHAR" || t == "STRING" || t == "CHAR" {
        return ("STR", "var(--blue)", "tp-keyword");
    }
    if t == "BIGINT"
        || t == "HUGEINT"
        || t == "UBIGINT"
        || t == "INTEGER"
        || t == "INT"
        || t == "SMALLINT"
        || t == "TINYINT"
    {
        return ("INT", "var(--accent-2)", "tp-bigint");
    }
    if t == "DOUBLE" || t == "FLOAT" || t == "REAL" || t == "DECIMAL" {
        return ("NUM", "var(--accent-soft)", "tp-float");
    }
    if t == "BOOLEAN" || t == "BOOL" {
        return ("BOOL", "var(--teal)", "tp-bool");
    }
    if t.starts_with("TIMESTAMP") || t == "DATE" || t == "TIME" {
        return ("TS", "var(--ink-3)", "tp-timestamp");
    }
    if t.contains("TEXT") {
        return ("TXT", "var(--green)", "tp-text");
    }
    ("OTHER", "var(--panel-3)", "tp-keyword")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_count_compacts() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(42), "42");
        assert_eq!(format_count(1_500), "1.5k");
        assert_eq!(format_count(12_345), "12k");
        assert_eq!(format_count(1_800_000), "1.8M");
        assert_eq!(format_count(2_500_000_000), "2.5B");
    }

    #[test]
    fn format_exact_groups_every_digit() {
        assert_eq!(format_exact(0), "0");
        assert_eq!(format_exact(7), "7");
        assert_eq!(format_exact(999), "999");
        // The boundary the grouping is for: nothing is rounded away.
        assert_eq!(format_exact(1_000), "1,000");
        assert_eq!(format_exact(1_204_913), "1,204,913");
        assert_eq!(format_exact(u64::MAX), "18,446,744,073,709,551,615");
        // Never lossy, unlike `format_count`.
        assert_eq!(format_count(1_204_913), "1.2M");
        assert_ne!(format_exact(1_204_913), format_count(1_204_913));
    }

    #[test]
    fn format_bytes_compacts() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(4_200), "4 KB");
        assert_eq!(format_bytes(284_000_000), "284 MB");
        assert_eq!(format_bytes(1_200_000_000), "1.2 GB");
    }

    #[test]
    fn format_uptime_picks_two_largest_units() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(42), "42s");
        assert_eq!(format_uptime(60), "1m");
        assert_eq!(format_uptime(12 * 60 + 30), "12m");
        assert_eq!(format_uptime(5 * 3600 + 12 * 60), "5h 12m");
        assert_eq!(format_uptime(5 * 3600), "5h");
        assert_eq!(format_uptime(3 * 86_400 + 4 * 3600), "3d 4h");
        assert_eq!(format_uptime(3 * 86_400), "3d");
    }

    #[test]
    fn cov_pct_handles_zero_total() {
        assert_eq!(cov_pct(0, 0), 0);
        assert_eq!(cov_pct(0, 100), 100);
        assert_eq!(cov_pct(50, 100), 50);
        assert_eq!(cov_pct(99, 100), 1);
    }

    #[test]
    fn degraded_reads_only_the_server_stamped_list() {
        let column = |name: &str| trawl_api::ServiceColumnStats {
            name: name.to_string(),
            data_type: "VARCHAR".to_string(),
            null_count: 0,
            total_count: 10,
            min_value: None,
            max_value: None,
            compressed_bytes: 0,
        };
        let svc = |degraded: &[&str]| trawl_api::ServiceSchema {
            name: "nginx".to_string(),
            columns: vec![column("duration"), column("status")],
            earliest_date: None,
            latest_date: None,
            file_count: 0,
            total_bytes: 0,
            total_events: 0,
            daily_event_counts: Vec::new(),
            degraded_fields: degraded.iter().map(|s| (*s).to_string()).collect(),
        };

        assert_eq!(degraded_count(&svc(&[])), 0);
        assert_eq!(degraded_count(&svc(&["duration"])), 1);
        assert_eq!(degraded_count(&svc(&["duration", "status"])), 2);

        assert!(is_degraded_column(&svc(&["duration"]), "duration"));
        // The service carries the column and some other service degraded
        // it: not badged here.
        assert!(!is_degraded_column(&svc(&[]), "duration"));
        assert!(!is_degraded_column(&svc(&["duration"]), "status"));
        // A degraded field the service no longer carries is filtered
        // server-side; nothing here re-derives membership from columns.
        assert!(!is_degraded_column(&svc(&["duration"]), "host"));
    }

    #[test]
    fn type_pill_maps_known_types() {
        assert_eq!(type_pill("VARCHAR").0, "tp-keyword");
        assert_eq!(type_pill("BIGINT").0, "tp-bigint");
        assert_eq!(type_pill("TIMESTAMP WITH TIME ZONE").0, "tp-timestamp");
        assert_eq!(type_pill("BOOLEAN").0, "tp-bool");
    }
}
