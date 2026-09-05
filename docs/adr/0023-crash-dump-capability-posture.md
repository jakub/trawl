# Crash-dump capture holds `CAP_SYS_PTRACE` in the monitor only, through a permitted-only file capability

status: accepted (2026-09-05) — prep ruling record for #21; binds the deb channel (#19) too

`trawl-crashdump` re-execs `trawld` as a monitor process that ptraces the
crashed daemon and writes the minidump. Under `kernel.yama.ptrace_scope=2`
the monitor must hold EFFECTIVE `CAP_SYS_PTRACE`. A Kubernetes container
running as a non-root uid with `allowPrivilegeEscalation: false` never
gets there: the chart's `capabilities.add: [SYS_PTRACE]` lands in the
bounding set only, `no_new_privs` makes the kernel ignore file
capabilities, and the ambient set has no Kubernetes field. Capture is
armed and inert while startup prints `enabled`. The August 2026 fix
stamped `cap_sys_ptrace+ep` on the binary. That would have broken the
DEFAULT pod: `security/commoncap.c` fails `execve` with `EPERM` when a
file's effective bit is set and the bounding set lacks one of its
permitted capabilities (the "`ping` in a `--cap-drop ALL` container"
failure), and the default chart drops ALL. Both model families verified
that reading against 6.6, 6.12 and mainline.

## Rulings

1. **One binary.** The monitor stays a re-exec of `trawld`. A dedicated
   `trawl-crashdump-monitor` executable was the rival shape: it would add
   an artifact to the image, the `.deb` and the release tarballs, and
   buys no boundary, because a compromised daemon in a container that
   allows privilege escalation can exec the file-capable helper itself.

2. **The file capability is permitted-only: `cap_sys_ptrace+p`, never
   `+ep`.** `commoncap` returns the exec-time `EPERM` only when the
   file effective bit is set, so a `+p` stamp leaves the image runnable
   under `docker run`'s default capability set and under the chart's
   default `drop: [ALL]` + `no_new_privs`. The capability becomes
   effective only in a process that asks for it. The stamp is applied
   unconditionally in the Dockerfile, read back with `getcap` in the
   same `RUN` so a build that lost the xattr fails closed.

3. **The monitor raises it.** Before it binds its socket the monitor
   calls `capset(2)` to move `CAP_SYS_PTRACE` from permitted to
   effective. A failed raise is NOT fatal to the monitor: it binds and
   serves anyway, so the parent can observe the real post-attempt state
   and classify it, and the handler stays armed.

4. **The daemon drops it.** Once the monitor is connected and
   classified, the parent removes `CAP_SYS_PTRACE` from its own
   effective and permitted sets and sets `PR_SET_NO_NEW_PRIVS`, which
   closes the path back to the file capability through a later
   re-exec. This holds on every channel: the Kubernetes file cap, the
   deb's `AmbientCapabilities=` (ambient hands BOTH processes the bit;
   the daemon still drops), and bare `docker run`. Consequence:
   `trawld` may exec nothing after the monitor. That is a new
   constraint on the daemon, not a description of it.

5. **Readiness is probed, classified and logged, never assumed.** After
   the socket connect succeeds (which proves the monitor is past exec
   and running monitor code, and past its raise attempt), the parent
   reads the monitor's `/proc/<pid>/status` (`CapEff`, `CapPrm`,
   `CapBnd`, `NoNewPrivs`), `/proc/sys/kernel/yama/ptrace_scope`, its
   own `PR_GET_DUMPABLE`, and issues `PR_SET_PTRACER` at startup with
   its return checked (the lazy crash-time call observed nothing). The
   classes are closed: scope 3 is `denied`; scope 2 is `ready` iff the
   monitor's `CapEff` carries the bit, else `denied`; scope 1 is
   `ready` on the bit, else `ready` only if `PR_SET_PTRACER`
   succeeded, credentials match and the parent is dumpable (a
   privileged exec clears dumpable, so this is read, not assumed);
   scope 0 likewise without the ptracer step; unreadable or malformed
   inputs are `indeterminate`. `ready` means the capability, yama and
   commoncap prerequisites hold; seccomp and other LSMs are not probed
   and the log says so. The handler stays armed in every class: the
   probe is advisory and a wrong probe must not disarm capture.

6. **The verdict is one structured event, and the crate stops owning
   the parent's startup text.** `init()` returns a closed result:
   disabled (no directory configured), armed with its readiness, or
   failed with a content-free reason. `trawld` logs it exactly once
   through `tracing` after the subscriber is up (`info` for ready,
   `warn` for denied and indeterminate, naming the scope and the
   missing capability), so self-telemetry persists it and the operator
   can query it. The word `enabled` never appears on a denied or
   indeterminate class. The fatal-signal breadcrumb and the monitor's
   own lifecycle output stay on stderr: they are signal-safe or in a
   process with no subscriber, by necessity.

7. **The chart forces `allowPrivilegeEscalation: true` on the `trawld`
   container only, and only when `crashDump.enabled`.** Override, not
   render failure: the shared `securityContext` map is what every
   container inherits and its default is `false`, so failing would
   refuse every enable. `init-auth` and `trawl-web` keep the untouched
   shared map. Enabling crash dumps is therefore incompatible with the
   Restricted Pod Security profile; that is documented, not enforced,
   because admission policy is cluster state the chart cannot see.

8. **Evidence policy.** The capability transition and the startup
   verdict are yama-independent and become a standing CI check on the
   runner that has a docker daemon (`k8s-big`): build the image, read
   `getcap`, run it under the enabled and the misconfigured security
   contexts, assert the `/proc` masks and the logged class. The real
   scope-2 capture and denial pair is proven ONCE, through a real CRI
   (kind) on a host whose operator set `ptrace_scope=2` by hand, and
   recorded as a committed transcript. It is not re-run in CI and has
   no harness: `ptrace_scope` is kernel-global, an unprivileged runner
   cannot set it, and a shared runner must never have it changed.

## Considered and rejected

- **Dedicated monitor binary with `+ep`**: see ruling 1.
- **`+ep` on `trawld` with `SYS_PTRACE` always in the bounding set**:
  keeps the chart's default pod booting but breaks plain `docker run`
  of the image, and hands the daemon an effective capability it never
  needs.
- **Requiring `ptrace_scope=1` on the node**: works with the code as
  shipped and is the homelab stopgap, but it lowers a host-wide
  hardening control for every workload on the node to spare one
  process a narrow capability.
