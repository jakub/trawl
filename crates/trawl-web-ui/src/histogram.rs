// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure-logic helpers for the search-page histogram strip.
//!
//! Splits an iterator of `(timestamp_seconds, is_error)` tuples into
//! `n_buckets` evenly-spaced buckets between the observed min/max
//! timestamp. Each bucket returns `(ok_count, err_count)`.
//!
//! The histogram component is intentionally tolerant: callers pass
//! whatever timestamps they can extract (parsed `_time` strings,
//! ingest receipt times, etc.). When no timestamps are available the
//! component renders a neutral placeholder rather than guessing.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub ok: u32,
    pub err: u32,
}

#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)] // bucket arithmetic is bounded; precision loss past 2^52 events is acceptable
pub fn bucketize(events: &[(f64, bool)], n_buckets: usize) -> Vec<Bucket> {
    if events.is_empty() || n_buckets == 0 {
        return Vec::new();
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for &(t, _) in events {
        if t < min {
            min = t;
        }
        if t > max {
            max = t;
        }
    }
    // All same timestamp → degenerate range; widen by 1s so bucket
    // assignment doesn't divide by zero.
    let range = (max - min).max(1.0);
    let mut out = vec![Bucket { ok: 0, err: 0 }; n_buckets];
    let n = n_buckets as f64;
    for &(t, is_err) in events {
        let raw = (((t - min) / range) * n).floor();
        let idx = if raw <= 0.0 { 0 } else { raw as usize };
        let i = idx.min(n_buckets - 1);
        if is_err {
            out[i].err += 1;
        } else {
            out[i].ok += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        assert!(bucketize(&[], 10).is_empty());
    }

    #[test]
    fn zero_buckets_returns_empty() {
        let events = [(1.0, false), (2.0, true)];
        assert!(bucketize(&events, 0).is_empty());
    }

    #[test]
    fn single_event_lands_in_first_bucket() {
        let events = [(5.0, false)];
        let out = bucketize(&events, 4);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], Bucket { ok: 1, err: 0 });
        assert_eq!(out[1], Bucket { ok: 0, err: 0 });
    }

    #[test]
    fn distributes_across_buckets() {
        let events = [(0.0, false), (1.0, true), (2.0, false), (3.0, true)];
        let out = bucketize(&events, 4);
        assert_eq!(out.len(), 4);
        let total_ok: u32 = out.iter().map(|b| b.ok).sum();
        let total_err: u32 = out.iter().map(|b| b.err).sum();
        assert_eq!(total_ok, 2);
        assert_eq!(total_err, 2);
    }

    #[test]
    fn last_event_lands_in_last_bucket() {
        let events = [(0.0, false), (10.0, true)];
        let out = bucketize(&events, 5);
        assert_eq!(out[0].ok, 1);
        assert_eq!(out[4].err, 1);
    }
}
