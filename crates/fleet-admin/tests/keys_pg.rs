// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration coverage for `fleet-admin keys` against a real Postgres.
//!
//! Calls fleet-auth's `KeyStore` directly (the same API surface the CLI
//! drives) rather than spawning the `fleet-admin` binary per test. Faster,
//! and the binary's command functions are thin enough that they don't
//! contribute additional behaviour worth re-validating from a subprocess.

#[macro_use]
mod common;

use std::time::Duration;

use fleet_admin::commands::keys::{self, KeyPrefix};
use fleet_admin::error::AdminError;
use fleet_auth::{AuthError, KeyStore, PrincipalKind, RoleAssignment};

fn trawl_admin() -> RoleAssignment {
    RoleAssignment {
        app: "trawl".into(),
        role: "admin".into(),
    }
}

fn coastwatch_consumer() -> RoleAssignment {
    RoleAssignment {
        app: "coastwatch".into(),
        role: "siem_consumer".into(),
    }
}

pg_test!(
    create_emits_flt_prefix_and_persists,
    |store: KeyStore| async move {
        let created = store
            .create_key("svc", PrincipalKind::Service, &[trawl_admin()], None)
            .await
            .expect("create");

        assert!(
            created.plaintext_token.starts_with("flt_"),
            "expected flt_ prefix, got {:?}",
            &*created.plaintext_token
        );
        assert!(created.info.active);
        assert!(created.info.revoked_at.is_none());

        let listed = store.list_keys(true).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].prefix, created.info.prefix);
        assert_eq!(listed[0].name, "svc");
        assert_eq!(listed[0].kind, PrincipalKind::Service);
        assert_eq!(listed[0].assignments.len(), 1);
    }
);

pg_test!(list_all_includes_revoked, |store: KeyStore| async move {
    let alive = store
        .create_key("alive", PrincipalKind::Human, &[trawl_admin()], None)
        .await
        .unwrap();
    let dead = store
        .create_key("dead", PrincipalKind::Human, &[trawl_admin()], None)
        .await
        .unwrap();
    store.revoke_key(&dead.info.prefix).await.unwrap();

    let active_only = store.list_keys(true).await.unwrap();
    assert_eq!(active_only.len(), 1);
    assert_eq!(active_only[0].prefix, alive.info.prefix);

    let all = store.list_keys(false).await.unwrap();
    assert_eq!(all.len(), 2);
    let revoked = all.iter().find(|k| k.prefix == dead.info.prefix).unwrap();
    assert!(!revoked.active);
    assert!(revoked.revoked_at.is_some());
});

pg_test!(
    revoke_sets_active_false_and_revoked_at,
    |store: KeyStore| async move {
        let created = store
            .create_key("target", PrincipalKind::Human, &[], None)
            .await
            .unwrap();

        let info = store
            .revoke_key(&created.info.prefix)
            .await
            .expect("revoke");
        assert!(!info.active);
        assert!(info.revoked_at.is_some(), "revoked_at must be populated");

        let err = store
            .verify_key(&created.plaintext_token)
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidKey(_)));
    }
);

pg_test!(
    grant_adds_assignment_and_rejects_duplicate,
    |store: KeyStore| async move {
        let created = store
            .create_key("grantee", PrincipalKind::Service, &[], None)
            .await
            .unwrap();

        store
            .grant_assignment(&created.info.prefix, &trawl_admin())
            .await
            .expect("first grant");

        // Same (key, app) again → GrantExists, NOT silent overwrite.
        let err = store
            .grant_assignment(&created.info.prefix, &trawl_admin())
            .await
            .unwrap_err();
        assert!(
            matches!(err, AuthError::GrantExists { ref app, .. } if app == "trawl"),
            "expected GrantExists for app=trawl, got {err:?}"
        );

        store
            .grant_assignment(&created.info.prefix, &coastwatch_consumer())
            .await
            .expect("grant on different app");

        let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
        assert_eq!(info.assignments.len(), 2);
    }
);

pg_test!(
    grant_on_unknown_prefix_returns_key_not_found,
    |store: KeyStore| async move {
        let err = store
            .grant_assignment("ghostpfx", &trawl_admin())
            .await
            .unwrap_err();
        assert!(
            matches!(err, AuthError::KeyNotFound { ref prefix } if prefix == "ghostpfx"),
            "expected KeyNotFound, got {err:?}"
        );
    }
);

pg_test!(
    revoke_unknown_prefix_returns_key_not_found,
    |store: KeyStore| async move {
        let err = store.revoke_key("ghostpfx").await.unwrap_err();
        assert!(
            matches!(err, AuthError::KeyNotFound { ref prefix } if prefix == "ghostpfx"),
            "expected KeyNotFound, got {err:?}"
        );
    }
);

pg_test!(
    create_with_expires_persists_expires_at,
    |store: KeyStore| async move {
        let ninety_days = Duration::from_secs(90 * 86_400);
        let before = chrono::Utc::now();
        keys::create(
            &store,
            "exp",
            PrincipalKind::Service,
            &[trawl_admin()],
            Some(ninety_days),
        )
        .await
        .expect("create");

        let listed = store.list_keys(true).await.expect("list");
        assert_eq!(listed.len(), 1);
        let expires_at = listed[0].expires_at.expect("expires_at must be populated");

        let expected = before + chrono::Duration::seconds(90 * 86_400);
        let delta = (expires_at - expected).num_seconds().abs();
        assert!(
            delta < 5,
            "expires_at drift {delta}s vs expected {expected}, got {expires_at}"
        );
    }
);

pg_test!(
    revoke_twice_returns_already_revoked,
    |store: KeyStore| async move {
        let created = store
            .create_key("twice", PrincipalKind::Human, &[trawl_admin()], None)
            .await
            .unwrap();
        let prefix = KeyPrefix::parse(&created.info.prefix).expect("parse prefix");

        keys::revoke(&store, &prefix, true)
            .await
            .expect("first revoke");

        let err = keys::revoke(&store, &prefix, true)
            .await
            .expect_err("second revoke must error");
        assert!(
            matches!(err, AdminError::AlreadyRevoked { ref prefix, .. } if prefix == &created.info.prefix),
            "expected AlreadyRevoked, got {err:?}"
        );
    }
);

pg_test!(
    create_duplicate_name_is_allowed,
    |store: KeyStore| async move {
        // Pin the schema contract — name is NOT unique, two keys can share
        // a label (operators distinguish them by prefix).
        store
            .create_key("dup", PrincipalKind::Service, &[], None)
            .await
            .expect("first");
        store
            .create_key("dup", PrincipalKind::Service, &[], None)
            .await
            .expect("second");
        let all = store.list_keys(true).await.unwrap();
        assert_eq!(all.iter().filter(|k| k.name == "dup").count(), 2);
    }
);
