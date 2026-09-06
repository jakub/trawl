//! The capability transitions crash-dump capture depends on (ADR-0023).
//!
//! The image stamps `cap_sys_ptrace+p` on `trawld`: permitted, never effective,
//! so an ordinary exec of the binary still works under a dropped bounding set.
//! Two processes then move in opposite directions. The monitor raises the bit
//! into its effective set so it may ptrace a crashed daemon under
//! `kernel.yama.ptrace_scope=2`. The daemon drops the bit from both sets and
//! sets `no_new_privs`, which closes the route back to the file capability
//! through a later exec.
//!
//! `libc` 0.2 has the syscall numbers but no `capset` wrapper and no capability
//! structs, so the three `cap_user_*` types are declared here and both calls go
//! through `libc::syscall`. This is the only file in the crate that touches
//! capabilities, and every unsafe block is a single syscall.

use std::io;

use crate::readiness::CAP_SYS_PTRACE_BIT;

/// `_LINUX_CAPABILITY_VERSION_3`: 64-bit masks split across two 32-bit words.
const CAP_VERSION_3: u32 = 0x2008_0522;
/// `CAP_SYS_PTRACE` is bit 19, so it lives in word 0 of the pair.
const PTRACE_BIT_WORD0: u32 = 1 << CAP_SYS_PTRACE_BIT;

/// `cap_user_header_t`.
#[repr(C)]
#[derive(Clone, Copy)]
struct CapHeader {
    version: u32,
    /// 0 means "this process", which is the only target we ever ask about.
    pid: libc::c_int,
}

/// `cap_user_data_t`. Version 3 passes an array of two of these.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Read this process's capability sets.
fn capget() -> io::Result<[CapData; 2]> {
    let mut header = CapHeader {
        version: CAP_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData::default(); 2];
    // SAFETY: `capget(2)` reads the header and writes exactly two
    // `cap_user_data_t` when the header names version 3. Both pointers are to
    // live, correctly sized `#[repr(C)]` locals owned by this frame.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_capget,
            std::ptr::addr_of_mut!(header),
            data.as_mut_ptr(),
        )
    };
    if rc == 0 {
        Ok(data)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Write this process's capability sets back.
fn capset(data: &[CapData; 2]) -> io::Result<()> {
    let mut header = CapHeader {
        version: CAP_VERSION_3,
        pid: 0,
    };
    // SAFETY: `capset(2)` reads the header and exactly two `cap_user_data_t`
    // and writes nothing back. Both pointers are to live, correctly sized
    // `#[repr(C)]` locals; the data pointer is const-cast only because the
    // syscall wrapper is variadic.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_capset,
            std::ptr::addr_of_mut!(header),
            data.as_ptr(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Monitor side: move `CAP_SYS_PTRACE` from permitted to effective.
///
/// Every other bit in both words is carried across untouched, because the state
/// is read first and edited, never synthesized from the one mask we care about.
pub(crate) fn raise_ptrace_effective() -> io::Result<()> {
    let mut data = capget()?;
    if data[0].permitted & PTRACE_BIT_WORD0 == 0 {
        // Report this ourselves rather than letting capset answer EPERM: the
        // permitted set is empty exactly when the file capability was ignored
        // (no_new_privs, a stripped xattr, a dropped bounding set), which is
        // the case an operator needs to recognise.
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cap_sys_ptrace is not in the permitted set",
        ));
    }
    if data[0].effective & PTRACE_BIT_WORD0 != 0 {
        return Ok(());
    }
    data[0].effective |= PTRACE_BIT_WORD0;
    capset(&data)
}

/// Daemon side: drop `CAP_SYS_PTRACE` and close the door behind it (ADR-0023
/// ruling 4).
///
/// Clearing permitted also drops the bit from the ambient set, which is how the
/// deb channel's `AmbientCapabilities=` grant is given back. Both halves are
/// attempted even when the first fails: a daemon that kept the capability must
/// at least not be able to re-acquire it, and the other way round. The error
/// names the step that failed.
///
/// Consequence: `trawld` may exec nothing after the monitor is spawned.
pub(crate) fn seal() -> Result<(), (&'static str, io::Error)> {
    let dropped = drop_ptrace_capability();
    let no_new_privs = set_no_new_privs();
    match (dropped, no_new_privs) {
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(("prctl_no_new_privs", err)),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn drop_ptrace_capability() -> Result<(), (&'static str, io::Error)> {
    let mut data = capget().map_err(|err| ("capget", err))?;
    data[0].effective &= !PTRACE_BIT_WORD0;
    data[0].permitted &= !PTRACE_BIT_WORD0;
    capset(&data).map_err(|err| ("capset", err))
}

fn set_no_new_privs() -> io::Result<()> {
    // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)` takes scalars only and
    // touches no memory of ours.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Name the monitor as the one process allowed to ptrace us.
///
/// `crash_handler` issues this lazily from inside the signal handler, where a
/// failure is invisible. Calling it at startup with the return checked is what
/// makes the yama scope-1 branch of the readiness verdict an observation rather
/// than an assumption.
pub(crate) fn set_ptracer(pid: u32) -> io::Result<()> {
    // SAFETY: `prctl(PR_SET_PTRACER, pid)` takes scalars only and touches no
    // memory of ours.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::prctl(libc::PR_SET_PTRACER, libc::c_ulong::from(pid)) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Is this process dumpable in the sense `__ptrace_may_access` requires?
///
/// Only `SUID_DUMP_USER` (1) counts. `SUID_DUMP_DISABLE` (0) is what an exec of
/// a file-capable binary leaves behind, and `SUID_DUMP_ROOT` (2) admits root
/// only, which a same-uid monitor is not.
pub(crate) fn get_dumpable() -> io::Result<bool> {
    // SAFETY: `prctl(PR_GET_DUMPABLE)` takes no arguments beyond the option and
    // returns the value; it touches no memory of ours.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::prctl(libc::PR_GET_DUMPABLE) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PTRACE_BIT_WORD0, capget, get_dumpable, raise_ptrace_effective, seal, set_ptracer,
    };
    use crate::probe::{parse_status, self_status};

    fn masks() -> (u64, u64) {
        let data = capget().expect("capget on our own process");
        let join = |lo: u32, hi: u32| u64::from(lo) | (u64::from(hi) << 32);
        (
            join(data[0].effective, data[1].effective),
            join(data[0].permitted, data[1].permitted),
        )
    }

    #[test]
    fn capget_agrees_with_proc_status() {
        let (effective, permitted) = masks();
        let status = self_status().expect("/proc/self/status");
        assert_eq!(effective, status.eff, "CapEff");
        assert_eq!(permitted, status.prm, "CapPrm");
    }

    #[test]
    fn an_ordinary_process_is_dumpable() {
        assert!(get_dumpable().unwrap());
    }

    #[test]
    fn set_ptracer_accepts_a_pid() {
        set_ptracer(std::process::id()).expect("PR_SET_PTRACER on our own pid");
    }

    #[test]
    fn raising_succeeds_exactly_when_the_bit_is_permitted() {
        let permitted = capget().unwrap()[0].permitted & PTRACE_BIT_WORD0 != 0;
        assert_eq!(raise_ptrace_effective().is_ok(), permitted);
    }

    /// Mutates this process, so it relies on nextest running each test in its
    /// own process. Nothing here execs afterwards, which is the only thing
    /// `no_new_privs` would change.
    ///
    /// Both the capability sets and `no_new_privs` are per-THREAD, and this
    /// test body runs on a spawned harness thread while `/proc/self/status`
    /// reports the main thread. Hence `/proc/thread-self`. In `trawld` the seal
    /// happens on the main thread before any other exists, which is one more
    /// reason `init()` is the first statement of `main`.
    #[test]
    fn sealing_clears_the_bit_and_forbids_regaining_it() {
        seal().expect("seal an unprivileged process");
        let text = std::fs::read_to_string("/proc/thread-self/status").expect("thread status");
        let status = parse_status(&text).expect("thread status parses");
        let mask = u64::from(PTRACE_BIT_WORD0);
        assert_eq!(status.eff & mask, 0, "CapEff {:x}", status.eff);
        assert_eq!(status.prm & mask, 0, "CapPrm {:x}", status.prm);
        assert!(status.no_new_privs);
        assert!(raise_ptrace_effective().is_err());
    }
}
