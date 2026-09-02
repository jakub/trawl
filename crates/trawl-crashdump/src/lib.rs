//! Crash-dump (minidump) capture for trawld.
//!
//! On Linux, [`init`] installs a fatal-signal handler ([`crash_handler`]) and
//! re-execs the current binary as a minidump *monitor* process (a process
//! cannot ptrace its own thread group, so the dump must be written by a
//! separate process). On a fatal signal the handler emits a one-line stderr
//! breadcrumb, asks the monitor to write a minidump, and then lets the process
//! die with the original signal so the orchestrator still sees the real exit
//! code (e.g. 139 for `SIGSEGV`).
//!
//! On every other platform this is a no-op so the daemon builds everywhere.
//!
//! Capture is enabled by the environment contract the Helm chart sets:
//! `TRAWL_CRASH_DUMP_DIR` (directory for `*.dmp`) and `TRAWL_CRASH_DUMP_RETAIN`
//! (keep at most N dumps).

#[cfg(target_os = "linux")]
mod imp;

/// Keeps the crash handler installed for the lifetime of the process.
///
/// Dropping it uninstalls the handler (and on Linux disconnects from the
/// monitor), so bind it in `main` and hold it for the whole run.
#[derive(Debug)]
pub struct Guard {
    // Held only for its `Drop` (which detaches the handler); never read, so the
    // leading underscore suppresses dead_code (a derived `Debug` does not count
    // as a read for that analysis).
    #[cfg(target_os = "linux")]
    _inner: imp::Guard,
}

/// Initialize crash-dump capture.
///
/// MUST be the very first statement in `main()`, before any threads are spawned
/// or the async runtime is built: the monitor is launched by re-execing this
/// binary, and that re-exec/spawn is only fork-safe while the process is still
/// single-threaded.
///
/// On Linux, if this process was itself re-exec'd as the monitor, this runs the
/// monitor loop and **never returns** (it exits the process when the parent
/// goes away).
///
/// Returns `None` if capture is disabled (`TRAWL_CRASH_DUMP_DIR` unset) or
/// unsupported on this platform.
#[must_use]
pub fn init() -> Option<Guard> {
    #[cfg(target_os = "linux")]
    {
        imp::init().map(|inner| Guard { _inner: inner })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
