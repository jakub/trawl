// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Log ingestion pipeline: HTTP handler → WAL → parquet compaction.

pub mod compaction;
pub mod envelope;
pub mod handler;
pub mod pipeline;
pub mod wal;
