// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The incomplete-results notice's dismissal identity.
//!
//! A dismissal has to survive paging the same result — the query and the
//! degraded set are unchanged, only the offset moved — and has to lapse
//! the moment either half changes, so a new query, or the same query
//! after a repin retired one of the fields, gets the notice back.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// Injective across the join: each part is length-prefixed, so
/// `("ab", ["c"])` and `("a", ["bc"])` cannot collide into one key and
/// silently inherit each other's dismissal.
#[must_use]
pub fn degraded_notice_key(query: &str, fields: &[String]) -> String {
    let mut names: Vec<&str> = fields.iter().map(String::as_str).collect();
    names.sort_unstable();
    names.dedup();

    let mut key = String::new();
    push_part(&mut key, query);
    for name in names {
        push_part(&mut key, name);
    }
    key
}

fn push_part(out: &mut String, part: &str) {
    out.push_str(&part.len().to_string());
    out.push(':');
    out.push_str(part);
}

#[cfg(test)]
mod tests {
    use super::degraded_notice_key;

    fn fields(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn same_query_and_set_is_the_same_notice() {
        let a = degraded_notice_key("status>=400 last=1h", &fields(&["duration"]));
        let b = degraded_notice_key("status>=400 last=1h", &fields(&["duration"]));
        assert_eq!(a, b);
    }

    #[test]
    fn field_order_and_duplicates_do_not_matter() {
        let sorted = degraded_notice_key("*", &fields(&["duration", "status"]));
        assert_eq!(
            sorted,
            degraded_notice_key("*", &fields(&["status", "duration"]))
        );
        assert_eq!(
            sorted,
            degraded_notice_key("*", &fields(&["status", "duration", "status"]))
        );
    }

    #[test]
    fn either_half_changing_is_a_new_notice() {
        let base = degraded_notice_key("*", &fields(&["duration"]));
        assert_ne!(
            base,
            degraded_notice_key("* | head 5", &fields(&["duration"]))
        );
        assert_ne!(
            base,
            degraded_notice_key("*", &fields(&["duration", "status"]))
        );
        assert_ne!(base, degraded_notice_key("*", &fields(&["status"])));
        assert_ne!(base, degraded_notice_key("*", &fields(&[])));
    }

    #[test]
    fn parts_cannot_run_together() {
        assert_ne!(
            degraded_notice_key("ab", &fields(&["c"])),
            degraded_notice_key("a", &fields(&["bc"]))
        );
        assert_ne!(
            degraded_notice_key("*", &fields(&["a:b"])),
            degraded_notice_key("*", &fields(&["a", "b"]))
        );
    }

    #[test]
    fn multibyte_names_are_prefixed_in_bytes_not_chars() {
        assert_ne!(
            degraded_notice_key("*", &fields(&["dur\u{e9}e"])),
            degraded_notice_key("*", &fields(&["dur\u{e9}"]))
        );
    }
}
