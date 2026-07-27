// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Permission predicates for the SPA.
//!
//! ADR-0006: role names are operator-defined display strings, so the UI
//! never gates on a role — it gates on the resolved permission set from
//! `/me`. Every gating literal lives here so a permission rename is a
//! one-file edit.
//!
//! Pure + ungated so its tests run natively; the callers are all
//! wasm32-only, hence the native `dead_code` exemption.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// The permission that marks the trawl admin tier.
const SERVER_MANAGE: &str = "server_manage";

/// Whether the resolved permission set carries trawl admin capability.
pub fn is_trawl_admin(permissions: &[String]) -> bool {
    permissions.iter().any(|p| p == SERVER_MANAGE)
}

#[cfg(test)]
mod tests {
    use super::is_trawl_admin;

    #[test]
    fn admin_requires_server_manage() {
        assert!(is_trawl_admin(&["server_manage".to_string()]));
        assert!(is_trawl_admin(&[
            "query_read".to_string(),
            "server_manage".to_string()
        ]));
        assert!(!is_trawl_admin(&["query_read".to_string()]));
        assert!(!is_trawl_admin(&[]));
    }
}
