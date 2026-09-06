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
//! the designated unsafe file: [`peer_cred_of_fd`] asks the kernel who is on
//! the other end of the socket the dump client itself holds, which is how the
//! parent tells its own monitor from a process that bound the name first.

use std::io;
use std::os::fd::RawFd;

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

/// Ask the kernel to send `sig` to this process when its parent goes away.
///
/// `PR_SET_PDEATHSIG` is armed from the moment of the call and covers nothing
/// before it, so a caller that needs the tie to be airtight has to check its
/// parent separately; see `imp::monitor_main`, which does exactly that.
///
/// Two details that decide where the call belongs. The setting is per-thread,
/// and "parent" means the THREAD that forked this process, not that process's
/// thread group: the signal arrives when that thread exits, even if the rest of
/// the process lives on. It is also cleared in a child of `fork` and after an
/// exec that gains privilege, so each process arms it for itself.
pub(crate) fn set_parent_death_signal(sig: libc::c_int) -> io::Result<()> {
    let sig = libc::c_ulong::try_from(sig)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "signal number is negative"))?;
    // SAFETY: `prctl(PR_SET_PDEATHSIG, sig)` takes scalars only and touches no
    // memory of ours.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, sig) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Ask the kernel who is on the other end of a connected socket.
///
/// `SO_PEERCRED` freezes the peer's credentials when the connection is made,
/// and on the connecting side it reports the process that called `listen(2)` on
/// the socket that accepted this connection. That is the identity worth
/// checking: not who answers a message later, but who received this connection.
///
/// It has to be read on the descriptor the dump client itself holds. A second
/// connection to the same name is a different connection and can reach a
/// different listener; see `imp::client_peer_is_monitor` for the interleaving
/// that makes the two answers disagree.
///
/// The descriptor is only borrowed for the length of the call: this never
/// closes it and never takes ownership, so the caller holding the `Client`
/// alive is what keeps it valid.
// `size_of::<ucred>()` is 12, which fits `socklen_t` with room to spare.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn peer_cred_of_fd(fd: RawFd) -> io::Result<libc::ucred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `getsockopt(SO_PEERCRED)` writes at most `len` bytes into the
    // pointer and updates `len` to what it wrote. Both are live locals of
    // exactly the sizes named. `fd` is borrowed, never closed here, and a
    // descriptor that is closed or is not a socket makes the call fail rather
    // than misbehave.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::getsockopt(
            fd,
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
    use std::io;
    use std::io::BufRead;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use minidumper::{Server, SocketName};

    use super::{
        PTRACE_BIT_WORD0, capget, get_dumpable, peer_cred_of_fd, raise_ptrace_effective, seal,
        set_parent_death_signal, set_ptracer,
    };
    use crate::probe::{parse_status, self_status};

    /// Names the abstract socket [`abstract_socket_bind_helper`] should bind.
    const HELPER_ENV: &str = "TRAWL_CRASHDUMP_TEST_BIND";
    /// Selects the middle process of the parent-death-signal test.
    const PDEATHSIG_MIDDLE_ENV: &str = "TRAWL_CRASHDUMP_TEST_PDEATHSIG_MIDDLE";
    /// Selects the process that arms the signal.
    const PDEATHSIG_CHILD_ENV: &str = "TRAWL_CRASHDUMP_TEST_PDEATHSIG_CHILD";
    /// Printed by that process once armed, followed by its pid.
    const PDEATHSIG_ARMED: &str = "pdeathsig-armed";

    /// A name no other test, run or process is using.
    fn unique_name(tag: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after the epoch")
            .as_nanos();
        format!("trawld-crashdump-test-{tag}-{}-{nanos}", std::process::id())
    }

    /// The address of an abstract name: a leading NUL byte, then the name, with
    /// the length passed explicitly rather than read up to a terminator.
    ///
    /// The production code no longer builds one of these. It reads credentials
    /// off a descriptor `minidumper` opened, and these tests need a descriptor
    /// of their own to read.
    // Three casts to fixed-width kernel types, each of a value that provably
    // fits: `AF_UNIX` is 1, a `u8` byte into `c_char` (unsigned on aarch64,
    // signed on x86-64, and only the bit pattern reaches the kernel), and an
    // address length under 128.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    fn abstract_addr(name: &str) -> (libc::sockaddr_un, libc::socklen_t) {
        let bytes = name.as_bytes();
        let mut addr = libc::sockaddr_un {
            sun_family: libc::AF_UNIX as libc::sa_family_t,
            sun_path: [0; 108],
        };
        assert!(
            !bytes.is_empty() && bytes.len() < addr.sun_path.len(),
            "the name fits sun_path"
        );
        for (slot, &byte) in addr.sun_path[1..=bytes.len()].iter_mut().zip(bytes) {
            *slot = byte as libc::c_char;
        }
        let len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + bytes.len())
            as libc::socklen_t;
        (addr, len)
    }

    fn seqpacket_socket() -> OwnedFd {
        // SAFETY: `socket(2)` takes scalars only and touches no memory of ours.
        // The descriptor is handed straight to `OwnedFd`, which closes it.
        #[allow(unsafe_code)]
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0, "socket: {}", io::Error::last_os_error());
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        #[allow(unsafe_code)]
        unsafe {
            OwnedFd::from_raw_fd(raw)
        }
    }

    /// Connect to an abstract seqpacket name, exactly as `minidumper`'s client
    /// does, and hand back the descriptor so a test can read its peer.
    fn connect_abstract(name: &str) -> io::Result<OwnedFd> {
        let (addr, len) = abstract_addr(name);
        let fd = seqpacket_socket();
        // SAFETY: `connect(2)` reads `len` bytes of the address and writes
        // nothing back; `addr` is a live `#[repr(C)]` local and `len` is within
        // it by construction.
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
                len,
            )
        };
        if rc == 0 {
            Ok(fd)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Bind and listen on an abstract name, the way an impostor would.
    fn bind_listen(name: &str) -> OwnedFd {
        let (addr, len) = abstract_addr(name);
        let fd = seqpacket_socket();
        // SAFETY: `bind(2)` reads `len` bytes of the address and writes nothing
        // back; the same live local as above.
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
                len,
            )
        };
        assert_eq!(rc, 0, "bind: {}", io::Error::last_os_error());
        // SAFETY: `listen(2)` takes scalars only.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::listen(fd.as_raw_fd(), 8) };
        assert_eq!(rc, 0, "listen: {}", io::Error::last_os_error());
        fd
    }

    /// Take one pending connection off a listener, so closing the listener
    /// frees the NAME while the connection stays up.
    fn accept_one(listener: &OwnedFd) -> OwnedFd {
        // SAFETY: `accept(2)` with null address arguments writes nothing of
        // ours; the returned descriptor is owned by the `OwnedFd`.
        #[allow(unsafe_code)]
        let raw = unsafe {
            libc::accept(
                listener.as_raw_fd(),
                std::ptr::null_mut::<libc::sockaddr>(),
                std::ptr::null_mut::<libc::socklen_t>(),
            )
        };
        assert!(raw >= 0, "accept: {}", io::Error::last_os_error());
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        #[allow(unsafe_code)]
        unsafe {
            OwnedFd::from_raw_fd(raw)
        }
    }

    /// Re-exec this test binary as a process whose only job is to bind `name`.
    fn spawn_bind_helper(name: &str) -> Child {
        Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--ignored",
                "--exact",
                "caps::tests::abstract_socket_bind_helper",
            ])
            .env(HELPER_ENV, name)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("respawn the test binary as a bind helper")
    }

    /// Re-exec this test binary as one named ignored test, with its output
    /// uncaptured so a helper can talk to the test over its stdout.
    fn respawn_ignored(test: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test binary path"));
        command.args(["--ignored", "--exact", "--nocapture", test]);
        command
    }

    /// Is `pid` a live process, as opposed to gone or an unreaped zombie?
    ///
    /// A zombie still answers `kill(pid, 0)`, and whether an orphan is reaped
    /// promptly depends on which ancestor happens to be a subreaper, so the
    /// state letter in `/proc/<pid>/stat` is the answer that does not depend on
    /// the harness's process tree. The comm field is parenthesised and may
    /// itself contain spaces and parentheses, so the state is read after the
    /// LAST `)`.
    fn process_is_live(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some((_, tail)) = stat.rsplit_once(')') else {
            return false;
        };
        !matches!(tail.split_whitespace().next(), None | Some("Z" | "X"))
    }

    /// Connect to `name` until the peer of the connection is `pid`, and return
    /// the credentials that proved it.
    fn peer_cred_when_bound_by(name: &str, pid: u32) -> Option<libc::ucred> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(conn) = connect_abstract(name)
                && let Ok(cred) = peer_cred_of_fd(conn.as_raw_fd())
                && u32::try_from(cred.pid) == Ok(pid)
            {
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
        let conn = connect_abstract(&name).expect("connect to our own listener");

        let cred = peer_cred_of_fd(conn.as_raw_fd()).expect("peer credentials");

        assert_eq!(
            u32::try_from(cred.pid).expect("a pid is positive"),
            std::process::id(),
            "SO_PEERCRED names the binder"
        );
        let status = self_status().expect("/proc/self/status");
        assert_eq!(cred.uid, status.uid[1], "effective uid");
        assert_eq!(cred.gid, status.gid[1], "effective gid");
    }

    /// The ordinary case with a real second process: the pid the kernel reports
    /// on the connection is the one that BOUND the name, never the one that
    /// connected. That is what lets `install` tell its own monitor from a
    /// stranger who won the race for a predictable name.
    #[test]
    fn peer_cred_reports_the_other_process_that_bound_the_name() {
        let name = unique_name("impostor");
        let mut helper = spawn_bind_helper(&name);

        let cred = peer_cred_when_bound_by(&name, helper.id());
        let _ = helper.kill();
        let _ = helper.wait();

        let cred = cred.expect("the helper bound the name within the deadline");
        let peer = u32::try_from(cred.pid).expect("a pid is positive");
        assert_eq!(peer, helper.id(), "SO_PEERCRED names the helper");
        assert_ne!(peer, std::process::id(), "and not the connecting process");
    }

    /// The attack the descriptor accounting exists for, played out end to end.
    ///
    /// An impostor binds the predictable name first, accepts the connection the
    /// dump client makes, and then closes only its LISTENER. That frees the
    /// name, so the real monitor's own bind succeeds a moment later. From then
    /// on a FRESH connection to the name reaches the real monitor and reports
    /// its credentials, while the connection the client is actually holding
    /// still belongs to the impostor. Reading the credentials of a second
    /// connection would pass this; reading them off the client's own descriptor
    /// catches it.
    #[test]
    fn peer_cred_separates_the_held_connection_from_a_later_binder() {
        let name = unique_name("interleave");

        let listener = bind_listen(&name);
        let held = connect_abstract(&name).expect("connect to the impostor's listener");
        let _accepted = accept_one(&listener);
        drop(listener);

        let mut helper = spawn_bind_helper(&name);
        let probe = peer_cred_when_bound_by(&name, helper.id());
        let held_cred = peer_cred_of_fd(held.as_raw_fd());
        let _ = helper.kill();
        let _ = helper.wait();

        assert!(
            probe.is_some(),
            "the helper took the name the impostor released"
        );
        let held_cred = held_cred.expect("credentials of the connection we hold");
        let peer = u32::try_from(held_cred.pid).expect("a pid is positive");
        assert_eq!(
            peer,
            std::process::id(),
            "the held connection still belongs to the impostor"
        );
        assert_ne!(
            peer,
            helper.id(),
            "and never to the process that bound later"
        );
    }

    /// Runs only when [`peer_cred_reports_the_other_process_that_bound_the_name`]
    /// or [`peer_cred_separates_the_held_connection_from_a_later_binder`]
    /// re-execs the test binary with `--ignored --exact` and the name to bind in
    /// the environment. Ignored so an ordinary run never pays the sleep.
    #[test]
    #[ignore = "helper process for the peer_cred tests"]
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
    fn arming_the_parent_death_signal_succeeds() {
        // Arming it in the test process itself is harmless: the parent is the
        // harness, and if the harness died this process would be killed off
        // anyway.
        set_parent_death_signal(libc::SIGTERM).expect("PR_SET_PDEATHSIG");
    }

    #[test]
    fn a_negative_signal_number_is_refused_without_a_syscall() {
        assert_eq!(
            set_parent_death_signal(-1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// The mechanism the monitor's lifetime tie rests on, with real processes:
    /// a process that armed `PR_SET_PDEATHSIG` dies when its parent does, even
    /// though nothing it holds is closed and nobody signals it directly.
    ///
    /// Three processes. This test spawns a MIDDLE helper, which spawns a
    /// GRANDCHILD that arms the signal and prints its pid on the stdout it
    /// inherited. Printing after arming is the handshake: without it the test
    /// could kill the middle process before the `prctl` landed and prove
    /// nothing. Then the middle process is killed, and the grandchild has to go
    /// away on its own.
    #[test]
    fn a_child_that_armed_the_signal_dies_with_its_parent() {
        let mut middle = respawn_ignored("caps::tests::pdeathsig_middle_helper")
            .env(PDEATHSIG_MIDDLE_ENV, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("respawn the test binary as a pdeathsig helper");

        let stdout = middle.stdout.take().expect("piped stdout");
        let armed = io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find_map(|line| {
                line.strip_prefix(PDEATHSIG_ARMED)
                    .and_then(|rest| rest.trim().parse::<u32>().ok())
            });

        let armed = armed.expect("the grandchild armed the signal and named itself");
        assert!(process_is_live(armed), "the grandchild is running");

        let _ = middle.kill();
        let _ = middle.wait();

        let deadline = Instant::now() + Duration::from_secs(5);
        while process_is_live(armed) {
            assert!(
                Instant::now() < deadline,
                "pid {armed} outlived its parent by more than the deadline"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The middle process of [`a_child_that_armed_the_signal_dies_with_its_parent`]:
    /// spawn the grandchild, then do nothing until the test kills us.
    #[test]
    #[ignore = "helper process for the parent-death-signal test"]
    fn pdeathsig_middle_helper() {
        if std::env::var_os(PDEATHSIG_MIDDLE_ENV).is_none() {
            return;
        }
        // Stdout is inherited, so the grandchild writes to the test's pipe.
        let mut grandchild = respawn_ignored("caps::tests::pdeathsig_grandchild_helper")
            .env(PDEATHSIG_CHILD_ENV, "1")
            .stderr(Stdio::null())
            .spawn()
            .expect("respawn the test binary as a pdeathsig grandchild");
        // The test kills this process; the sleep only bounds an orphan if it
        // never does.
        std::thread::sleep(Duration::from_secs(30));
        let _ = grandchild.kill();
        let _ = grandchild.wait();
    }

    /// The process under test: arm the signal, say so, then wait to be killed
    /// by the kernel rather than by anyone.
    #[test]
    #[ignore = "helper process for the parent-death-signal test"]
    fn pdeathsig_grandchild_helper() {
        if std::env::var_os(PDEATHSIG_CHILD_ENV).is_none() {
            return;
        }
        set_parent_death_signal(libc::SIGTERM).expect("PR_SET_PDEATHSIG");
        println!("{PDEATHSIG_ARMED} {}", std::process::id());
        io::Write::flush(&mut io::stdout()).expect("flush the handshake");
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn peer_cred_of_something_that_is_not_a_socket_is_an_error() {
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        assert!(peer_cred_of_fd(file.as_raw_fd()).is_err());
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
