//! Log ingestion pipeline: HTTP handler → WAL → parquet compaction.

pub mod compaction;
pub mod handler;
pub mod pipeline;
pub mod wal;
