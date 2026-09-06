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
//!
//! It is also where the crate's other raw-syscall work lives, because this is
//! the designated unsafe file: [`peer_cred_of_abstract_socket`] asks the kernel
//! who is listening on the monitor's abstract socket, which is how the parent
//! tells its own monitor from a process that bound the name first.

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

/// Ask the kernel which process is listening on an abstract socket name.
///
/// A successful `Client::with_name` proves only that SOMETHING is bound to the
/// name. Abstract names carry no filesystem permissions and this one is derived
/// from the daemon's pid, so any process in the same network namespace can bind
/// it first; the real monitor's own bind then fails and it exits, while the
/// parent goes on probing `/proc/<monitor>` and reporting a readiness that
/// describes a process it is not talking to.
///
/// `SO_PEERCRED` is the kernel's own answer to "who is on the other end". For a
/// connecting socket it reports the credentials captured when the peer called
/// `listen`, so it names the process that actually holds the name, and a
/// caller who compares that pid against the child it spawned cannot be fooled
/// by a stranger who won the race.
///
/// The connection this makes is a throwaway: it is opened, asked one question
/// and closed. `SOCK_SEQPACKET` because that is what `minidumper`'s server
/// binds; a `SOCK_STREAM` connect to the same name is refused.
// Three casts to fixed-width kernel types, each of a value that provably fits:
// `AF_UNIX` is 1, a `u8` byte into `c_char` (unsigned on aarch64, signed on
// x86-64, and only the bit pattern reaches the kernel), and `size_of::<ucred>()`
// is 12.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
pub(crate) fn peer_cred_of_abstract_socket(name: &[u8]) -> io::Result<libc::ucred> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let mut addr = libc::sockaddr_un {
        sun_family: libc::AF_UNIX as libc::sa_family_t,
        sun_path: [0; 108],
    };
    // An abstract address is a leading NUL byte, then the name, with the length
    // passed explicitly rather than read up to a terminator.
    if name.is_empty() || name.len() + 1 > addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "abstract socket name does not fit sun_path",
        ));
    }
    for (slot, &byte) in addr.sun_path[1..=name.len()].iter_mut().zip(name) {
        *slot = byte as libc::c_char;
    }
    let addr_len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len()) as libc::socklen_t;

    // SAFETY: `socket(2)` takes scalars only and touches no memory of ours. The
    // returned descriptor is handed straight to `OwnedFd`, which closes it on
    // every path out of this function.
    #[allow(unsafe_code)]
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor this function owns and never
    // duplicates or closes itself.
    #[allow(unsafe_code)]
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    // SAFETY: `connect(2)` reads `addr_len` bytes of the address and writes
    // nothing back. `addr` is a live, correctly sized `#[repr(C)]` local and
    // `addr_len` is within it by the length check above.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
            addr_len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `getsockopt(SO_PEERCRED)` writes at most `len` bytes into the
    // pointer and updates `len` to what it wrote. Both are live locals of
    // exactly the sizes named.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast::<libc::c_void>(),
            std::ptr::addr_of_mut!(len),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned a short credential",
        ));
    }
    if cred.pid == 0 {
        // The peer's pid does not translate into our pid namespace, so there is
        // no identity here to compare against.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED carries no pid",
        ));
    }
    Ok(cred)
}

/// This process's effective uid, for comparison against a peer's.
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: `geteuid(2)` takes no arguments, touches no memory of ours and
    // cannot fail.
    #[allow(unsafe_code)]
    unsafe {
        libc::geteuid()
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
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use minidumper::{Server, SocketName};

    use super::{
        PTRACE_BIT_WORD0, capget, get_dumpable, peer_cred_of_abstract_socket,
        raise_ptrace_effective, seal, set_ptracer,
    };
    use crate::probe::{parse_status, self_status};

    /// Names the abstract socket [`abstract_socket_bind_helper`] should bind.
    const HELPER_ENV: &str = "TRAWL_CRASHDUMP_TEST_BIND";

    /// A name no other test, run or process is using.
    fn unique_name(tag: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after the epoch")
            .as_nanos();
        format!("trawld-crashdump-test-{tag}-{}-{nanos}", std::process::id())
    }

    /// Poll the name until something is listening on it (or give up).
    fn peer_cred_when_bound(name: &str) -> Option<libc::ucred> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(cred) = peer_cred_of_abstract_socket(name.as_bytes()) {
                return Some(cred);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

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

    #[test]
    fn peer_cred_reports_this_process_when_it_holds_the_name() {
        let name = unique_name("self");
        let _server = Server::with_name(SocketName::abstract_namespace(&name))
            .expect("bind an abstract seqpacket listener");

        let cred = peer_cred_of_abstract_socket(name.as_bytes()).expect("peer credentials");

        assert_eq!(
            u32::try_from(cred.pid).expect("a pid is positive"),
            std::process::id(),
            "SO_PEERCRED names the binder"
        );
        let status = self_status().expect("/proc/self/status");
        assert_eq!(cred.uid, status.uid[1], "effective uid");
        assert_eq!(cred.gid, status.gid[1], "effective gid");
    }

    /// The impostor case, with a real second process: the pid the kernel
    /// reports is the one that BOUND the name, never the one that connected.
    /// That is the whole reason `install` can tell its own monitor from a
    /// stranger who won the race for a predictable name.
    #[test]
    fn peer_cred_reports_the_other_process_that_bound_the_name() {
        let name = unique_name("impostor");
        let mut helper = Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--ignored",
                "--exact",
                "caps::tests::abstract_socket_bind_helper",
            ])
            .env(HELPER_ENV, &name)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("respawn the test binary as a bind helper");

        let cred = peer_cred_when_bound(&name);
        let _ = helper.kill();
        let _ = helper.wait();
        let cred = cred.expect("the helper bound the name within the deadline");

        let peer = u32::try_from(cred.pid).expect("a pid is positive");
        assert_eq!(peer, helper.id(), "SO_PEERCRED names the helper");
        assert_ne!(peer, std::process::id(), "and not the connecting process");
    }

    /// Runs only when [`peer_cred_reports_the_other_process_that_bound_the_name`]
    /// re-execs the test binary with `--ignored --exact` and the name to bind in
    /// the environment. Ignored so an ordinary run never pays the sleep.
    #[test]
    #[ignore = "helper process for peer_cred_reports_the_other_process_that_bound_the_name"]
    fn abstract_socket_bind_helper() {
        let Ok(name) = std::env::var(HELPER_ENV) else {
            return;
        };
        let _server = Server::with_name(SocketName::abstract_namespace(&name))
            .expect("bind an abstract seqpacket listener");
        // The parent kills this process as soon as it has read the credentials;
        // the sleep only bounds an orphan if it never does.
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn peer_cred_of_an_unbound_name_is_an_error() {
        let name = unique_name("unbound");
        assert!(peer_cred_of_abstract_socket(name.as_bytes()).is_err());
    }

    #[test]
    fn peer_cred_refuses_a_name_that_cannot_fit_an_abstract_address() {
        assert!(peer_cred_of_abstract_socket(b"").is_err());
        assert!(peer_cred_of_abstract_socket(&[b'x'; 200]).is_err());
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
