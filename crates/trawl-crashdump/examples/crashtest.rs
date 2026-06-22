//! Manual crash-dump smoke test (Linux only).
//!
//! ```sh
//! TRAWL_CRASH_DUMP_DIR=/tmp/cores \
//!   cargo run -p trawl-crashdump --example crashtest
//! ```
//!
//! Expected: a breadcrumb on stderr, a `*.dmp` written to the dir, and the
//! process exits with signal SIGSEGV (139). On non-Linux, `init` is a no-op and
//! the process just segfaults with no dump.

fn main() {
    let guard = trawl_crashdump::init();
    eprintln!(
        "crashtest: crash-dump guard installed = {}",
        guard.is_some()
    );

    // Deliberately dereference a null pointer to raise a real SIGSEGV (a true
    // hardware fault, si_code > 0 — the prod scenario), not a software abort.
    eprintln!("crashtest: triggering SIGSEGV...");
    #[allow(unsafe_code)]
    unsafe {
        std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 0);
    }

    eprintln!("crashtest: STILL ALIVE — unexpected, the write should have faulted");
}
