//! Manual crash-dump smoke test (Linux only).
//!
//! ```sh
//! TRAWL_CRASH_DUMP_DIR=/tmp/cores \
//!   cargo run -p trawl-crashdump --example crashtest
//! ```
//!
//! Expected: a breadcrumb on stderr, a `*.dmp` written to the dir, the monitor
//! reporting how many threads and memory regions it captured, and the process
//! exiting with signal SIGSEGV (139). A dump with `threads=0` means the ptrace
//! was refused, which is what the printed readiness class predicts. On
//! non-Linux, `init` is a no-op and the process just segfaults with no dump.

use trawl_crashdump::Status;

fn main() {
    // Held for the whole run: dropping the report detaches the handler.
    let report = trawl_crashdump::init();
    match report.status() {
        Status::Disabled => eprintln!("crashtest: capture disabled (set TRAWL_CRASH_DUMP_DIR)"),
        Status::Armed(readiness) => eprintln!(
            "crashtest: armed, readiness={} scope={:?} monitor_pid={} missing={:?} dir={}",
            readiness.class.as_str(),
            readiness.inputs.ptrace_scope,
            readiness.monitor_pid,
            readiness.missing_capability(),
            readiness.dir.display(),
        ),
        Status::Failed(reason) => {
            eprintln!("crashtest: failed to arm: {}", reason.as_str());
        }
    }

    // Deliberately dereference a null pointer to raise a real SIGSEGV (a true
    // hardware fault, si_code > 0 — the prod scenario), not a software abort.
    eprintln!("crashtest: triggering SIGSEGV...");
    #[allow(unsafe_code)]
    unsafe {
        std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 0);
    }

    eprintln!("crashtest: STILL ALIVE — unexpected, the write should have faulted");
    drop(report);
}
