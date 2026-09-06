//! Linux crash-dump implementation.
//!
//! A process cannot ptrace its own thread group, so the minidump is written by
//! a separate *monitor* process. We obtain that monitor by re-execing the
//! current binary with [`MONITOR_ENV`] set (re-exec rather than bare `fork`,
//! because `trawl-engine` links libduckdb and a `-sys` dependency could spawn a
//! thread from a static initializer before `main` runs — bare `fork` of an
//! already-multithreaded process risks deadlock on an inherited locked mutex;
//! re-exec resets to a clean single-threaded image).
//!
//! Lifecycle:
//! - parent: spawn the monitor, connect a [`Client`], probe what a crash would
//!   actually be allowed to do, drop `CAP_SYS_PTRACE`, install a
//!   [`CrashHandler`].
//! - on a fatal signal: emit a signal-safe stderr breadcrumb, call
//!   `request_dump` (which blocks until the monitor has written the dump and
//!   acked), then return `Handled(true)` so the instruction re-faults and the
//!   process dies with the original signal (exit 139 for `SIGSEGV`).
//! - the monitor exits when the client disconnects (parent died or shut down).

use std::fs::{DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crash_handler::{CrashContext, CrashEvent, CrashEventResult, CrashHandler};
use minidumper::{
    Client, Error as MdError, LoopAction, MinidumpBinary, Server, ServerHandler, SocketName,
};

use crate::readiness::{FailureReason, ProbeInputs, Readiness, classify, credentials_match};
use crate::{caps, mdmp, probe};

/// Set on the re-exec'd monitor process to select monitor mode.
const MONITOR_ENV: &str = "TRAWL_CRASHDUMP_MONITOR";
/// Carries the abstract-socket name from parent to monitor.
const SOCKET_ENV: &str = "TRAWL_CRASHDUMP_SOCKET";
/// Directory for `*.dmp` output (set by the chart / operator).
const DIR_ENV: &str = "TRAWL_CRASH_DUMP_DIR";
/// Keep at most this many dumps.
const RETAIN_ENV: &str = "TRAWL_CRASH_DUMP_RETAIN";
const DEFAULT_RETAIN: usize = 10;

/// Keeps the installed [`CrashHandler`] alive. Dropping it detaches the handler
/// and drops the embedded [`Client`], which disconnects and lets the monitor
/// exit.
pub struct Guard {
    _handler: CrashHandler,
    /// Declared after the handler so drop order stays handler first (dropping
    /// the client disconnects and the monitor exits), child second.
    ///
    /// Holding the child unreaped is also what keeps the monitor's pid
    /// reserved: nothing in trawld ignores `SIGCHLD`, so as long as this value
    /// lives the pid cannot be recycled under the `/proc/<pid>` the probe read.
    _monitor: std::process::Child,
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard").finish_non_exhaustive()
    }
}

/// What [`init`] did. The crate prints nothing about it: `trawld` logs the
/// outcome once, through `tracing`, after its subscriber is up.
// See the note on `crate::InitReport`: one value, once, for the process.
#[allow(clippy::large_enum_variant)]
pub enum Init {
    /// No dump directory configured.
    Disabled,
    /// Handler installed, with the verdict on whether a dump would be usable.
    Armed(Guard, Readiness),
    /// Capture did not arm.
    Failed(FailureReason),
}

/// See [`crate::init`].
pub fn init() -> Init {
    let Some(dir) = std::env::var_os(DIR_ENV).map(PathBuf::from) else {
        // Capture is off, but the `cap_sys_ptrace+p` stamp on the binary is
        // unconditional, so the daemon still has to give the capability back
        // (ADR-0023 ruling 4).
        return match caps::seal() {
            Ok(()) => Init::Disabled,
            Err(_) => Init::Failed(FailureReason::Seal),
        };
    };

    // Monitor mode: this process was re-exec'd to write dumps. Never returns.
    if std::env::var_os(MONITOR_ENV).is_some() {
        run_monitor(&dir);
    }

    install(&dir)
}

/// Parent side: spawn the monitor, probe, seal, install the handler.
fn install(dir: &Path) -> Init {
    if create_dump_dir(dir).is_err() {
        return sealed_failure(FailureReason::DumpDir);
    }
    let retain = retain_from_env();
    // Prune leftover dumps from previous runs at startup (a safe context — never
    // inside the signal handler, where directory enumeration is not async-safe).
    prune_dumps(dir, retain);

    // Unique abstract socket name shared with the monitor via the environment.
    let socket = format!("trawld-crashdump-{}", std::process::id());

    let Ok(exe) = std::env::current_exe() else {
        return sealed_failure(FailureReason::CurrentExe);
    };
    let Ok(mut monitor) = Command::new(exe)
        .env(MONITOR_ENV, "1")
        .env(SOCKET_ENV, &socket)
        .env(DIR_ENV, dir)
        .env(RETAIN_ENV, retain.to_string())
        .spawn()
    else {
        return sealed_failure(FailureReason::SpawnMonitor);
    };
    let monitor_pid = monitor.id();

    let Some(client) = connect(&socket) else {
        // Don't leave an orphaned monitor behind.
        reap(&mut monitor);
        return sealed_failure(FailureReason::MonitorUnreachable);
    };

    // The successful connect proves the monitor is past exec, running monitor
    // code and past its capability raise, so its `/proc` status now means
    // something. Ask whether it is still alive BEFORE reading that status: a
    // monitor that died leaves a zombie whose masks read as all-zero, which
    // would be classified as a denial that never happened.
    let alive = !matches!(monitor.try_wait(), Ok(Some(_)));

    // crash-handler issues its own PR_SET_PTRACER from inside the signal
    // handler, where the return value is unobservable. This one is checked, and
    // its result is what the yama scope-1 branch of the verdict turns on.
    let ptracer = caps::set_ptracer(monitor_pid).map_err(|err| err.raw_os_error().unwrap_or(-1));
    let monitor_status = if alive {
        probe::monitor_status(monitor_pid)
    } else {
        None
    };
    let self_status = probe::self_status();
    let inputs = ProbeInputs {
        ptrace_scope: probe::ptrace_scope(),
        monitor: monitor_status,
        ptracer: Some(ptracer),
        dumpable: caps::get_dumpable().ok(),
        credentials_match: monitor_status
            .zip(self_status)
            .map(|(monitor, parent)| credentials_match(&monitor, &parent)),
    };
    // Advisory only. Nothing below reads it, so a probe that got the answer
    // wrong costs an operator a misleading log line and never a disarmed
    // handler (ADR-0023 ruling 5).
    let class = classify(inputs);

    if caps::seal().is_err() {
        reap(&mut monitor);
        return Init::Failed(FailureReason::Seal);
    }
    // Read back what the seal actually achieved rather than asserting it.
    let after_seal = probe::self_status();

    let Ok(handler) = CrashHandler::attach(Box::new(Handler { client })) else {
        reap(&mut monitor);
        return Init::Failed(FailureReason::AttachHandler);
    };
    // Narrow the ptrace grant to the monitor (satisfies Yama ptrace_scope=1;
    // CAP_SYS_PTRACE covers scope 2). Issued lazily by crash-handler at crash
    // time, which is why the checked call above exists as well.
    handler.set_ptracer(Some(monitor_pid));

    Init::Armed(
        Guard {
            _handler: handler,
            _monitor: monitor,
        },
        Readiness {
            class,
            inputs,
            after_seal,
            monitor_pid,
            dir: dir.to_path_buf(),
            retain,
        },
    )
}

/// Give the capability back on the way out of a failed arm.
///
/// A failed seal outranks the reason we were already failing for: the daemon is
/// still holding privilege it was supposed to drop, and that is the one crash
/// dump failure trawld refuses to boot past.
fn sealed_failure(reason: FailureReason) -> Init {
    match caps::seal() {
        Ok(()) => Init::Failed(reason),
        Err(_) => Init::Failed(FailureReason::Seal),
    }
}

/// Stop and reap a monitor we are not going to use.
fn reap(monitor: &mut std::process::Child) {
    let _ = monitor.kill();
    let _ = monitor.wait();
}

/// Connect to the monitor's server, retrying until it is listening (~2s max).
fn connect(socket: &str) -> Option<Client> {
    for _ in 0..100 {
        if let Ok(client) = Client::with_name(SocketName::abstract_namespace(socket)) {
            return Some(client);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// The fatal-signal handler.
struct Handler {
    client: Client,
}

// SAFETY: `on_crash` performs only async-signal-safe work — a single
// `libc::write` of a static buffer (see [`breadcrumb`]) and `request_dump`,
// which serializes the crash context to the monitor over a SEQPACKET socket and
// blocks on the ack. No allocation and no locks held by the crashing threads.
#[allow(unsafe_code)]
unsafe impl CrashEvent for Handler {
    fn on_crash(&self, context: &CrashContext) -> CrashEventResult {
        breadcrumb();
        // Best-effort: even if the monitor is gone, we still want to die with
        // the original signal.
        let _ = self.client.request_dump(context);
        // Restore SIG_DFL so the faulting instruction re-executes and the
        // process terminates with the original signal (exit 139 / 134), which
        // is what the orchestrator records.
        CrashEventResult::Handled(true)
    }
}

const BREADCRUMB: &[u8] = b"trawld: FATAL signal caught - writing minidump to crash-dump dir\n";

/// Write a one-line breadcrumb to stderr without allocating.
fn breadcrumb() {
    // SAFETY: `write(2)` is async-signal-safe. We pass a pointer + length into a
    // static buffer and ignore the result; no allocator or stdio buffer touched.
    #[allow(unsafe_code)]
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            BREADCRUMB.as_ptr().cast::<libc::c_void>(),
            BREADCRUMB.len(),
        );
    }
}

/// Monitor side: run the minidumper server, then exit the process.
fn run_monitor(dir: &Path) -> ! {
    let code = match monitor_main(dir) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("trawl-crashdump monitor: {err}");
            1
        }
    };
    std::process::exit(code);
}

fn monitor_main(dir: &Path) -> Result<(), String> {
    let socket = std::env::var(SOCKET_ENV).map_err(|_| "monitor missing socket env".to_owned())?;
    create_dump_dir(dir).map_err(|e| format!("create dump dir: {e}"))?;

    // ADR-0023 ruling 3: raise before binding, so a parent that sees the socket
    // come up is looking at the monitor's final capability state. A failed
    // raise is deliberately NOT fatal: binding anyway is what lets the parent
    // observe and classify the real state instead of guessing from a monitor
    // that vanished.
    match caps::raise_ptrace_effective() {
        Ok(()) => eprintln!("trawl-crashdump monitor: cap_sys_ptrace raise ok"),
        Err(err) => eprintln!("trawl-crashdump monitor: cap_sys_ptrace raise failed: {err}"),
    }

    let mut server = Server::with_name(SocketName::abstract_namespace(&socket))
        .map_err(|e| format!("bind monitor socket: {e}"))?;

    let handler = MonitorHandler {
        dir: dir.to_path_buf(),
        retain: retain_from_env(),
    };
    let shutdown = AtomicBool::new(false);
    // `stale_timeout = None`: a healthy trawld only sends a message when it
    // crashes, so a timeout would wrongly drop the connection during normal
    // operation. The monitor instead exits when the client disconnects.
    server
        .run(Box::new(handler), &shutdown, None)
        .map_err(|e| format!("monitor loop: {e}"))
}

struct MonitorHandler {
    dir: PathBuf,
    retain: usize,
}

impl ServerHandler for MonitorHandler {
    fn create_minidump_file(&self) -> Result<(File, PathBuf), std::io::Error> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = self.dir.join(format!("trawld-crash-{stamp}.dmp"));
        // 0o600: a minidump is a verbatim copy of process memory (API tokens,
        // TLS keys, auth-db rows), so it must never be group/world-readable.
        // Readable as well as writable so the counts can be parsed back out of
        // this very handle, rather than reopening a path that could have been
        // swapped in between.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        Ok((file, path))
    }

    fn on_minidump_created(&self, result: Result<MinidumpBinary, MdError>) -> LoopAction {
        match result {
            Ok(md) => {
                // A dump whose PTRACE_ATTACH was refused still writes a valid
                // file with no threads and no memory, so the counts are the
                // difference between a capture and an empty shell.
                let path = md.path.display();
                match mdmp::counts(&md.file) {
                    Ok(counts) => eprintln!(
                        "trawl-crashdump: wrote minidump {path} threads={} memory_regions={}",
                        counts.threads, counts.memory_regions
                    ),
                    Err(err) => eprintln!(
                        "trawl-crashdump: wrote minidump {path} threads=? memory_regions=? (header unreadable: {err})"
                    ),
                }
                prune_dumps(&self.dir, self.retain);
            }
            Err(e) => eprintln!("trawl-crashdump: minidump write failed: {e}"),
        }
        // Keep serving; the client disconnecting is what ends the loop.
        LoopAction::Continue
    }

    fn on_message(&self, _kind: u32, _buffer: Vec<u8>) {}

    fn on_client_disconnected(&self, num_clients: usize) -> LoopAction {
        if num_clients == 0 {
            LoopAction::Exit
        } else {
            LoopAction::Continue
        }
    }
}

/// Create the dump directory (and any missing parents) restricted to the owner.
/// Dumps hold raw process memory, so the directory must not be group/world-
/// accessible; `0o700` only affects components this call actually creates (an
/// existing mount point keeps the perms the orchestrator gave it).
fn create_dump_dir(dir: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

fn retain_from_env() -> usize {
    std::env::var(RETAIN_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_RETAIN)
}

/// Delete the oldest `*.dmp` files so at most `retain` remain. Best-effort.
fn prune_dumps(dir: &Path, retain: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dumps: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "dmp"))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    if dumps.len() <= retain {
        return;
    }
    dumps.sort_by_key(|(mtime, _)| *mtime); // oldest first
    let remove = dumps.len() - retain;
    for (_, path) in dumps.into_iter().take(remove) {
        let _ = std::fs::remove_file(path);
    }
}
