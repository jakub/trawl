# Crash-dump capture holds `CAP_SYS_PTRACE` in the monitor only, through a permitted-only file capability

status: accepted (2026-09-05) — prep ruling record for #21; binds the deb channel (#19) too

`trawl-crashdump` re-execs `trawld` as a monitor process that ptraces the
crashed daemon and writes the minidump. Under `kernel.yama.ptrace_scope=2`
the monitor must hold EFFECTIVE `CAP_SYS_PTRACE`. It did not, on the
chart as it stood. containerd grants `capabilities.add: [SYS_PTRACE]` to
the container's INIT process as permitted and effective, so trawld itself
held the bit, but trawld re-execs to become the monitor and the released
binary carried no file capability. An `execve` of a file with no
capabilities, by a process with an empty inheritable and ambient set,
produces an empty permitted set. The bit died at that exec, every mask in
`/proc/<monitor>/status` read zero, and capture was armed and inert while
startup printed `enabled`. The August 2026 fix stamped `cap_sys_ptrace+ep`
on the binary. That would have broken the
DEFAULT pod: `security/commoncap.c` fails `execve` with `EPERM` when a
file's effective bit is set and the bounding set lacks one of its
permitted capabilities (the "`ping` in a `--cap-drop ALL` container"
failure), and the default chart drops ALL. Both model families verified
that reading against 6.6, 6.12 and mainline.

## Rulings

1. **One binary.** The monitor stays a re-exec of `trawld`. A dedicated
   `trawl-crashdump-monitor` executable was the rival shape: it would add
   an artifact to the image, the `.deb` and the release tarballs, and
   buys no boundary, because containerd hands the pod spec's added
   capability to the container's init process, so trawld holds
   `CAP_SYS_PTRACE` itself until it seals (ruling 4), whoever owns the
   binary that ptraces.

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

7. **The chart adds `SYS_PTRACE` to the `trawld` container only, and
   only when `crashDump.enabled`. It forces no privilege escalation:
   `allowPrivilegeEscalation` stays `false` everywhere.** (Amended
   2026-09-06; see the amendment below for the run that settled it.) The
   added capability is the whole grant. containerd hands it to the
   container's init process as permitted and effective, and the
   `cap_sys_ptrace+p` file capability keeps the bit across trawld's exec
   of the monitor, which `no_new_privs` allows because nothing is gained.
   `init-auth` and `trawl-web` keep the untouched shared map. Enabling
   crash dumps is still incompatible with the Restricted Pod Security
   profile, now for a different reason: Restricted refuses any added
   capability except `NET_BIND_SERVICE`. That is documented, not
   enforced, because admission policy is cluster state the chart cannot
   see.

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

## Amendment (2026-09-06)

Ruling 7's original posture, forcing `allowPrivilegeEscalation: true` on
the `trawld` container, is superseded. It was based on a wrong reading of
where the pod spec's added capability lands.

A kind run (containerd 2.3.1, node image v1.36.1) on a host whose
operator had set `kernel.yama.ptrace_scope=2` settled it. With the pod
patched to `allowPrivilegeEscalation: false` and `capabilities.add:
[SYS_PTRACE]` kept, the monitor still held `CAP_SYS_PTRACE` effective:
`CapEff=0x80000` with `NoNewPrivs=1`, verdict `ready ptrace_scope=2`, and
`kill -SEGV 1` wrote a dump carrying 34 threads. `crictl inspect` shows
why: containerd grants an added capability to the container's init
process as permitted AND effective, plus bounding, not bounding-only.
The `cap_sys_ptrace+p` file capability then carries the bit across
trawld's exec of the monitor, and `no_new_privs` does not object, because
commoncap downgrades only when the new permitted set is not a subset of
the old one. Nothing is gained here, so nothing is stripped.

The empty masks in the original bug report were the same capability dying
at that exec: 0.3.2's trawld had no file capability, an empty ambient set
and nothing inheritable, so the monitor started with nothing. Ruling 2's
`+p` stamp is what fixed that, and the escalation was never the missing
piece.

The genuine denied case is the capability being absent from the pod spec:
all sets zero, verdict `denied ... missing="CAP_SYS_PTRACE"`, and a 72 KB
dump reporting `threads=0 memory_regions=0`.

So the chart adds `SYS_PTRACE` and nothing else. Restricted Pod Security
remains incompatible, because it refuses any added capability except
`NET_BIND_SERVICE`. Rulings 2 and 8 are unaffected: the `+p` reasoning
about the default `drop: [ALL]` pod still holds, and the evidence policy
that produced this run is the reason the posture is now right.
