//! Reading the kernel's account of the two processes.
//!
//! Everything here is a read of a `procfs` text file. Nothing is inferred: a
//! field that is missing, duplicated or malformed yields `None`, which the
//! classifier turns into `indeterminate` rather than a denial.

use std::io::ErrorKind;

use crate::readiness::{MonitorStatus, SelfStatus};

const YAMA_PTRACE_SCOPE: &str = "/proc/sys/kernel/yama/ptrace_scope";

/// The six `/proc/<pid>/status` fields that decide whether a ptrace can happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatusFields {
    pub(crate) eff: u64,
    pub(crate) prm: u64,
    pub(crate) bnd: u64,
    pub(crate) no_new_privs: bool,
    pub(crate) uid: [u32; 4],
    pub(crate) gid: [u32; 4],
}

/// Read the monitor's status. `None` when the file is gone (the monitor exited)
/// or does not parse.
pub(crate) fn monitor_status(pid: u32) -> Option<MonitorStatus> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let f = parse_status(&text)?;
    Some(MonitorStatus {
        eff: f.eff,
        prm: f.prm,
        bnd: f.bnd,
        no_new_privs: f.no_new_privs,
        uid: f.uid,
        gid: f.gid,
    })
}

/// Read our own status through the same parser.
pub(crate) fn self_status() -> Option<SelfStatus> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let f = parse_status(&text)?;
    Some(SelfStatus {
        eff: f.eff,
        prm: f.prm,
        bnd: f.bnd,
        no_new_privs: f.no_new_privs,
        uid: f.uid,
        gid: f.gid,
    })
}

/// Read `kernel.yama.ptrace_scope`.
///
/// A missing file means yama is not built into this kernel, which is exactly
/// classic scope-0 semantics (any process may trace a same-uid target), so that
/// case reports `Some(0)`. Every other error, an unparseable body, or a value
/// the classifier has no row for reports `None` and lands as `indeterminate`.
pub(crate) fn ptrace_scope() -> Option<u8> {
    match std::fs::read_to_string(YAMA_PTRACE_SCOPE) {
        Ok(text) => match text.trim().parse::<u8>() {
            Ok(scope @ 0..=3) => Some(scope),
            _ => None,
        },
        Err(err) if err.kind() == ErrorKind::NotFound => Some(0),
        Err(_) => None,
    }
}

/// Parse a `/proc/<pid>/status` body.
///
/// Fields may arrive in any order and unknown ones are ignored, but each of the
/// six we need must appear exactly once with a well-formed value.
pub(crate) fn parse_status(text: &str) -> Option<StatusFields> {
    let mut eff = None;
    let mut prm = None;
    let mut bnd = None;
    let mut no_new_privs = None;
    let mut uid = None;
    let mut gid = None;

    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name {
            "CapEff" => set(&mut eff, parse_hex_u64(value)?)?,
            "CapPrm" => set(&mut prm, parse_hex_u64(value)?)?,
            "CapBnd" => set(&mut bnd, parse_hex_u64(value)?)?,
            "NoNewPrivs" => set(&mut no_new_privs, parse_flag(value)?)?,
            "Uid" => set(&mut uid, parse_ids(value)?)?,
            "Gid" => set(&mut gid, parse_ids(value)?)?,
            _ => {}
        }
    }

    Some(StatusFields {
        eff: eff?,
        prm: prm?,
        bnd: bnd?,
        no_new_privs: no_new_privs?,
        uid: uid?,
        gid: gid?,
    })
}

/// Fill a slot, refusing a second value for the same field. Two `CapEff` lines
/// mean the text is not what we think it is, and picking one would be a guess.
fn set<T>(slot: &mut Option<T>, value: T) -> Option<()> {
    if slot.is_some() {
        return None;
    }
    *slot = Some(value);
    Some(())
}

/// Capability masks are bare lowercase or uppercase hex. `from_str_radix` alone
/// would accept a leading sign, so the digits are checked first.
fn parse_hex_u64(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(value, 16).ok()
}

fn parse_flag(value: &str) -> Option<bool> {
    match value {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// `Uid`/`Gid` carry exactly four ids: real, effective, saved, filesystem.
fn parse_ids(value: &str) -> Option<[u32; 4]> {
    let mut ids = [0u32; 4];
    let mut fields = value.split_whitespace();
    for slot in &mut ids {
        *slot = parse_u32(fields.next()?)?;
    }
    if fields.next().is_some() {
        return None;
    }
    Some(ids)
}

fn parse_u32(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{StatusFields, parse_status, ptrace_scope, self_status};

    /// Shaped like the real thing: tab after the colon, unrelated fields around
    /// the ones we read, trailing newline.
    const SAMPLE: &str = "Name:\ttrawld\nUmask:\t0022\nState:\tS (sleeping)\nTgid:\t7\nPid:\t7\n\
PPid:\t1\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\nFDSize:\t256\n\
Threads:\t9\nCapInh:\t0000000000000000\nCapPrm:\t0000000000080000\n\
CapEff:\t0000000000080000\nCapBnd:\t000001ffffffffff\nCapAmb:\t0000000000000000\n\
NoNewPrivs:\t1\nSeccomp:\t2\nSpeculation_Store_Bypass:\tthread vulnerable\n";

    fn sample_fields() -> StatusFields {
        StatusFields {
            eff: 0x0008_0000,
            prm: 0x0008_0000,
            bnd: 0x0000_01ff_ffff_ffff,
            no_new_privs: true,
            uid: [1000; 4],
            gid: [1000; 4],
        }
    }

    #[test]
    fn parses_a_real_shaped_status() {
        assert_eq!(parse_status(SAMPLE), Some(sample_fields()));
    }

    #[test]
    fn field_order_does_not_matter() {
        let mut lines: Vec<&str> = SAMPLE.lines().collect();
        lines.reverse();
        assert_eq!(parse_status(&lines.join("\n")), Some(sample_fields()));
    }

    #[test]
    fn a_missing_required_field_is_unreadable() {
        for name in ["Uid", "Gid", "CapPrm", "CapEff", "CapBnd", "NoNewPrivs"] {
            let text: String = SAMPLE
                .lines()
                .filter(|l| !l.starts_with(&format!("{name}:")))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(parse_status(&text), None, "without {name}");
        }
    }

    #[test]
    fn a_duplicate_required_field_is_unreadable() {
        let text = format!("{SAMPLE}CapEff:\t0000000000000000\n");
        assert_eq!(parse_status(&text), None);
    }

    #[test]
    fn a_prefixed_or_signed_mask_is_unreadable() {
        for value in ["0x80000", "+80000", "-80000", "", "8000g"] {
            let text = SAMPLE.replace("CapEff:\t0000000000080000", &format!("CapEff:\t{value}"));
            assert_eq!(parse_status(&text), None, "CapEff {value:?}");
        }
    }

    #[test]
    fn a_mask_wider_than_u64_is_unreadable() {
        let text = SAMPLE.replace("CapBnd:\t000001ffffffffff", "CapBnd:\t1ffffffffffffffff");
        assert_eq!(parse_status(&text), None);
    }

    #[test]
    fn uppercase_hex_parses() {
        let text = SAMPLE.replace("CapBnd:\t000001ffffffffff", "CapBnd:\t000001FFFFFFFFFF");
        assert_eq!(parse_status(&text), Some(sample_fields()));
    }

    #[test]
    fn no_new_privs_is_exactly_zero_or_one() {
        let zero = SAMPLE.replace("NoNewPrivs:\t1", "NoNewPrivs:\t0");
        assert_eq!(
            parse_status(&zero),
            Some(StatusFields {
                no_new_privs: false,
                ..sample_fields()
            })
        );
        for value in ["2", "true", "01", ""] {
            let text = SAMPLE.replace("NoNewPrivs:\t1", &format!("NoNewPrivs:\t{value}"));
            assert_eq!(parse_status(&text), None, "NoNewPrivs {value:?}");
        }
    }

    #[test]
    fn ids_are_exactly_four_decimals() {
        for value in [
            "1000\t1000\t1000",
            "1000\t1000\t1000\t1000\t1000",
            "1000\t1000\t1000\t-1",
            "1000\t1000\t1000\t+1",
            "1000\t1000\t1000\t4294967296",
        ] {
            let text = SAMPLE.replace("Uid:\t1000\t1000\t1000\t1000", &format!("Uid:\t{value}"));
            assert_eq!(parse_status(&text), None, "Uid {value:?}");
        }
        let mixed = SAMPLE.replace("Gid:\t1000\t1000\t1000\t1000", "Gid:\t0 1000  33\t33");
        assert_eq!(
            parse_status(&mixed),
            Some(StatusFields {
                gid: [0, 1000, 33, 33],
                ..sample_fields()
            })
        );
    }

    #[test]
    fn a_field_name_must_match_whole() {
        let text = SAMPLE.replace("CapEff:", "CapEffective:");
        assert_eq!(parse_status(&text), None);
    }

    #[test]
    fn reads_our_own_status_from_procfs() {
        let status = self_status().expect("/proc/self/status is readable and well formed");
        // Whatever this test runner's credentials are, the four uid slots are
        // real numbers and the parser round-trips them.
        assert_eq!(status.uid.len(), 4);
    }

    #[test]
    fn reads_the_yama_scope_or_defaults_to_zero() {
        assert!(matches!(ptrace_scope(), Some(0..=3)));
    }
}
