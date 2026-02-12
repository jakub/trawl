//! Log ingestion pipeline: HTTP handler → WAL → parquet compaction.

pub mod compaction;
pub mod handler;
pub mod types;
pub mod wal;
