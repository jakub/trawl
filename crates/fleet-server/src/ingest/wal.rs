//! Write-ahead log for crash-safe event ingestion.
//!
//! Each ingest request writes events to a WAL file atomically:
//! write to `.tmp`, then rename to `.ndjson`. The compaction task
//! later converts these to parquet.

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

    /// Write events atomically: `.tmp` → rename to `.ndjson`.
    ///
    /// Returns the final path of the WAL file on success.
    pub fn write(&self, service: &str, events: &[u8]) -> std::io::Result<PathBuf> {
        let filename = Self::generate_filename(service);
        let tmp_path = self.wal_dir.join(format!("{filename}.tmp"));
        let final_path = self.wal_dir.join(format!("{filename}.ndjson"));

        std::fs::write(&tmp_path, events)?;
        std::fs::rename(&tmp_path, &final_path)?;

        Ok(final_path)
    }

    /// Generate a unique filename: `{service}_{unix_millis}_{4_hex_random}`.
    fn generate_filename(service: &str) -> String {
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_millis();

        // 4 hex chars of randomness to avoid collisions within the same ms.
        let random: u16 = rand::random();
        let hex = format!("{random:04x}");

        // Sanitize service name: only alphanumeric, dash, underscore.
        let safe_service: String = service
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();

        format!("{safe_service}_{millis}_{hex}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_ndjson_file() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().to_path_buf());
        writer.ensure_dir().unwrap();

        let events = b"{\"service\":\"test\",\"message\":\"hello\"}\n";
        let path = writer.write("test", events).unwrap();

        assert!(path.exists());
        assert!(path.extension().is_some_and(|ext| ext == "ndjson"));
        assert_eq!(std::fs::read(&path).unwrap(), events);
    }

    #[test]
    fn filename_sanitizes_service_name() {
        let name = WalWriter::generate_filename("my/bad service");
        assert!(!name.contains('/'));
        assert!(name.starts_with("my_bad_service_"));
    }

    #[test]
    fn no_tmp_file_left_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().to_path_buf());
        writer.ensure_dir().unwrap();

        writer.write("test", b"{}\n").unwrap();

        let tmp_files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(tmp_files.is_empty(), "no .tmp files should remain");
    }
}
