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
//! - parent: spawn the monitor, connect a [`Client`], install a [`CrashHandler`].
//! - on a fatal signal: emit a signal-safe stderr breadcrumb, call
//!   `request_dump` (which blocks until the monitor has written the dump and
//!   acked), then return `Handled(true)` so the instruction re-faults and the
//!   process dies with the original signal (exit 139 for `SIGSEGV`).
//! - the monitor exits when the client disconnects (parent died or shut down).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crash_handler::{CrashContext, CrashEvent, CrashEventResult, CrashHandler};
use minidumper::{
    Client, Error as MdError, LoopAction, MinidumpBinary, Server, ServerHandler, SocketName,
};

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
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard").finish_non_exhaustive()
    }
}

/// See [`crate::init`].
pub fn init() -> Option<Guard> {
    // Disabled unless the dump directory is configured.
    let dir = std::env::var_os(DIR_ENV).map(PathBuf::from)?;

    // Monitor mode: this process was re-exec'd to write dumps. Never returns.
    if std::env::var_os(MONITOR_ENV).is_some() {
        run_monitor(&dir);
    }

    match install(&dir) {
        Ok(guard) => Some(guard),
        Err(err) => {
            // Best-effort: log and run without capture rather than block startup.
            eprintln!("trawl-crashdump: capture disabled: {err}");
            None
        }
    }
}

/// Parent side: spawn the monitor, connect, install the handler.
fn install(dir: &Path) -> Result<Guard, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create dump dir {}: {e}", dir.display()))?;
    let retain = retain_from_env();
    // Prune leftover dumps from previous runs at startup (a safe context — never
    // inside the signal handler, where directory enumeration is not async-safe).
    prune_dumps(dir, retain);

    // Unique abstract socket name shared with the monitor via the environment.
    let socket = format!("trawld-crashdump-{}", std::process::id());

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut monitor = Command::new(exe)
        .env(MONITOR_ENV, "1")
        .env(SOCKET_ENV, &socket)
        .env(DIR_ENV, dir)
        .env(RETAIN_ENV, retain.to_string())
        .spawn()
        .map_err(|e| format!("spawn monitor: {e}"))?;
    let monitor_pid = monitor.id();

    let Some(client) = connect(&socket) else {
        // Don't leave an orphaned monitor behind.
        let _ = monitor.kill();
        let _ = monitor.wait();
        return Err("monitor did not come up in time".to_owned());
    };

    let handler = CrashHandler::attach(Box::new(Handler { client }))
        .map_err(|e| format!("attach crash handler: {e}"))?;
    // Narrow the ptrace grant to the monitor (satisfies Yama ptrace_scope=1;
    // CAP_SYS_PTRACE covers scope 2). Issued lazily by crash-handler at crash
    // time.
    handler.set_ptracer(Some(monitor_pid));

    eprintln!(
        "trawl-crashdump: enabled (dir={}, retain={retain})",
        dir.display()
    );
    Ok(Guard { _handler: handler })
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
    std::fs::create_dir_all(dir).map_err(|e| format!("create dump dir: {e}"))?;

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
        let file = File::create(&path)?;
        Ok((file, path))
    }

    fn on_minidump_created(&self, result: Result<MinidumpBinary, MdError>) -> LoopAction {
        match result {
            Ok(md) => {
                eprintln!("trawl-crashdump: wrote minidump {}", md.path.display());
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
