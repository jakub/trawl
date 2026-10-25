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

/// The permission the catalog read surfaces are gated on server-side
/// (`/schema/fields`, `/schema/field`, `/schema/conflicts`,
/// `/schema/services`, `/schema/repin/status`). A repin job read by its
/// id (`/schema/repin/status?id=`) accepts it, or [`SCHEMA_WRITE`] for a
/// job the same key requested.
const SCHEMA_READ: &str = "schema_read";

/// The permission `POST /api/v1/schema/repin` is gated on. The server is
/// the sole enforcement; this predicate only decides whether the UI
/// offers the affordance.
const SCHEMA_WRITE: &str = "schema_write";

/// The permission that lists and reads investigations, together with
/// `query` (ADR-0059).
const INVESTIGATION_READ: &str = "investigation_read";

/// The permission that writes to investigations, together with
/// [`INVESTIGATION_READ`] and `query` (ADR-0059).
const INVESTIGATION_WRITE: &str = "investigation_write";

/// Whether the resolved permission set carries trawl admin capability.
pub fn is_trawl_admin(permissions: &[String]) -> bool {
    permissions.iter().any(|p| p == SERVER_MANAGE)
}

/// Whether the session may read the field catalog.
// Consumed by the query notice's case-file links: a session without
// `schema_read` gets the field names as plain text, because the link
// target would 403.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn can_schema_read(permissions: &[String]) -> bool {
    permissions.iter().any(|p| p == SCHEMA_READ)
}

/// Whether the session may trigger a repin.
// Consumed by the case file's remedy block, where the button and the
// `trawl schema repin` line are alternatives rather than a disabled
// pair: with the permission the reader gets the trigger, without it the
// command to run from a shell.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn can_schema_write(permissions: &[String]) -> bool {
    permissions.iter().any(|p| p == SCHEMA_WRITE)
}

/// Query listing requires its own permission, even for administrators.
pub fn can_query(permissions: &[String]) -> bool {
    permissions.iter().any(|p| p == "query")
}

/// Whether the session may list and read investigations. The server
/// requires `query` as well, because kept evidence is event data, so the
/// pages and their navigation entry appear only for both (ADR-0059).
// Consumed by the investigation pages and their rail entry.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn can_read_investigations(permissions: &[String]) -> bool {
    can_query(permissions) && permissions.iter().any(|p| p == INVESTIGATION_READ)
}

/// Whether the session may write to investigations: keep, record a
/// finding, close or reopen. Writing needs everything reading needs.
// Consumed by the investigation write controls.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn can_write_investigations(permissions: &[String]) -> bool {
    can_read_investigations(permissions) && permissions.iter().any(|p| p == INVESTIGATION_WRITE)
}

/// Ownership is server-computed for each row, never inferred from names.
pub fn can_cancel_query(permissions: &[String], own: bool) -> bool {
    is_trawl_admin(permissions) || (own && permissions.iter().any(|p| p == "query_cancel"))
}

#[cfg(test)]
mod tests {
    use super::{can_schema_read, can_schema_write, is_trawl_admin};

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

    #[test]
    fn schema_read_is_its_own_permission() {
        assert!(can_schema_read(&["schema_read".to_string()]));
        assert!(can_schema_read(&[
            "query_read".to_string(),
            "schema_read".to_string()
        ]));
        // Neither admin nor write implies read: permissions are a flat
        // set the server resolves, with no hierarchy (ADR-0006).
        assert!(!can_schema_read(&["server_manage".to_string()]));
        assert!(!can_schema_read(&["schema_write".to_string()]));
        assert!(!can_schema_read(&[]));
    }

    #[test]
    fn schema_write_is_its_own_permission() {
        assert!(can_schema_write(&["schema_write".to_string()]));
        assert!(!can_schema_write(&["schema_read".to_string()]));
        assert!(!can_schema_write(&["server_manage".to_string()]));
        assert!(!can_schema_write(&[]));
    }
}

#[cfg(test)]
mod query_tests {
    use super::*;
    #[test]
    fn query_and_cancel_permissions_are_independent() {
        for admin in [false, true] {
            for query in [false, true] {
                for cancel in [false, true] {
                    let p = [
                        (admin, "server_manage"),
                        (query, "query"),
                        (cancel, "query_cancel"),
                    ]
                    .into_iter()
                    .filter(|(enabled, _)| *enabled)
                    .map(|(_, p)| p.to_owned())
                    .collect::<Vec<_>>();
                    assert_eq!(can_query(&p), query);
                    for own in [false, true] {
                        assert_eq!(can_cancel_query(&p, own), admin || (cancel && own));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod investigation_tests {
    use super::*;

    #[test]
    fn investigation_access_needs_query_and_write_needs_read() {
        for query in [false, true] {
            for read in [false, true] {
                for write in [false, true] {
                    for admin in [false, true] {
                        let p = [
                            (query, "query"),
                            (read, "investigation_read"),
                            (write, "investigation_write"),
                            (admin, "server_manage"),
                        ]
                        .into_iter()
                        .filter(|(enabled, _)| *enabled)
                        .map(|(_, p)| p.to_owned())
                        .collect::<Vec<_>>();
                        // server_manage implies neither: the set is flat.
                        assert_eq!(can_read_investigations(&p), query && read, "{p:?}");
                        assert_eq!(
                            can_write_investigations(&p),
                            query && read && write,
                            "{p:?}"
                        );
                    }
                }
            }
        }
    }
}
