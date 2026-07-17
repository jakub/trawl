// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure formatting helpers shared by `<ServiceCard/>` and
//! `<ServiceDrawer/>`. No I/O, no Leptos — just string munging.

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

/// Average non-null coverage across a set of columns, expressed as
/// `"N%"`. Returns `"—"` when there are no columns.
#[must_use]
pub fn format_avg_coverage(columns: &[trawl_api::ServiceColumnStats]) -> String {
    if columns.is_empty() {
        return "—".to_string();
    }
    let sum: f64 = columns
        .iter()
        .map(|c| {
            if c.total_count == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                {
                    (c.total_count - c.null_count) as f64 / c.total_count as f64
                }
            }
        })
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let avg = sum / columns.len() as f64;
    format!("{:.0}%", avg * 100.0)
}

/// Mean non-null coverage across columns as integer permille
/// (`0..=1000`) — an `Ord`-friendly twin of [`format_avg_coverage`]
/// for the services table sort. Returns 0 when there are no columns.
#[must_use]
pub fn avg_cov_permille(columns: &[trawl_api::ServiceColumnStats]) -> u32 {
    if columns.is_empty() {
        return 0;
    }
    let sum: f64 = columns
        .iter()
        .map(|c| {
            if c.total_count == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                {
                    (c.total_count - c.null_count) as f64 / c.total_count as f64
                }
            }
        })
        .sum();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    {
        (sum / columns.len() as f64 * 1000.0).round() as u32
    }
}

/// Format the `earliest_date`/`latest_date` pair from a `ServiceSchema`
/// as `"YYYY-MM-DD → YYYY-MM-DD"`, collapsing to a single date when
/// they match. Returns an empty string when either is missing.
#[must_use]
pub fn date_range(svc: &trawl_api::ServiceSchema) -> String {
    match (svc.earliest_date.as_deref(), svc.latest_date.as_deref()) {
        (Some(a), Some(b)) if a == b => a.to_string(),
        (Some(a), Some(b)) => format!("{a} → {b}"),
        _ => String::new(),
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
    fn format_bytes_compacts() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(4_200), "4 KB");
        assert_eq!(format_bytes(284_000_000), "284 MB");
        assert_eq!(format_bytes(1_200_000_000), "1.2 GB");
    }

    #[test]
    fn cov_pct_handles_zero_total() {
        assert_eq!(cov_pct(0, 0), 0);
        assert_eq!(cov_pct(0, 100), 100);
        assert_eq!(cov_pct(50, 100), 50);
        assert_eq!(cov_pct(99, 100), 1);
    }

    #[test]
    fn avg_cov_permille_averages_columns() {
        let col = |null_count, total_count| trawl_api::ServiceColumnStats {
            name: String::new(),
            data_type: String::new(),
            null_count,
            total_count,
            min_value: None,
            max_value: None,
            compressed_bytes: 0,
        };
        assert_eq!(avg_cov_permille(&[]), 0);
        assert_eq!(avg_cov_permille(&[col(0, 100)]), 1000);
        assert_eq!(avg_cov_permille(&[col(0, 100), col(50, 100)]), 750);
        // total_count == 0 counts as zero coverage, not a div-by-zero
        assert_eq!(avg_cov_permille(&[col(0, 0), col(0, 100)]), 500);
    }

    #[test]
    fn type_pill_maps_known_types() {
        assert_eq!(type_pill("VARCHAR").0, "tp-keyword");
        assert_eq!(type_pill("BIGINT").0, "tp-bigint");
        assert_eq!(type_pill("TIMESTAMP WITH TIME ZONE").0, "tp-timestamp");
        assert_eq!(type_pill("BOOLEAN").0, "tp-bool");
    }
}
