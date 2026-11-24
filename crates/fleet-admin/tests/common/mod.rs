// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)] // each test binary uses a subset of these items

use fleet_auth::KeyStore;
use sqlx::PgPool;

pub async fn migrated_store(pool: PgPool) -> KeyStore {
    fleet_auth::migrate(&pool)
        .await
        .expect("apply fleet-auth migrations to per-test database");
    KeyStore::from_pool(pool)
}

pub fn isolated_database_url(pool: &PgPool) -> String {
    let options = pool.connect_options();
    let database = options.get_database().expect("test pool has a database");
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must configure Postgres tests");
    let (base, query) = url
        .split_once('?')
        .map_or((url.as_str(), None), |(base, query)| (base, Some(query)));
    let after_scheme = base.find("://").map_or(0, |index| index + 3);
    let authority = base[after_scheme..]
        .find('/')
        .map_or(base, |index| &base[..after_scheme + index]);

    match query {
        Some(query) => format!("{authority}/{database}?{query}"),
        None => format!("{authority}/{database}"),
    }
}
