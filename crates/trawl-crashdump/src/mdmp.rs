//! Just enough minidump header parsing to say whether a dump captured anything.
//!
//! A dump whose `PTRACE_ATTACH` was refused is still a well-formed file:
//! `minidump-writer` treats the failure as soft and writes a dump with no
//! threads and no memory. So "a `.dmp` exists" answers nothing and "how many
//! threads did it capture" answers everything. This mirrors the reader the deb
//! harness already uses (crates/trawl-server/debian/tests/crashdump-harness.sh
//! lines 459-483), field for field.
//!
//! Layout: header `magic(4) version(4) stream_count(4) directory_rva(4)`,
//! then `stream_count` directory entries of `type(4) size(4) rva(4)`. A stream's
//! payload starts with a u32 count.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

const MAGIC: &[u8; 4] = b"MDMP";
/// `ThreadListStream`, matching the harness reader.
const THREAD_LIST_STREAM: u32 = 3;
/// `MemoryListStream`, matching the harness reader.
const MEMORY_LIST_STREAM: u32 = 5;
/// A real dump has a handful of streams. This bounds the directory walk of a
/// file that is corrupt or not ours.
const MAX_STREAMS: u32 = 4096;

/// What a dump captured. `-1` means the stream is absent entirely, which is a
/// different fact from a stream that is present and says zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MdmpCounts {
    pub(crate) threads: i64,
    pub(crate) memory_regions: i64,
}

/// Positional reads, so the dump can be parsed through the same handle it was
/// written with instead of being reopened by a path an attacker could swap.
pub(crate) trait ReadAt {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

impl ReadAt for File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, offset)
    }
}

impl ReadAt for [u8] {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| truncated())?;
        let end = start.checked_add(buf.len()).ok_or_else(truncated)?;
        let slice = self.get(start..end).ok_or_else(truncated)?;
        buf.copy_from_slice(slice);
        Ok(())
    }
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "minidump ends mid-structure")
}

fn malformed(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// Count the threads and memory regions a dump captured.
pub(crate) fn counts<R: ReadAt + ?Sized>(src: &R) -> io::Result<MdmpCounts> {
    let mut header = [0u8; 16];
    src.read_exact_at(&mut header, 0)?;
    if &header[..4] != MAGIC {
        return Err(malformed("not a minidump: bad magic"));
    }
    let stream_count = u32_at(&header, 8);
    let directory_rva = u32_at(&header, 12);
    if stream_count > MAX_STREAMS {
        return Err(malformed("minidump stream count over cap"));
    }

    let mut found = MdmpCounts {
        threads: -1,
        memory_regions: -1,
    };
    for index in 0..stream_count {
        let mut entry = [0u8; 12];
        src.read_exact_at(&mut entry, u64::from(directory_rva) + u64::from(index) * 12)?;
        let slot = match u32_at(&entry, 0) {
            THREAD_LIST_STREAM => &mut found.threads,
            MEMORY_LIST_STREAM => &mut found.memory_regions,
            _ => continue,
        };
        let mut count = [0u8; 4];
        src.read_exact_at(&mut count, u64::from(u32_at(&entry, 8)))?;
        *slot = i64::from(u32_at(&count, 0));
    }
    Ok(found)
}

/// Little-endian u32 at a fixed offset in a buffer we already read in full.
fn u32_at(buf: &[u8], offset: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&buf[offset..offset + 4]);
    u32::from_le_bytes(word)
}

#[cfg(test)]
mod tests {
    use super::{MEMORY_LIST_STREAM, MdmpCounts, THREAD_LIST_STREAM, counts};

    /// Build a minidump header plus a directory of `(stream_type, count)`
    /// streams, each stream's payload being its u32 count.
    fn dump(streams: &[(u32, u32)]) -> Vec<u8> {
        let dir_rva: u32 = 16;
        let payloads_rva = dir_rva + 12 * u32::try_from(streams.len()).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(b"MDMP");
        out.extend_from_slice(&0xa793u32.to_le_bytes());
        out.extend_from_slice(&u32::try_from(streams.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&dir_rva.to_le_bytes());
        for (i, (kind, _)) in streams.iter().enumerate() {
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&4u32.to_le_bytes());
            out.extend_from_slice(&(payloads_rva + 4 * u32::try_from(i).unwrap()).to_le_bytes());
        }
        for (_, count) in streams {
            out.extend_from_slice(&count.to_le_bytes());
        }
        out
    }

    #[test]
    fn counts_a_populated_dump() {
        let bytes = dump(&[(THREAD_LIST_STREAM, 9), (MEMORY_LIST_STREAM, 431)]);
        assert_eq!(
            counts(bytes.as_slice()).unwrap(),
            MdmpCounts {
                threads: 9,
                memory_regions: 431
            }
        );
    }

    #[test]
    fn counts_the_denied_signature() {
        // The shape a refused PTRACE_ATTACH leaves behind: a valid file that
        // captured nothing.
        let bytes = dump(&[(THREAD_LIST_STREAM, 0), (MEMORY_LIST_STREAM, 0)]);
        assert_eq!(
            counts(bytes.as_slice()).unwrap(),
            MdmpCounts {
                threads: 0,
                memory_regions: 0
            }
        );
    }

    #[test]
    fn an_absent_stream_reports_minus_one() {
        let bytes = dump(&[(THREAD_LIST_STREAM, 4)]);
        assert_eq!(
            counts(bytes.as_slice()).unwrap(),
            MdmpCounts {
                threads: 4,
                memory_regions: -1
            }
        );
        let none = dump(&[(7, 3)]);
        assert_eq!(
            counts(none.as_slice()).unwrap(),
            MdmpCounts {
                threads: -1,
                memory_regions: -1
            }
        );
    }

    #[test]
    fn unrelated_streams_are_skipped() {
        let bytes = dump(&[(7, 1), (THREAD_LIST_STREAM, 2), (15, 99)]);
        assert_eq!(counts(bytes.as_slice()).unwrap().threads, 2);
    }

    #[test]
    fn a_bad_magic_is_an_error() {
        let mut bytes = dump(&[(THREAD_LIST_STREAM, 1)]);
        bytes[..4].copy_from_slice(b"PMDM");
        assert!(counts(bytes.as_slice()).is_err());
    }

    #[test]
    fn a_short_file_is_an_error_not_a_zero() {
        let bytes = dump(&[(THREAD_LIST_STREAM, 1)]);
        for len in [0, 4, 15, bytes.len() - 1] {
            assert!(counts(&bytes[..len]).is_err(), "truncated to {len}");
        }
    }

    #[test]
    fn a_stream_count_over_the_cap_is_an_error() {
        let mut bytes = dump(&[(THREAD_LIST_STREAM, 1)]);
        bytes[8..12].copy_from_slice(&5000u32.to_le_bytes());
        assert!(counts(bytes.as_slice()).is_err());
    }

    #[test]
    fn a_directory_pointing_past_the_end_is_an_error() {
        let mut bytes = dump(&[(THREAD_LIST_STREAM, 1)]);
        bytes[12..16].copy_from_slice(&4096u32.to_le_bytes());
        assert!(counts(bytes.as_slice()).is_err());
    }
}
