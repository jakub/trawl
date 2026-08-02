// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Write-ahead log for crash-safe event ingestion.
//!
//! Each ingest request writes events to a WAL file durably: write to
//! `.tmp`, fsync the data, rename to `.ndjson`, then fsync the parent
//! directory. The fsync *before* the rename is what prevents a hard kill
//! from leaving a full-length but NUL-filled file (unflushed blocks read
//! back as zeros) — an unparseable poison pill that would later wedge
//! compaction. The compaction task converts these to parquet.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Atomic WAL file writer for ingest events.
#[derive(Debug)]
pub struct WalWriter {
    wal_dir: PathBuf,
}

impl WalWriter {
    /// Create a new writer targeting the given WAL directory.
    pub fn new(wal_dir: PathBuf) -> Self {
        Self { wal_dir }
    }

    /// Ensure the WAL directory exists.
    pub fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.wal_dir)
    }

    /// The WAL directory path.
    pub fn dir(&self) -> &Path {
        &self.wal_dir
    }

    /// Write events durably: create `.tmp`, fsync its data, rename to
    /// `.ndjson`, then fsync the parent directory so the rename itself
    /// survives a crash.
    ///
    /// The fsync *before* the rename prevents a torn write: without it the
    /// rename can be journaled before the data blocks reach the device, so a
    /// hard kill / power loss leaves a full-length `.ndjson` of NUL bytes
    /// that head-of-line-blocks compaction. The parent-directory fsync makes
    /// the rename entry durable so a crash can't lose the just-acked batch.
    ///
    /// Returns the final path of the WAL file on success.
    ///
    /// Files land in `wal_dir/{env}/` (lazily created), named
    /// `{service}_{unix_millis}_{4_hex}.ndjson` with the service name
    /// VERBATIM — path encoding is injective by validation (ADR-0009):
    /// both `env` and `service` were validated at ingest, so `api.v2`
    /// and `api_v2` are distinct files and pruning stays exact.
    pub fn write(&self, env: &str, service: &str, events: &[u8]) -> std::io::Result<PathBuf> {
        let filename = Self::generate_filename(service)?;
        let env_dir = self.wal_dir.join(env);
        std::fs::create_dir_all(&env_dir)?;
        let tmp_path = env_dir.join(format!("{filename}.tmp"));
        let final_path = env_dir.join(format!("{filename}.ndjson"));

        let mut file = File::create(&tmp_path)?;
        file.write_all(events)?;
        file.sync_all()?;
        drop(file);

        std::fs::rename(&tmp_path, &final_path)?;

        // fsync the directory entry so the rename is durable, not just the
        // file's data — the canonical "make a rename crash-safe" step.
        //
        // Best-effort: the data fsync above already made the bytes durable and
        // the rename has published the file (the compactor WILL consume it), so
        // a dir-fsync failure here only weakens crash-survival of the rename
        // entry. It must NOT fail an otherwise-successful, already-visible write
        // — that would falsely reject the batch and risk a duplicate on retry.
        if let Err(e) = File::open(&env_dir).and_then(|d| d.sync_all()) {
            tracing::warn!(
                event_type = "wal_dir_fsync_failed",
                dir = %env_dir.display(),
                error = %e,
                "WAL parent-dir fsync failed; write is durable but the rename \
                 entry may not survive a crash"
            );
        }

        Ok(final_path)
    }

    /// Generate a unique filename: `{service}_{unix_millis}_{4_hex_random}`.
    ///
    /// The service name is carried verbatim — it was validated at ingest.
    fn generate_filename(service: &str) -> std::io::Result<String> {
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_millis();

        // 4 hex chars of randomness to avoid collisions within the same ms.
        let random: u16 = rand::random();
        let hex = format!("{random:04x}");

        Ok(format!("{service}_{millis}_{hex}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_ndjson_file_under_env_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().to_path_buf());
        writer.ensure_dir().unwrap();

        let events = b"{\"service\":\"test\",\"message\":\"hello\"}\n";
        let path = writer.write("prod", "test", events).unwrap();

        assert!(path.exists());
        assert!(path.extension().is_some_and(|ext| ext == "ndjson"));
        assert_eq!(std::fs::read(&path).unwrap(), events);
        assert_eq!(
            path.parent().unwrap(),
            tmp.path().join("prod"),
            "WAL files land under wal_dir/{{env}}/"
        );
    }

    #[test]
    fn filename_carries_service_verbatim() {
        // api.v2 and api_v2 must be DISTINCT files (injective paths,
        // ADR-0009) — the old sanitizer collapsed them.
        let dotted = WalWriter::generate_filename("api.v2").unwrap();
        let underscored = WalWriter::generate_filename("api_v2").unwrap();
        assert!(dotted.starts_with("api.v2_"), "got {dotted}");
        assert!(underscored.starts_with("api_v2_"), "got {underscored}");
    }

    #[test]
    fn two_envs_same_service_are_separate_files() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().to_path_buf());
        writer.ensure_dir().unwrap();

        let prod = writer.write("prod", "svc", b"{}\n").unwrap();
        let lab = writer.write("lab", "svc", b"{}\n").unwrap();
        assert_ne!(prod, lab);
        assert!(prod.starts_with(tmp.path().join("prod")));
        assert!(lab.starts_with(tmp.path().join("lab")));
    }

    #[test]
    fn no_tmp_file_left_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().to_path_buf());
        writer.ensure_dir().unwrap();

        writer.write("prod", "test", b"{}\n").unwrap();

        let tmp_files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(tmp_files.is_empty(), "no .tmp files should remain");
    }
}
