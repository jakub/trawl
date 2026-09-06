//! What the crash-dump probe found, and the one rule that turns it into a verdict.
//!
//! Pure data and one decision function: no syscalls, no `/proc`, no `cfg`. The
//! Linux-only modules ([`crate::probe`], [`crate::caps`]) gather the inputs and
//! this module decides what they mean, so the whole decision table is testable
//! on any host.
//!
//! The verdict is ADVISORY (ADR-0023 ruling 5). Nothing between the probe and
//! `CrashHandler::attach` reads it, so a probe that is wrong costs an operator a
//! misleading log line, never a disarmed handler.

use std::path::PathBuf;

/// Bit position of `CAP_SYS_PTRACE` in a capability mask (`include/uapi/linux/capability.h`).
///
/// Lives here rather than in `caps` because the classifier needs it on every
/// platform and `caps` is Linux-only.
pub const CAP_SYS_PTRACE_BIT: u32 = 19;

const CAP_SYS_PTRACE_MASK: u64 = 1 << CAP_SYS_PTRACE_BIT;

/// The verdict on whether a crash would actually produce a usable minidump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessClass {
    /// The capability, yama and commoncap prerequisites hold. Seccomp and other
    /// LSMs are not probed, so this is a necessary condition, not a promise.
    Ready,
    /// A prerequisite is provably missing: the dump will have zero threads.
    Denied,
    /// An input was unreadable or malformed, so no verdict can be given.
    Indeterminate,
}

impl ReadinessClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Denied => "denied",
            Self::Indeterminate => "indeterminate",
        }
    }
}

/// The monitor process's `/proc/<pid>/status`, reduced to the fields that decide capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorStatus {
    /// `CapEff`.
    pub eff: u64,
    /// `CapPrm`.
    pub prm: u64,
    /// `CapBnd`.
    pub bnd: u64,
    /// `NoNewPrivs`.
    pub no_new_privs: bool,
    /// `Uid`: real, effective, saved, filesystem.
    pub uid: [u32; 4],
    /// `Gid`: real, effective, saved, filesystem.
    pub gid: [u32; 4],
}

impl MonitorStatus {
    #[must_use]
    pub fn has_ptrace_effective(&self) -> bool {
        self.eff & CAP_SYS_PTRACE_MASK != 0
    }

    #[must_use]
    pub fn has_ptrace_permitted(&self) -> bool {
        self.prm & CAP_SYS_PTRACE_MASK != 0
    }
}

/// The daemon's own `/proc/self/status`, same fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfStatus {
    /// `CapEff`.
    pub eff: u64,
    /// `CapPrm`.
    pub prm: u64,
    /// `CapBnd`.
    pub bnd: u64,
    /// `NoNewPrivs`.
    pub no_new_privs: bool,
    /// `Uid`: real, effective, saved, filesystem.
    pub uid: [u32; 4],
    /// `Gid`: real, effective, saved, filesystem.
    pub gid: [u32; 4],
}

impl SelfStatus {
    #[must_use]
    pub fn has_ptrace_effective(&self) -> bool {
        self.eff & CAP_SYS_PTRACE_MASK != 0
    }

    #[must_use]
    pub fn has_ptrace_permitted(&self) -> bool {
        self.prm & CAP_SYS_PTRACE_MASK != 0
    }
}

/// Everything the parent observed between connecting to the monitor and sealing itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeInputs {
    /// `/proc/sys/kernel/yama/ptrace_scope`. `None` = unreadable or out of range.
    pub ptrace_scope: Option<u8>,
    /// The monitor's status. `None` = unreadable or malformed. A monitor the
    /// parent could not observe alive never reaches a verdict at all: it fails
    /// the arm instead.
    pub monitor: Option<MonitorStatus>,
    /// The startup `prctl(PR_SET_PTRACER)`: `Some(Ok)` returned 0, `Some(Err(errno))`
    /// failed, `None` was not attempted.
    pub ptracer: Option<Result<(), i32>>,
    /// `prctl(PR_GET_DUMPABLE) == 1`. A privileged exec clears this, so it is read.
    pub dumpable: Option<bool>,
    /// Whether the monitor may ptrace us on credentials alone (see [`credentials_match`]).
    pub credentials_match: Option<bool>,
}

/// Does the monitor pass `__ptrace_may_access`'s credential half against this parent?
///
/// The kernel wants the tracer's real uid to equal all three of the target's
/// real, effective and saved uids (same for gid) and the tracer's permitted set
/// to be a superset of the target's. The parent's permitted set is taken as it
/// will be AFTER the seal (`CAP_SYS_PTRACE` cleared), because that is the state
/// a crash would find.
#[must_use]
pub fn credentials_match(monitor: &MonitorStatus, parent: &SelfStatus) -> bool {
    let ids_match = parent.uid[..3].iter().all(|&u| u == monitor.uid[0])
        && parent.gid[..3].iter().all(|&g| g == monitor.gid[0]);
    let parent_after_seal_prm = parent.prm & !CAP_SYS_PTRACE_MASK;
    ids_match && monitor.prm & parent_after_seal_prm == parent_after_seal_prm
}

/// Turn the probe into a verdict (ADR-0023 ruling 5).
///
/// The order matters and is the ruling's order: yama scope 3 denies everything,
/// an unreadable scope or monitor is indeterminate, an effective
/// `CAP_SYS_PTRACE` on the monitor is ready at every other scope, and only then
/// do the scope-specific fallbacks apply.
///
/// `Ready` means the capability, yama and commoncap prerequisites hold. Seccomp
/// and other LSMs are not probed.
#[must_use]
pub fn classify(inputs: ProbeInputs) -> ReadinessClass {
    use ReadinessClass::{Denied, Indeterminate, Ready};

    match inputs.ptrace_scope {
        // Yama scope 3 refuses every tracer, capability or not, and the setting
        // cannot be lowered without a reboot.
        Some(3) => return Denied,
        None => return Indeterminate,
        _ => {}
    }
    // No status to read. The parent proved the monitor alive before reading it,
    // so this is a `/proc` file that would not parse (or a monitor that died in
    // between); guessing from a zombie's zeroed masks would report a denial that
    // never happened.
    let Some(monitor) = inputs.monitor else {
        return Indeterminate;
    };
    let capable = monitor.has_ptrace_effective();
    match inputs.ptrace_scope {
        // Scope 2 admits privileged tracers and nothing else.
        Some(2) => {
            if capable {
                Ready
            } else {
                Denied
            }
        }
        Some(scope @ (0 | 1)) => {
            if capable {
                return Ready;
            }
            // Scopes 0 and 1 can still admit the monitor on credentials. Both
            // halves must be known: a missing one is a probe failure, not a
            // denial.
            let (Some(dumpable), Some(credentials)) = (inputs.dumpable, inputs.credentials_match)
            else {
                return Indeterminate;
            };
            // Scope 1 additionally needs the target to have named this tracer,
            // which is what the startup PR_SET_PTRACER does. Scope 0 asks for
            // no declaration.
            let declared = scope == 0 || inputs.ptracer == Some(Ok(()));
            if declared && credentials && dumpable {
                Ready
            } else {
                Denied
            }
        }
        // A scope this table does not know. The probe never produces one, so
        // reaching here means the kernel grew a value and the verdict is not
        // ours to give.
        _ => Indeterminate,
    }
}

/// The armed state: the verdict, what it was made of, and where dumps land.
#[derive(Debug, Clone)]
pub struct Readiness {
    /// The verdict.
    pub class: ReadinessClass,
    /// The inputs [`classify`] saw.
    pub inputs: ProbeInputs,
    /// The daemon's own status re-read after the seal. `None` = unreadable.
    pub after_seal: Option<SelfStatus>,
    /// Pid of the monitor process.
    pub monitor_pid: u32,
    /// Directory dumps are written to.
    pub dir: PathBuf,
    /// How many dumps are kept.
    pub retain: usize,
}

impl Readiness {
    /// The one place a capability is named for a log.
    ///
    /// `Some` only when the verdict actually turned on the monitor not holding
    /// the bit. Yama scope 3 refuses every tracer regardless, so it names
    /// nothing: telling an operator to grant a capability that would not help
    /// is worse than saying nothing.
    #[must_use]
    pub fn missing_capability(&self) -> Option<&'static str> {
        if self.class != ReadinessClass::Denied || self.inputs.ptrace_scope == Some(3) {
            return None;
        }
        let monitor = self.inputs.monitor?;
        (!monitor.has_ptrace_effective()).then_some("CAP_SYS_PTRACE")
    }
}

/// Why arming failed. Content-free by design (ADR-0023 ruling 6): these reach a
/// log at a trust boundary, so they name a step and never quote an OS message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// `std::env::current_exe` failed, so the monitor could not be re-exec'd.
    CurrentExe,
    /// The dump directory could not be created.
    DumpDir,
    /// Spawning the monitor failed.
    SpawnMonitor,
    /// The monitor never came up on its socket, or it had already exited when
    /// the parent went to use its pid.
    MonitorUnreachable,
    /// Something other than the spawned monitor holds the socket name. The
    /// abstract name has no permissions and is predictable, so a stranger can
    /// bind it first; `SO_PEERCRED` says who actually did.
    MonitorIdentity,
    /// `CrashHandler::attach` failed.
    AttachHandler,
    /// Dropping `CAP_SYS_PTRACE` or setting `no_new_privs` failed.
    Seal,
}

impl FailureReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CurrentExe => "current_exe",
            Self::DumpDir => "dump_dir",
            Self::SpawnMonitor => "spawn_monitor",
            Self::MonitorUnreachable => "monitor_unreachable",
            Self::MonitorIdentity => "monitor_identity",
            Self::AttachHandler => "attach_handler",
            Self::Seal => "seal",
        }
    }
}

/// What `init` did, without the guard: cloneable, so the daemon can log it long
/// after `main` has parked the guard.
#[derive(Debug, Clone)]
pub enum Status {
    /// No dump directory configured.
    Disabled,
    /// Handler installed; the verdict says whether a dump would be usable.
    Armed(Readiness),
    /// Capture did not arm.
    Failed(FailureReason),
}

impl Status {
    /// Every value the `readiness` log field can take.
    ///
    /// `Disabled` is not in here: that class logs nothing at all. The CI script
    /// greps for these words, and a drift test reads both.
    pub const ALL: [&'static str; 4] = ["ready", "denied", "indeterminate", "failed"];

    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Armed(readiness) => readiness.class.as_str(),
            Self::Failed(_) => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CAP_SYS_PTRACE_MASK, FailureReason, MonitorStatus, ProbeInputs, Readiness, ReadinessClass,
        SelfStatus, Status, classify, credentials_match,
    };
    use std::path::PathBuf;

    fn monitor(capable: bool) -> MonitorStatus {
        MonitorStatus {
            eff: if capable { CAP_SYS_PTRACE_MASK } else { 0 },
            prm: CAP_SYS_PTRACE_MASK,
            bnd: CAP_SYS_PTRACE_MASK,
            no_new_privs: false,
            uid: [1000; 4],
            gid: [1000; 4],
        }
    }

    /// Every fallback input satisfied, so a test that flips one input is
    /// changing exactly the thing it names.
    fn inputs(scope: Option<u8>, monitor: Option<MonitorStatus>) -> ProbeInputs {
        ProbeInputs {
            ptrace_scope: scope,
            monitor,
            ptracer: Some(Ok(())),
            dumpable: Some(true),
            credentials_match: Some(true),
        }
    }

    #[test]
    fn classify_scope3_denies_even_a_capable_monitor() {
        assert_eq!(
            classify(inputs(Some(3), Some(monitor(true)))),
            ReadinessClass::Denied
        );
    }

    #[test]
    fn classify_scope3_denies_before_the_monitor_is_consulted() {
        assert_eq!(classify(inputs(Some(3), None)), ReadinessClass::Denied);
    }

    #[test]
    fn classify_unreadable_scope_is_indeterminate() {
        assert_eq!(
            classify(inputs(None, Some(monitor(true)))),
            ReadinessClass::Indeterminate
        );
    }

    #[test]
    fn classify_scope_outside_the_table_is_indeterminate() {
        assert_eq!(
            classify(inputs(Some(4), Some(monitor(true)))),
            ReadinessClass::Indeterminate
        );
    }

    #[test]
    fn classify_unreadable_monitor_is_indeterminate_at_every_live_scope() {
        for scope in [0, 1, 2] {
            assert_eq!(
                classify(inputs(Some(scope), None)),
                ReadinessClass::Indeterminate,
                "scope {scope}"
            );
        }
    }

    #[test]
    fn classify_scope2_capable_monitor_is_ready() {
        assert_eq!(
            classify(inputs(Some(2), Some(monitor(true)))),
            ReadinessClass::Ready
        );
    }

    #[test]
    fn classify_scope2_without_the_capability_is_denied() {
        // Nothing rescues scope 2: ptracer, credentials and dumpable are all
        // satisfied here and the verdict is still denied.
        assert_eq!(
            classify(inputs(Some(2), Some(monitor(false)))),
            ReadinessClass::Denied
        );
    }

    #[test]
    fn classify_scope1_capable_monitor_is_ready() {
        assert_eq!(
            classify(inputs(Some(1), Some(monitor(true)))),
            ReadinessClass::Ready
        );
    }

    #[test]
    fn classify_scope0_capable_monitor_is_ready() {
        assert_eq!(
            classify(inputs(Some(0), Some(monitor(true)))),
            ReadinessClass::Ready
        );
    }

    #[test]
    fn classify_scope1_fallback_is_ready_when_ptracer_creds_and_dumpable_hold() {
        assert_eq!(
            classify(inputs(Some(1), Some(monitor(false)))),
            ReadinessClass::Ready
        );
    }

    #[test]
    fn classify_scope1_fallback_needs_the_ptracer_call() {
        for ptracer in [None, Some(Err(libc_esrch()))] {
            let probe = ProbeInputs {
                ptracer,
                ..inputs(Some(1), Some(monitor(false)))
            };
            assert_eq!(classify(probe), ReadinessClass::Denied, "{ptracer:?}");
        }
    }

    #[test]
    fn classify_scope1_fallback_denied_on_mismatched_credentials() {
        let probe = ProbeInputs {
            credentials_match: Some(false),
            ..inputs(Some(1), Some(monitor(false)))
        };
        assert_eq!(classify(probe), ReadinessClass::Denied);
    }

    #[test]
    fn classify_scope1_fallback_denied_when_not_dumpable() {
        let probe = ProbeInputs {
            dumpable: Some(false),
            ..inputs(Some(1), Some(monitor(false)))
        };
        assert_eq!(classify(probe), ReadinessClass::Denied);
    }

    #[test]
    fn classify_fallback_is_indeterminate_when_dumpable_or_credentials_are_unknown() {
        for scope in [0, 1] {
            let no_dumpable = ProbeInputs {
                dumpable: None,
                ..inputs(Some(scope), Some(monitor(false)))
            };
            assert_eq!(
                classify(no_dumpable),
                ReadinessClass::Indeterminate,
                "scope {scope} dumpable"
            );
            let no_creds = ProbeInputs {
                credentials_match: None,
                ..inputs(Some(scope), Some(monitor(false)))
            };
            assert_eq!(
                classify(no_creds),
                ReadinessClass::Indeterminate,
                "scope {scope} credentials"
            );
        }
    }

    #[test]
    fn classify_scope0_fallback_ignores_the_ptracer_call() {
        for ptracer in [None, Some(Err(libc_esrch())), Some(Ok(()))] {
            let probe = ProbeInputs {
                ptracer,
                ..inputs(Some(0), Some(monitor(false)))
            };
            assert_eq!(classify(probe), ReadinessClass::Ready, "{ptracer:?}");
        }
    }

    #[test]
    fn classify_scope0_fallback_denied_on_credentials_or_dumpable() {
        let bad_creds = ProbeInputs {
            credentials_match: Some(false),
            ..inputs(Some(0), Some(monitor(false)))
        };
        assert_eq!(classify(bad_creds), ReadinessClass::Denied);
        let not_dumpable = ProbeInputs {
            dumpable: Some(false),
            ..inputs(Some(0), Some(monitor(false)))
        };
        assert_eq!(classify(not_dumpable), ReadinessClass::Denied);
    }

    fn libc_esrch() -> i32 {
        3
    }

    fn readiness(class: ReadinessClass, probe: ProbeInputs) -> Readiness {
        Readiness {
            class,
            inputs: probe,
            after_seal: None,
            monitor_pid: 4242,
            dir: PathBuf::from("/tmp/cores"),
            retain: 10,
        }
    }

    #[test]
    fn missing_capability_names_the_bit_the_verdict_turned_on() {
        let denied = readiness(
            ReadinessClass::Denied,
            inputs(Some(2), Some(monitor(false))),
        );
        assert_eq!(denied.missing_capability(), Some("CAP_SYS_PTRACE"));

        let fallback_denial = readiness(
            ReadinessClass::Denied,
            ProbeInputs {
                dumpable: Some(false),
                ..inputs(Some(1), Some(monitor(false)))
            },
        );
        assert_eq!(fallback_denial.missing_capability(), Some("CAP_SYS_PTRACE"));
    }

    #[test]
    fn missing_capability_is_silent_when_the_capability_would_not_help() {
        let scope3 = readiness(
            ReadinessClass::Denied,
            inputs(Some(3), Some(monitor(false))),
        );
        assert_eq!(scope3.missing_capability(), None);

        let ready = readiness(ReadinessClass::Ready, inputs(Some(2), Some(monitor(true))));
        assert_eq!(ready.missing_capability(), None);

        let unknown = readiness(ReadinessClass::Indeterminate, inputs(None, None));
        assert_eq!(unknown.missing_capability(), None);

        let no_monitor = readiness(ReadinessClass::Denied, inputs(Some(1), None));
        assert_eq!(no_monitor.missing_capability(), None);
    }

    fn parent() -> SelfStatus {
        SelfStatus {
            eff: CAP_SYS_PTRACE_MASK,
            prm: CAP_SYS_PTRACE_MASK,
            bnd: CAP_SYS_PTRACE_MASK,
            no_new_privs: false,
            uid: [1000; 4],
            gid: [1000; 4],
        }
    }

    #[test]
    fn credentials_match_accepts_the_ordinary_pod() {
        assert!(credentials_match(&monitor(true), &parent()));
        // The monitor without the capability still matches: the permitted set
        // compared is the parent's POST-seal one, which has the bit cleared.
        assert!(credentials_match(
            &MonitorStatus {
                prm: 0,
                ..monitor(false)
            },
            &parent()
        ));
    }

    #[test]
    fn credentials_match_rejects_a_uid_or_gid_mismatch() {
        let wrong_user = MonitorStatus {
            uid: [1001, 1000, 1000, 1000],
            ..monitor(true)
        };
        assert!(!credentials_match(&wrong_user, &parent()));
        let wrong_group = MonitorStatus {
            gid: [1001, 1000, 1000, 1000],
            ..monitor(true)
        };
        assert!(!credentials_match(&wrong_group, &parent()));
        let setuid_parent = SelfStatus {
            uid: [1000, 0, 1000, 1000],
            ..parent()
        };
        assert!(!credentials_match(&monitor(true), &setuid_parent));
    }

    #[test]
    fn credentials_match_rejects_a_monitor_missing_an_unrelated_permitted_bit() {
        // CAP_NET_BIND_SERVICE (bit 10) held by the parent but not the monitor.
        let parent = SelfStatus {
            prm: CAP_SYS_PTRACE_MASK | (1 << 10),
            ..parent()
        };
        assert!(!credentials_match(&monitor(true), &parent));
        let wider = MonitorStatus {
            prm: CAP_SYS_PTRACE_MASK | (1 << 10),
            ..monitor(true)
        };
        assert!(credentials_match(&wider, &parent));
    }

    #[test]
    fn status_words_are_pinned_and_delegated() {
        assert_eq!(Status::ALL, ["ready", "denied", "indeterminate", "failed"]);
        assert_eq!(Status::Disabled.as_str(), "disabled");
        assert_eq!(
            Status::Armed(readiness(
                ReadinessClass::Ready,
                inputs(Some(2), Some(monitor(true)))
            ))
            .as_str(),
            "ready"
        );
        assert_eq!(Status::Failed(FailureReason::Seal).as_str(), "failed");
        for class in [
            ReadinessClass::Ready,
            ReadinessClass::Denied,
            ReadinessClass::Indeterminate,
        ] {
            assert!(Status::ALL.contains(&class.as_str()), "{class:?}");
        }
        assert_eq!(FailureReason::Seal.as_str(), "seal");
        assert_eq!(FailureReason::CurrentExe.as_str(), "current_exe");
        assert_eq!(FailureReason::MonitorIdentity.as_str(), "monitor_identity");
        assert_eq!(
            FailureReason::MonitorUnreachable.as_str(),
            "monitor_unreachable"
        );
    }
}
