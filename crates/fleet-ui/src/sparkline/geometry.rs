// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure sparkline point/scale math. No `leptos`, no `web_sys` — builds
//! on every target so the degenerate-input and scaling behaviour is
//! exercised by native unit tests. The SVG assembly stays in the
//! wasm-only [`component`](super::component).

/// The two SVG point lists a sparkline renders: `line` for the
/// `<polyline>` stroke, `fill` for the closed `<polygon>` area under it.
#[derive(Debug, Clone, PartialEq)]
pub struct SparkPath {
    pub line: String,
    pub fill: String,
}

/// Compute the sparkline geometry for `data` inside a `w`×`h` viewBox.
///
/// Returns `None` for degenerate inputs (fewer than two samples) — the
/// component renders an empty `<svg>` so the parent slot collapses
/// instead of eating layout space. Values auto-scale to the max sample
/// (min 1, so an all-zero series draws a flat line along the bottom):
/// `y = h - (v / max) * (h - 2) - 1`, keeping a 1px margin top+bottom.
#[must_use]
pub fn spark_path(data: &[u64], w: f64, h: f64) -> Option<SparkPath> {
    if data.len() < 2 {
        return None;
    }

    #[allow(clippy::cast_precision_loss)] // sparkline scale, not a measurement
    let max = data.iter().copied().max().unwrap_or(0).max(1) as f64;
    #[allow(clippy::cast_precision_loss)] // see above; values bounded by ingest
    let last_idx = (data.len() - 1) as f64;

    let pts: Vec<String> = data
        .iter()
        .enumerate()
        .map(|(i, v)| {
            #[allow(clippy::cast_precision_loss)]
            let x = (i as f64 / last_idx) * w;
            #[allow(clippy::cast_precision_loss)]
            let y = h - (*v as f64 / max) * (h - 2.0) - 1.0;
            format!("{x:.2},{y:.2}")
        })
        .collect();
    let line = pts.join(" ");
    let fill = format!("0,{h} {line} {w},{h}");
    Some(SparkPath { line, fill })
}

#[cfg(test)]
mod tests {
    use super::spark_path;

    fn ys(line: &str) -> Vec<f64> {
        line.split(' ')
            .map(|pt| pt.split(',').nth(1).unwrap().parse().unwrap())
            .collect()
    }

    fn xs(line: &str) -> Vec<f64> {
        line.split(' ')
            .map(|pt| pt.split(',').next().unwrap().parse().unwrap())
            .collect()
    }

    #[test]
    fn degenerate_inputs_yield_none() {
        assert_eq!(spark_path(&[], 120.0, 22.0), None);
        assert_eq!(spark_path(&[5], 120.0, 22.0), None);
    }

    #[test]
    fn all_zero_series_draws_a_flat_bottom_line() {
        let p = spark_path(&[0, 0, 0], 120.0, 22.0).unwrap();
        let ys = ys(&p.line);
        // max clamps to 1, every v/max is 0 → y = h - 1 for all points.
        assert!(ys.iter().all(|y| (*y - 21.0).abs() < f64::EPSILON));
    }

    #[test]
    fn all_equal_series_draws_a_flat_top_line() {
        let p = spark_path(&[7, 7, 7, 7], 120.0, 22.0).unwrap();
        let ys = ys(&p.line);
        // v == max → y = h - (h - 2) - 1 = 1 (1px top margin).
        assert!(ys.iter().all(|y| (*y - 1.0).abs() < f64::EPSILON));
    }

    #[test]
    fn scaling_is_monotonic_and_x_spans_the_width() {
        let p = spark_path(&[1, 2, 4], 100.0, 20.0).unwrap();
        let ys = ys(&p.line);
        // Bigger values sit higher (smaller y).
        assert!(ys[0] > ys[1] && ys[1] > ys[2]);
        let xs = xs(&p.line);
        assert!((xs[0] - 0.0).abs() < f64::EPSILON);
        assert!((xs[2] - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fill_closes_the_polygon_along_the_baseline() {
        let p = spark_path(&[1, 2], 100.0, 20.0).unwrap();
        assert!(p.fill.starts_with("0,20 "));
        assert!(p.fill.ends_with(" 100,20"));
        assert!(p.fill.contains(&p.line));
    }
}
