// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[path = "build_support/duckdb.rs"]
mod duckdb;
#[path = "build_support/provenance.rs"]
mod provenance;

fn main() {
    duckdb::prepare();
    provenance::main();
}
