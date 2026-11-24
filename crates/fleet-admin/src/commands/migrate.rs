// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use fleet_auth::migrate;
use sqlx::postgres::PgPool;

use crate::error::AdminError;

pub async fn run(pool: &PgPool) -> Result<(), AdminError> {
    migrate(pool).await?;
    eprintln!("fleet-admin: migrations applied");
    Ok(())
}
