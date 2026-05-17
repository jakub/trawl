// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres integration tests for fleet-auth's `KeyStore`.
//!
//! Requires a reachable Postgres at `$FLEET_DATABASE_URL` (or `$DATABASE_URL`
//! as a fallback) with `CREATEDB` privilege. Each test gets a fresh ephemeral
//! database — created with the schema migration applied, used, then dropped.
//! Skipped (with a clear message) when no DB URL is set in the environment.
//!
//! We hand-roll the fixture instead of using `#[sqlx::test]` because
//! `sqlx-macros` transitively triggers a cargo links-uniqueness conflict
//! with the workspace's rusqlite (see `migrations.rs` for context).

#![cfg(feature = "keystore")]

use std::time::Duration;

use fleet_auth::{
    AuthError, KeyStore, MIGRATOR, PrincipalKind, RoleAssignment, token, validate_app_namespace,
};
use sqlx_core::executor::Executor as _;
use sqlx_postgres::{PgConnectOptions, PgConnection, PgPool, PgPoolOptions};

/// Reads the base database URL from the environment.
///
/// Empty values are treated as unset — important so a CI secret that fails
/// to inject (lands as empty string) doesn't silently become "no DB", which
/// in turn would make `FLEET_TESTS_REQUIRED=1` mis-fire.
fn base_database_url() -> Option<String> {
    for var in ["FLEET_DATABASE_URL", "DATABASE_URL"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            return Some(v);
        }
    }
    None
}

/// If set, missing/unreachable Postgres is a hard failure instead of a skip.
/// CI sets this so a misconfigured `FLEET_DATABASE_URL` secret can't silently
/// turn the integration suite into a green no-op.
fn require_database() -> bool {
    std::env::var("FLEET_TESTS_REQUIRED").is_ok_and(|v| !v.is_empty())
}

/// RAII fixture: create + migrate a fresh per-test database, hand out a pool,
/// drop the database on `Drop`.
struct PgFixture {
    admin_opts: PgConnectOptions,
    test_db: String,
    pool: Option<PgPool>,
}

impl PgFixture {
    async fn setup() -> Option<Self> {
        use sqlx_core::connection::Connection as _;

        let admin_url = base_database_url()?;
        let admin_opts: PgConnectOptions = admin_url
            .parse()
            .expect("FLEET_DATABASE_URL is not a valid Postgres URL");
        let test_db = format!("fleet_auth_test_{}", random_db_suffix());

        // Connect to the admin DB to issue CREATE DATABASE — this requires
        // CREATEDB on the connecting role.
        let mut admin: PgConnection = PgConnection::connect_with(&admin_opts)
            .await
            .expect("connect to admin DB (requires reachable Postgres + CREATEDB)");

        admin
            .execute(format!(r#"CREATE DATABASE "{test_db}""#).as_str())
            .await
            .expect("CREATE DATABASE — does the role have CREATEDB?");

        // Swap to the per-test database via the typed options builder instead
        // of hand-rolling URL surgery — `database()` overrides cleanly and
        // can't be tricked by usernames containing slashes etc.
        let test_opts = admin_opts.clone().database(&test_db);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(test_opts)
            .await
            .expect("connect to per-test DB");

        MIGRATOR.run(&pool).await.expect("apply migrations");

        Some(Self {
            admin_opts,
            test_db,
            pool: Some(pool),
        })
    }

    fn pool(&self) -> PgPool {
        self.pool.as_ref().expect("fixture live").clone()
    }
}

impl Drop for PgFixture {
    fn drop(&mut self) {
        // Run teardown on a dedicated OS thread so we never start a nested
        // runtime inside the test's own tokio runtime. `DROP DATABASE ...
        // WITH (FORCE)` terminates any lingering connections from the per-
        // test pool (its own Drop closes them lazily).
        self.pool.take(); // release our handle so WITH (FORCE) can reclaim
        let admin_opts = self.admin_opts.clone();
        let test_db = self.test_db.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("teardown runtime");
            rt.block_on(async move {
                use sqlx_core::connection::Connection as _;
                if let Ok(mut admin) = PgConnection::connect_with(&admin_opts).await {
                    let _ = admin
                        .execute(
                            format!(r#"DROP DATABASE IF EXISTS "{test_db}" WITH (FORCE)"#).as_str(),
                        )
                        .await;
                }
            });
        });
        // Wait for teardown so a fast test loop doesn't out-run cleanup.
        let _ = handle.join();
    }
}

fn random_db_suffix() -> String {
    use rand::Rng as _;
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| {
            let n: u8 = rng.gen_range(0..26);
            (b'a' + n) as char
        })
        .collect()
}

/// Macro to skip a test with a clear message when no DB is configured —
/// unless `FLEET_TESTS_REQUIRED` is set, in which case the absence is a
/// hard failure (CI relies on this so a misconfigured secret can't quietly
/// turn the suite into a green no-op).
macro_rules! pg_test {
    ($name:ident, $body:expr) => {
        #[tokio::test]
        async fn $name() {
            let Some(fx) = PgFixture::setup().await else {
                let msg = format!(
                    "fleet-auth integration test '{}' skipped: FLEET_DATABASE_URL not set or empty",
                    stringify!($name)
                );
                if require_database() {
                    panic!("{msg} — but FLEET_TESTS_REQUIRED is set, so this is a hard failure",);
                }
                eprintln!("{msg}");
                return;
            };
            let pool = fx.pool();
            let store = KeyStore::from_pool(pool);
            #[allow(clippy::redundant_closure_call)]
            ($body)(store).await;
            drop(fx);
        }
    };
}

fn trawl_admin() -> Vec<RoleAssignment> {
    vec![RoleAssignment {
        app: "trawl".into(),
        role: "admin".into(),
    }]
}

// ---------------------------------------------------------------------------
// lifecycle tests
// ---------------------------------------------------------------------------

pg_test!(create_and_verify_roundtrip, |store: KeyStore| async move {
    let created = store
        .create_key("test", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .expect("create");
    assert!(created.plaintext_token.starts_with("flt_"));

    let verified = store
        .verify_key(&created.plaintext_token)
        .await
        .expect("verify");
    assert_eq!(verified.name, "test");
    assert_eq!(verified.kind, PrincipalKind::Human);
    assert_eq!(verified.role_for("trawl"), Some("admin"));
});

pg_test!(
    create_with_multiple_app_grants,
    |store: KeyStore| async move {
        let grants = vec![
            RoleAssignment {
                app: "trawl".into(),
                role: "analyst".into(),
            },
            RoleAssignment {
                app: "coastwatch".into(),
                role: "siem_consumer".into(),
            },
        ];
        let created = store
            .create_key("multi", PrincipalKind::Service, &grants, None)
            .await
            .expect("create");

        let verified = store.verify_key(&created.plaintext_token).await.unwrap();
        assert_eq!(verified.kind, PrincipalKind::Service);
        assert_eq!(verified.assignments.len(), 2);
        assert_eq!(verified.role_for("trawl"), Some("analyst"));
        assert_eq!(verified.role_for("coastwatch"), Some("siem_consumer"));
    }
);

pg_test!(
    duplicate_app_in_batch_rejected,
    |store: KeyStore| async move {
        let err = store
            .create_key(
                "dup",
                PrincipalKind::Human,
                &[
                    RoleAssignment {
                        app: "trawl".into(),
                        role: "admin".into(),
                    },
                    RoleAssignment {
                        app: "trawl".into(),
                        role: "reader".into(),
                    },
                ],
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::GrantExists { .. }));
    }
);

pg_test!(
    empty_grants_create_then_verify,
    |store: KeyStore| async move {
        let created = store
            .create_key("empty", PrincipalKind::Service, &[], None)
            .await
            .expect("create");
        let verified = store.verify_key(&created.plaintext_token).await.unwrap();
        assert!(verified.assignments.is_empty());
    }
);

pg_test!(
    create_key_with_expiry_in_future,
    |store: KeyStore| async move {
        let created = store
            .create_key(
                "expiring",
                PrincipalKind::Human,
                &trawl_admin(),
                Some(Duration::from_secs(86400)),
            )
            .await
            .expect("create");
        assert!(created.info.expires_at.is_some());
        let verified = store.verify_key(&created.plaintext_token).await.unwrap();
        assert_eq!(verified.name, "expiring");
    }
);

// ---------------------------------------------------------------------------
// verify_key security
// ---------------------------------------------------------------------------

pg_test!(
    verify_nonexistent_prefix_runs_dummy_and_fails,
    |store: KeyStore| async move {
        let result = store
            .verify_key("flt_ZZZZZZZZthisisntarealkeyatallnopenope12")
            .await;
        assert!(matches!(result, Err(AuthError::InvalidKey(_))));
    }
);

pg_test!(malformed_token_rejected, |store: KeyStore| async move {
    let result = store.verify_key("not-a-trawl-token").await;
    assert!(matches!(result, Err(AuthError::MalformedToken(_))));
});

pg_test!(
    wrong_token_with_existing_prefix_fails,
    |store: KeyStore| async move {
        let created = store
            .create_key("legit", PrincipalKind::Human, &trawl_admin(), None)
            .await
            .unwrap();
        // Same prefix, different body — should NOT verify (this is the security
        // regression test for the cache key shape — if cache were
        // (prefix, hash) only, this would falsely succeed after a successful
        // verify of the legit token).
        store.verify_key(&created.plaintext_token).await.unwrap();
        let prefix_only = &created.plaintext_token[..12]; // "flt_" + 8
        let tampered = format!("{prefix_only}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA0");
        let result = store.verify_key(&tampered).await;
        assert!(matches!(result, Err(AuthError::InvalidKey(_))));
    }
);

pg_test!(
    cache_hit_skips_kdf_but_updates_last_used,
    |store: KeyStore| async move {
        let created = store
            .create_key("cacheable", PrincipalKind::Human, &trawl_admin(), None)
            .await
            .unwrap();

        let before = store.cache_stats().entries;
        store.verify_key(&created.plaintext_token).await.unwrap();
        let after_first = store.cache_stats().entries;
        assert_eq!(after_first, before + 1, "first verify populates cache");

        // Capture last_used from after the first verify.
        let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
        let last_used_first = info.last_used.expect("set by verify");

        // Brief sleep so the next timestamp can differ (NOW() is microsecond-precise).
        tokio::time::sleep(Duration::from_millis(20)).await;

        store.verify_key(&created.plaintext_token).await.unwrap();
        let after_second = store.cache_stats().entries;
        assert_eq!(after_second, after_first, "cache hit, no new entry");

        let info_second = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
        let last_used_second = info_second.last_used.expect("still set");
        assert!(
            last_used_second > last_used_first,
            "cache hit still updates last_used"
        );
    }
);

pg_test!(
    revoke_blocks_verify_even_with_cache_entry,
    |store: KeyStore| async move {
        let created = store
            .create_key("revokable", PrincipalKind::Human, &trawl_admin(), None)
            .await
            .unwrap();
        // Populate cache.
        store.verify_key(&created.plaintext_token).await.unwrap();

        store.revoke_key(&created.info.prefix).await.unwrap();

        // Cache still has the entry, but the conditional UPDATE rejects.
        let result = store.verify_key(&created.plaintext_token).await;
        assert!(matches!(result, Err(AuthError::InvalidKey(_))));
    }
);

pg_test!(expired_key_fails_verify, |store: KeyStore| async move {
    // Can't backdate expires_at directly — the api_keys_expiry_after_create
    // CHECK constraint refuses any expires_at <= created_at. Instead, create
    // with a near-zero TTL and sleep past it before verifying.
    let created = store
        .create_key(
            "doomed",
            PrincipalKind::Human,
            &trawl_admin(),
            Some(Duration::from_millis(50)),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let result = store.verify_key(&created.plaintext_token).await;
    assert!(matches!(result, Err(AuthError::InvalidKey(_))));
});

pg_test!(
    toctou_revoke_between_select_and_update_rejects,
    |store: KeyStore| async move {
        let created = store
            .create_key("racy", PrincipalKind::Human, &trawl_admin(), None)
            .await
            .unwrap();
        // Populate cache via one successful verify, then revoke directly through
        // SQL so we don't bust the cache, then verify again. The conditional
        // last_used UPDATE catches the revocation.
        store.verify_key(&created.plaintext_token).await.unwrap();
        sqlx_core::query::query("UPDATE api_keys SET active = FALSE WHERE id = $1")
            .bind(created.info.id)
            .execute(store.pool())
            .await
            .unwrap();
        let result = store.verify_key(&created.plaintext_token).await;
        assert!(matches!(result, Err(AuthError::InvalidKey(_))));
    }
);

pg_test!(
    concurrent_verifies_all_succeed,
    |store: KeyStore| async move {
        let created = store
            .create_key("concurrent", PrincipalKind::Human, &trawl_admin(), None)
            .await
            .unwrap();
        let token = created.plaintext_token.to_string();

        let mut handles = Vec::new();
        for _ in 0..16 {
            let s = store.clone();
            let t = token.clone();
            handles.push(tokio::spawn(async move { s.verify_key(&t).await }));
        }
        for h in handles {
            h.await.unwrap().expect("each verify succeeds");
        }
    }
);

pg_test!(
    revoke_assignment_waits_for_key_row_lock,
    |store: KeyStore| async move {
        let created = store
            .create_key(
                "lock-step",
                PrincipalKind::Human,
                &[
                    RoleAssignment {
                        app: "trawl".into(),
                        role: "admin".into(),
                    },
                    RoleAssignment {
                        app: "coastwatch".into(),
                        role: "siem_consumer".into(),
                    },
                ],
                None,
            )
            .await
            .unwrap();

        // `verify_key` now holds this same row lock while it fetches
        // assignments. Holding it manually gives the regression test a
        // deterministic interleaving instead of hoping the scheduler lands
        // inside a microsecond race window.
        let mut tx = store.pool().begin().await.unwrap();
        sqlx_core::query::query("SELECT id FROM api_keys WHERE id = $1 FOR UPDATE")
            .bind(created.info.id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();

        let revoke_store = store.clone();
        let prefix = created.info.prefix.clone();
        let revoke =
            tokio::spawn(
                async move { revoke_store.revoke_assignment(&prefix, "coastwatch").await },
            );

        assert!(
            tokio::time::timeout(Duration::from_millis(100), async {
                while !revoke.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "revoke_assignment must wait for the key row lock"
        );

        tx.commit().await.unwrap();
        revoke.await.unwrap().unwrap();

        let verified = store.verify_key(&created.plaintext_token).await.unwrap();
        assert_eq!(verified.role_for("coastwatch"), None);
    }
);

// ---------------------------------------------------------------------------
// admin operations
// ---------------------------------------------------------------------------

pg_test!(
    revoke_nonexistent_prefix_errors,
    |store: KeyStore| async move {
        let result = store.revoke_key("ZZZZZZZZ").await;
        assert!(matches!(result, Err(AuthError::KeyNotFound { .. })));
    }
);

pg_test!(
    double_revoke_errors_with_kindly_message,
    |store: KeyStore| async move {
        let created = store
            .create_key("twice", PrincipalKind::Human, &trawl_admin(), None)
            .await
            .unwrap();
        store.revoke_key(&created.info.prefix).await.unwrap();
        let result = store.revoke_key(&created.info.prefix).await;
        assert!(matches!(result, Err(AuthError::KeyRevoked { .. })));
    }
);

pg_test!(
    list_keys_active_only_filters,
    |store: KeyStore| async move {
        let keep = store
            .create_key("keep", PrincipalKind::Human, &trawl_admin(), None)
            .await
            .unwrap();
        let drop_me = store
            .create_key("drop", PrincipalKind::Human, &trawl_admin(), None)
            .await
            .unwrap();
        store.revoke_key(&drop_me.info.prefix).await.unwrap();

        let active = store.list_keys(true).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].prefix, keep.info.prefix);

        let all = store.list_keys(false).await.unwrap();
        assert_eq!(all.len(), 2);
    }
);

pg_test!(
    grant_and_revoke_assignment_roundtrip,
    |store: KeyStore| async move {
        let created = store
            .create_key("shared", PrincipalKind::Service, &trawl_admin(), None)
            .await
            .unwrap();
        store
            .grant_assignment(
                &created.info.prefix,
                &RoleAssignment {
                    app: "coastwatch".into(),
                    role: "siem_consumer".into(),
                },
            )
            .await
            .unwrap();

        let after = store.list_assignments(&created.info.prefix).await.unwrap();
        assert_eq!(after.len(), 2);

        store
            .revoke_assignment(&created.info.prefix, "coastwatch")
            .await
            .unwrap();
        let after_revoke = store.list_assignments(&created.info.prefix).await.unwrap();
        assert_eq!(after_revoke.len(), 1);
    }
);

pg_test!(grant_rejects_duplicate_app, |store: KeyStore| async move {
    let created = store
        .create_key("k", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    let err = store
        .grant_assignment(
            &created.info.prefix,
            &RoleAssignment {
                app: "trawl".into(),
                role: "reader".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::GrantExists { .. }));
});

pg_test!(revoke_missing_grant_errors, |store: KeyStore| async move {
    let created = store
        .create_key("k", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    let err = store
        .revoke_assignment(&created.info.prefix, "nonexistent")
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::GrantNotFound { .. }));
});

pg_test!(retype_key_succeeds, |store: KeyStore| async move {
    let created = store
        .create_key("morphing", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    let updated = store
        .retype_key(&created.info.prefix, PrincipalKind::Service)
        .await
        .unwrap();
    assert_eq!(updated.kind, PrincipalKind::Service);
});

pg_test!(retype_revoked_key_errors, |store: KeyStore| async move {
    let created = store
        .create_key("dead", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    store.revoke_key(&created.info.prefix).await.unwrap();
    let err = store
        .retype_key(&created.info.prefix, PrincipalKind::Service)
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::KeyRevoked { .. }));
});

pg_test!(ping_succeeds, |store: KeyStore| async move {
    store.ping().await.expect("ping");
});

// One sync sanity test to confirm imports compile without DB.
#[test]
fn validation_reexported_works() {
    assert!(validate_app_namespace("trawl").is_ok());
    assert!(validate_app_namespace("BAD").is_err());
    // and token module is reachable
    let t = token::generate_token();
    assert!(t.plaintext.starts_with("flt_"));
}
