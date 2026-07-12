// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fleet-admin migrate` — apply embedded fleet-auth schema migrations.

use fleet_auth::MIGRATOR;
use sqlx::postgres::PgPool;

use crate::error::AdminError;

/// Apply the fleet-auth migrator to the supplied pool.
///
/// The migrator is idempotent — applied versions are tracked in
/// `_sqlx_migrations`, so re-running this against an up-to-date database
/// is a no-op.
pub async fn run(pool: &PgPool) -> Result<(), AdminError> {
    MIGRATOR.run(pool).await?;
    eprintln!("fleet-admin: migrations applied");
    Ok(())
}
