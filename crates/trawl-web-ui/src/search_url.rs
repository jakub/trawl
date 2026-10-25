// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The search URL contract (ADR-0027) lives in `trawl-search-url`, where the
//! MCP server reads the same codec. This module re-exports it under the
//! `crate::search_url::X` paths the UI uses and adds the one piece that
//! needs a UI type: admitting a [`fleet_ui::RangeValue`] from the date-range
//! dialog.

use trawl_search_url::query_merge::{QUICK_RANGES, RangeSpec};

pub use trawl_search_url::search_url::*;

/// Validate a dialog draft using the same preset list and range reader as URLs.
/// The caller commits the resulting range and reports navigator refusal too.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn normalize_dialog_range(picked: fleet_ui::RangeValue) -> Result<RangeSpec, String> {
    match picked {
        fleet_ui::RangeValue::Quick(id) => QUICK_RANGES
            .iter()
            .copied()
            .find(|preset| *preset == id)
            .map(RangeSpec::Quick)
            .ok_or_else(|| "Unknown quick range.".to_string()),
        fleet_ui::RangeValue::Absolute { from, to } => {
            let from = normalize_instant(&from).ok_or_else(|| {
                "From needs a timestamp like 2026-04-18T00:00:00Z (an offset is fine).".to_string()
            })?;
            let to = if to.trim().is_empty() || to.trim() == "now" {
                "now".to_string()
            } else {
                normalize_instant(&to).ok_or_else(|| {
                    "To needs a timestamp like 2026-04-18T00:00:00Z, or the word now.".to_string()
                })?
            };
            let picked = RangeSpec::Absolute { from, to };
            if !matches!(decode_range(&encode_range(&picked)), Verdict::Valid(_)) {
                return Err("To is before From.".to_string());
            }
            Ok(picked)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_range_admission_keeps_app_policy_and_canonical_bounds() {
        use fleet_ui::RangeValue;
        for id in QUICK_RANGES {
            assert_eq!(
                normalize_dialog_range(RangeValue::Quick((*id).into())),
                Ok(RangeSpec::Quick(id))
            );
        }
        assert!(normalize_dialog_range(RangeValue::Quick("30m".into())).is_err());
        assert_eq!(
            normalize_dialog_range(RangeValue::Absolute {
                from: "2026-01-01T03:00:00+03:00".into(),
                to: " ".into(),
            }),
            Ok(RangeSpec::Absolute {
                from: "2026-01-01T00:00:00Z".into(),
                to: "now".into()
            })
        );
        for (from, to) in [
            ("bad", "now"),
            ("2026-01-01T00:00:00Z", "bad"),
            ("2026-01-02T00:00:00Z", "2026-01-01T00:00:00Z"),
        ] {
            assert!(
                normalize_dialog_range(RangeValue::Absolute {
                    from: from.into(),
                    to: to.into()
                })
                .is_err()
            );
        }
    }
}
