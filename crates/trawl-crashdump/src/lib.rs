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
//!
//! Whether an armed handler would actually produce a usable dump depends on
//! `CAP_SYS_PTRACE`, `kernel.yama.ptrace_scope` and the daemon's own dumpable
//! flag, so [`init`] probes all three and returns the verdict as data
//! ([`Readiness`]). It prints nothing: the daemon logs the verdict once, through
//! `tracing`, after its subscriber exists. The daemon also comes back from
//! [`init`] without `CAP_SYS_PTRACE` and with `no_new_privs` set (ADR-0023
//! ruling 4), so it may exec nothing afterwards.

#[cfg(target_os = "linux")]
mod caps;
#[cfg(target_os = "linux")]
mod imp;
#[cfg(target_os = "linux")]
mod mdmp;
#[cfg(target_os = "linux")]
mod probe;
pub mod readiness;

pub use readiness::{
    FailureReason, MonitorStatus, ProbeInputs, Readiness, ReadinessClass, SelfStatus, Status,
};

/// Keeps the crash handler installed for the lifetime of the process.
///
/// Dropping it uninstalls the handler, disconnects from the monitor (which
/// makes the monitor exit) and releases the monitor's child handle, so bind it
/// in `main` and hold it for the whole run.
#[derive(Debug)]
pub struct Guard {
    // Held only for its `Drop` (which detaches the handler); never read, so the
    // leading underscore suppresses dead_code (a derived `Debug` does not count
    // as a read for that analysis).
    #[cfg(target_os = "linux")]
    _inner: imp::Guard,
}

/// What [`init`] did, with the guard the caller has to keep alive.
///
/// [`status`](InitReport::status) copies the verdict out so it can be logged
/// long after `main` has parked the guard.
// One value, built once per process and parked in `main`'s frame for the whole
// run. Boxing the readiness to even out the variants would buy an allocation
// and an indirection and save nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum InitReport {
    /// Capture is off: `TRAWL_CRASH_DUMP_DIR` is unset, or this is not Linux.
    Disabled,
    /// The handler is installed. `readiness` says whether a crash would produce
    /// a dump with anything in it.
    Armed {
        /// Hold this for the lifetime of the process.
        guard: Guard,
        /// The probe and its verdict.
        readiness: Readiness,
    },
    /// Capture did not arm. `reason` names the step, without quoting an OS
    /// message at what is a trust boundary.
    Failed {
        /// Which step failed.
        reason: FailureReason,
    },
}

impl InitReport {
    /// The verdict without the guard, for logging.
    #[must_use]
    pub fn status(&self) -> Status {
        match self {
            Self::Disabled => Status::Disabled,
            Self::Armed { readiness, .. } => Status::Armed(readiness.clone()),
            Self::Failed { reason } => Status::Failed(*reason),
        }
    }
}

/// Seal a validation-only process without starting crash capture.
///
/// Call on the main thread before reading configuration or starting threads.
/// This drops ptrace privileges and prevents privilege acquisition on exec,
/// but creates no files, monitor processes, or signal handlers.
///
/// # Errors
/// Returns a content-free seal failure if either protection cannot be set.
pub fn seal_for_config_check() -> Result<(), FailureReason> {
    #[cfg(target_os = "linux")]
    caps::seal().map_err(|_| FailureReason::Seal)?;
    Ok(())
}

/// Initialize crash-dump capture.
///
/// For daemon startup, call before config reads or any threads are spawned
/// or the async runtime is built: the monitor is launched by re-execing this
/// binary, that re-exec/spawn is only fork-safe while the process is still
/// single-threaded, and the capability seal applies to the calling thread, which
/// has to be the one every later thread inherits from.
///
/// On Linux, if this process was itself re-exec'd as the monitor, this runs the
/// monitor loop and **never returns** (it exits the process when the parent
/// goes away).
///
/// On return the process no longer holds `CAP_SYS_PTRACE` and has
/// `no_new_privs` set, on every path including [`InitReport::Disabled`]. It
/// therefore may not exec anything afterwards.
#[must_use]
pub fn init() -> InitReport {
    #[cfg(target_os = "linux")]
    {
        match imp::init() {
            imp::Init::Disabled => InitReport::Disabled,
            imp::Init::Armed(inner, readiness) => InitReport::Armed {
                guard: Guard { _inner: inner },
                readiness,
            },
            imp::Init::Failed(reason) => InitReport::Failed { reason },
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        InitReport::Disabled
    }
}
