---
title: Crash dumps
description: Enabling minidump capture for trawld on the Debian and helm channels, what a dump contains, and how to check that capture can actually work.
---

When trawld dies on a fatal signal (`SIGSEGV`, `SIGABRT`, `SIGBUS`), it can
write a **minidump**: a `.dmp` file holding the crashed process's threads,
their stacks, and the memory around them. Dumps land in a directory you
choose and are named `trawld-crash-<nanos>.dmp`, mode `0600`.

Capture is off until an operator turns it on, on both distribution channels.

## How it works, and why a privilege is involved

A process cannot ptrace its own thread group, so trawld cannot dump itself.
Instead it re-execs its own binary as a separate **monitor** process at
startup and keeps a socket open to it. On a fatal signal the handler writes a
one-line stderr breadcrumb, asks the monitor to attach and write the dump,
waits for the ack, then re-raises so the process still dies with the original
signal. Supervisors see the same signal exit they always did — systemd records
it as `status=11/SEGV`, a shell renders it as 139 — and restart trawld as
usual.

The attach is the whole reason a capability appears anywhere in this page.
The monitor needs `CAP_SYS_PTRACE`, or a permissive enough yama policy, to
read the crashed process. Nothing else about trawld needs it, and nothing
grants it unless you enable capture.

Capture is Linux-only; on any other platform `init` is a no-op.

## Enabling on Debian

The `trawl-server` package ships an inactive systemd drop-in at
`/usr/share/doc/trawl-server/examples/crashdump.conf`. It is documentation
until you copy it into place:

```ini
[Service]
AmbientCapabilities=CAP_SYS_PTRACE
Environment=TRAWL_CRASH_DUMP_DIR=/var/lib/trawl/cores
Environment=TRAWL_CRASH_DUMP_RETAIN=10
```

To enable:

```bash
sudo install -D -m 0644 \
  /usr/share/doc/trawl-server/examples/crashdump.conf \
  /etc/systemd/system/trawld.service.d/crashdump.conf
sudo systemctl daemon-reload && sudo systemctl restart trawld
```

`/var/lib/trawl/cores` is already there. postinst creates it `trawl:trawl`
mode `0700` on every install, whether or not you enable capture, so the
directory existing tells you nothing about whether dumps are being written.

That path belongs to the package. `systemd-tmpfiles` reapplies it on every
configure and every boot from `/usr/lib/tmpfiles.d/trawl.conf`, and the entry
enforces the type: anything at that path that is not a directory, a regular
file or a symlink you put there, gets removed and replaced with the directory.
So do not point `/var/lib/trawl/cores` somewhere else with a symlink. It will
survive until the next boot and then quietly stop being your symlink.

To write dumps elsewhere, change where trawld looks instead. Edit
`TRAWL_CRASH_DUMP_DIR` in your copy of the drop-in under
`/etc/systemd/system/trawld.service.d/`, and create the target yourself, owned
by `trawl` and mode `0700`. trawld only applies `0700` to directories it
creates itself, so a directory that already exists, including a mount point,
keeps whatever permissions you gave it.

To disable, remove the file and restart:

```bash
sudo rm /etc/systemd/system/trawld.service.d/crashdump.conf
sudo systemctl daemon-reload && sudo systemctl restart trawld
```

The restart is the part that matters. `daemon-reload` alone rewrites the
unit's future, and the running trawld keeps `CAP_SYS_PTRACE` until something
replaces it.

Disabling stops new dumps. It does not remove the ones already written, and
each of those is still a verbatim copy of trawld's memory sitting in
`/var/lib/trawl/cores`. Nothing prunes them either, since the retain count is
applied by a trawld that is no longer capturing. When the dumps have served
their purpose, delete or archive them:

```bash
sudo find /var/lib/trawl/cores -maxdepth 1 -type f -name '*.dmp' -delete
```

`find` rather than `sudo rm .../*.dmp` because your shell expands the glob
before `sudo` runs, as you, and the directory is `0700 trawl:trawl`. The glob
matches nothing, and what happens next depends on your shell: bash passes the
literal `*.dmp` through and `rm` reports one missing file, zsh refuses the
command outright. Neither deletes anything, and the first one looks close
enough to working to be believed.

The copy under `/etc` is yours. Package upgrades never refresh it, so if a
later release changes the shipped example your installed drop-in stays as it
was. After upgrading `trawl-server`, compare the two:

```bash
diff /usr/share/doc/trawl-server/examples/crashdump.conf \
     /etc/systemd/system/trawld.service.d/crashdump.conf
```

## Enabling on kubernetes (helm)

```bash
helm upgrade trawl chart/trawl --reuse-values --set crashDump.enabled=true
```

`--reuse-values` keeps the release's existing settings; without it the upgrade
resets every other value to the chart defaults.

That one value is the whole enable step, the way copying the drop-in is on
Debian. It sets `TRAWL_CRASH_DUMP_DIR` and `TRAWL_CRASH_DUMP_RETAIN` on the
trawld container, mounts a dedicated `cores` PVC at `crashDump.mountPath`
(`/var/lib/trawl/cores` by default), and adds `SYS_PTRACE` to that container's
capabilities. The init-auth and trawl-web containers are untouched.

It requires `persistence.enabled=true`. With persistence off the chart
refuses to render rather than writing dumps to pod-local storage that
disappears with the pod. The dump PVC is deliberately separate from the data
PVC so dumps stay out of data backups. `crashDump.size`,
`crashDump.storageClass`, `crashDump.mountPath` and `crashDump.retain` are
the remaining knobs; see the
[chart README](https://github.com/jakub/trawl/blob/main/chart/trawl/README.md).

The image carries the capability and the enable step grants permission to use
it. `/usr/bin/trawld` is stamped `cap_sys_ptrace+p` at build time, permitted
only and never effective. A permitted-only stamp is inert in any process that
does not ask for the bit, so the image still runs under `docker run`'s default
capability set and under the chart's default `drop: [ALL]`. An `+ep` stamp would
not: `commoncap` fails `execve` with `EPERM` when a file's effective bit is set
and the container's bounding set lacks one of that file's permitted
capabilities, which describes every default pod.

`capabilities.add` in a pod spec is more than a bounding-set entry. containerd
hands an added capability to the container's init process as permitted and
effective, so trawld holds `CAP_SYS_PTRACE` from its first instruction. What the
grant does not survive on its own is trawld's re-exec into the monitor: an
`execve` of a file carrying no capabilities, by a process whose inheritable and
ambient sets are empty, leaves the new process with an empty permitted set.
Kubernetes has no field for the ambient set either, and the feature that would
add one, KEP-2763, is not GA. The `cap_sys_ptrace+p` stamp is what carries the
bit across that exec.

`no_new_privs` does not object to that, which is the part worth stating plainly.
`allowPrivilegeEscalation: false` sets `no_new_privs`, and the kernel enforces it
by downgrading the new credentials only when an `execve` would grant a permitted
set that is not a subset of the one the process already had. Here the file
capability hands back a bit trawld already holds, so nothing is gained and
nothing is stripped. A kind run at `ptrace_scope=2` with the pod at
`allowPrivilegeEscalation: false` gave a monitor holding `CapEff=0x80000` under
`NoNewPrivs: 1`, a `ready` verdict, and a dump with 34 threads. So the chart adds
the capability and leaves escalation alone.

That rule cuts the other way too. The bit survives only because the process
that execs `trawld` already holds it, and in the chart that process is the
container's init, since the image's `ENTRYPOINT` is `trawld` itself. Put a
shell or an init shim in front of it and the shell drops the bit at its own
exec; `trawld`'s exec is then a gain, `no_new_privs` strips it, and capture goes
dead with nothing but the `denied` verdict to say so. Keep `trawld` as the
container's first process, or grant escalation if you cannot.

The capability is still worth weighing before you enable, because neither
built-in Pod Security profile admits it. Restricted refuses every added
capability except `NET_BIND_SERVICE`. Baseline is looser but still an
allowlist, and the list is the set of capabilities a container runtime already
grants by default, which does not include `SYS_PTRACE`. Both profiles and the
exact list are in the kubernetes
[Pod Security Standards](https://kubernetes.io/docs/concepts/security/pod-security-standards/).
So relaxing a namespace from Restricted to Baseline does not help. Admission
rejects the StatefulSet either way, over the added capability.

Enabling crash dumps needs a namespace with no Pod Security enforcement, an
exemption for this workload, or an admission policy of your own that permits
`SYS_PTRACE` here. The chart documents this rather than refusing to render,
because admission policy is cluster state the chart cannot read.

The monitor raises the capability to effective before it binds its socket, and
trawld drops it from its own sets. See "The capability is the other half of the
cost" below for what that buys and what it does not.

With the capability added, kubernetes capture works at yama scope 0, 1 and 2,
the same values Debian works at. Scope 3 refuses every tracer on both channels.
Check the startup verdict under "Checking that it can work" instead of assuming
the chart's flag landed.

## What a dump contains

A minidump is raw process memory. Whatever trawld was holding when it died is
in the file: API tokens from in-flight requests, the TLS private key, the
postgres DSN and its password, event payloads, log lines. Treat a dump as a
credential, because that is what it is.

Two things follow from that. The dump directory is `0700` and each dump is
`0600`, and the Debian package ships the drop-in inert instead of enabling
capture for everyone who installs trawld.

Reading a dump on the Debian channel means being the `trawl` user or root.
Nothing else on the box qualifies, including trawl's own web proxy: since the
proxy runs as `trawl-web` rather than `trawl`, the mode alone refuses it, and
`/proc/<trawld-pid>/root` refuses it too because the kernel's ptrace check
compares uids. That mattered enough to give the proxy its own account. On
kubernetes the separation is structural instead, since the `cores` PVC only
mounts into the trawld container.

The `0700` is enforced on the packaged path only, by `systemd-tmpfiles` at
every configure and boot. Point `TRAWL_CRASH_DUMP_DIR` elsewhere and the mode
is yours to get right, for the reason given under "Enabling on Debian".

Never attach a dump to a public bug report or upload it anywhere you do not
control. If you need help reading one, share the stack summary, not the file.

### The capability is the other half of the cost

Enabling capture puts `CAP_SYS_PTRACE` inside the trawl service. Read that
literally. `CAP_SYS_PTRACE` is not "may attach to processes owned by the trawl
user". It is the capability the kernel checks *instead of* the uid comparison:
with it, `ptrace_may_access` short-circuits, and the holder can attach to any
process it can see. Root-owned services included. sshd, your database, the
agent holding your keys.

Attaching means reading and writing another process's memory and registers, so
this is not a read-only power. A compromised holder can take over a root
process rather than merely inspect it.

Which process holds it is narrower than it used to be. Both processes start
with the bit, through `AmbientCapabilities=` on Debian and through the binary's
file capability on kubernetes. At startup the monitor raises it to effective,
and trawld clears it from its own effective and permitted sets and sets
`no_new_privs` on itself. trawld execs nothing after that, so it cannot pick the
capability back up.

That limits what an ordinary bug in the query path can reach. It is not a
boundary against a compromised daemon. The monitor is trawld's own child, runs
the same binary under the same uid, and will attach to trawld on request over a
socket trawld holds. Count the capability as held by the service, and decide on
that basis.

`NoNewPrivileges=true` in the unit does not contain any of this either. It stops
a process gaining *new* privileges through `execve`, and on Debian the ambient
capability is one the service already holds before any exec.

So the honest framing is: turning on crash dumps moves trawld from an
unprivileged daemon to one that can compromise the whole host if it is
compromised itself. On a dedicated log box that is a defensible trade for being
able to debug a crash. On a machine running anything you care about
independently, it is a real decision, and it is why the package ships the
drop-in inert rather than enabling capture for every install.

One more thing to weigh on a multi-user host: the daemon and its monitor talk
over an abstract unix socket, which has no filesystem permissions, and its name
is predictable from trawld's pid, so any local process in the same network
namespace can connect to it and could race trawld to bind the name. Winning
that race no longer buys anything. trawld reads `SO_PEERCRED` on the
connection it is holding, the one a crash context would travel over, which
names the process that called `listen` on the socket that accepted it. Asking
about the name instead of the connection would not do: an impostor can bind
first, accept trawld's connection, then close its listener and let the real
monitor bind the freed name, at which point a fresh connection reports the real
monitor while the dump still goes to the impostor. If the peer of the held
connection is not the monitor trawld spawned, running as trawld's own uid,
capture does not arm at all: the startup event reports `readiness="failed"`
with `reason="monitor_identity"`, and the daemon runs on with no crash handler
installed.

The residual runs the other way. Any process under the same uid can still
connect to the monitor as a second client and ask it for a dump. What it gets
back is a dump of itself: the monitor dumps the pid the kernel reports on the
other end of the connection, never a pid the client names. Same-uid separation
is not a boundary this design draws, and the monitor is trawld's child under
trawld's uid anyway.

## yama ptrace_scope

`/proc/sys/kernel/yama/ptrace_scope` decides whether the monitor's attach is
allowed at all.

Both channels work at the same values. The mechanism differs, so the table
splits them.

| value | policy | debian | kubernetes |
|-------|--------|--------|------------|
| `0` | classic ptrace permissions | works | works |
| `1` | attach limited to declared descendants | works: trawld calls `PR_SET_PTRACER` naming its own monitor | works, same mechanism |
| `2` | admin-only attach | works: `CAP_SYS_PTRACE` from the drop-in is what "admin" means here | works: the monitor raises `CAP_SYS_PTRACE`, put in the container by `crashDump.enabled=true` and carried across the re-exec by the image's file capability |
| `3` | no attach, ever | never works. The capability does not exempt anyone | never works |

Scope 3 is a known limitation and there is no workaround: the setting is
one-way until reboot, and no privilege lifts it. If your hosts run scope 3,
crash dumps are not available there.

Scope 2 asks the tracer for `CAP_SYS_PTRACE` held effective, and both channels
now answer it. Debian's monitor gets the bit from the drop-in's ambient
capability. The kubernetes monitor raises it from the image's file capability,
which holds open a bit the container was already granted. Neither channel
is quiet when it goes wrong. The startup verdict below reports `denied` before a
crash ever happens.

## Checking that it can work

trawld probes the setup at startup and logs one verdict. Nothing about the probe
is lazy. By the time the line is written the monitor is connected, its capability
sets have been read out of `/proc`, `PR_SET_PTRACER` has been issued with its
return checked, and the daemon has sealed itself.

The ready case, wrapped here but one line in the journal:

```
INFO trawld: crash-dump capture ready; capability and yama checked, LSM policy not probed
  event_type="crash_dump" readiness="ready" ptrace_scope=2 monitor_pid=8213
  monitor_cap_eff_ptrace=true monitor_cap_prm_ptrace=true monitor_no_new_privs=true
  self_cap_eff_ptrace=false self_cap_prm_ptrace=false self_no_new_privs=true
  dumpable=true ptracer_set=true dir="/var/lib/trawl/cores" retain=10
```

Read it out of the journal:

```bash
journalctl -u trawld | grep crash_dump
```

Or query it back, since with `[ingest] internal_telemetry` on the verdict is an
ordinary trawld record:

```bash
trawl query 'service=trawld event_type=crash_dump last=24h | table _time, readiness, ptrace_scope, missing'
```

`readiness` takes four values and each one has a different next step.

| `readiness` | what it found | what to do |
|-------------|---------------|------------|
| `ready` | The capability, yama and commoncap prerequisites hold | Nothing |
| `denied` | A prerequisite is provably missing, so a crash writes a dump with zero threads | Read `ptrace_scope` and `missing` in the same event. `missing="CAP_SYS_PTRACE"` means the monitor never got the bit: a half-applied drop-in on Debian, or on kubernetes a capability that never reached the container, say because an admission policy stripped it. No `missing` at `ptrace_scope=3` means the node refuses every tracer and nothing you grant will change that |
| `indeterminate` | An input was unreadable or malformed, so there is no verdict either way | Treat capture as unknown. `/proc` being unreadable usually means a container filesystem restriction or a monitor that exited during startup. Check `monitor_pid` is alive and read the masks by hand |
| `failed` | Capture never armed. `reason` names the step that failed | `dump_dir` is a directory trawld cannot create or write. `spawn_monitor` and `monitor_unreachable` mean the re-exec did not come up. `seal` is fatal, see below |

`ready` is a necessary condition, not a promise. It covers the capability sets,
the yama scope and commoncap's exec rules. It does not cover seccomp or an LSM,
and SELinux or AppArmor can refuse the attach after all of those pass, with the
same empty-dump symptom. If the verdict says `ready` and dumps still come out
empty, audit LSM policy for trawld.

`reason="seal"` is the one failure that stops the daemon. It means trawld could
not drop `CAP_SYS_PTRACE` from its own sets or could not set `no_new_privs`, and
it exits with an error rather than serve queries holding ptrace power it said it
would give up. Every other `reason` leaves trawld running normally with capture
off.

No verdict disarms the handler. The signal handler is installed in every class,
including `denied`, because a probe that is wrong about a working host must not
be the reason you end up with no dump.

The monitor reports its own half on stderr, before it binds its socket:

```
trawl-crashdump monitor: cap_sys_ptrace raise ok
trawl-crashdump monitor: cap_sys_ptrace raise failed: Operation not permitted
```

A failed raise does not stop the monitor. It binds and serves anyway, so the
parent can read the real state and classify it, and on a scope 0 or 1 host
capture still works without the raise.

Do not force a crash to test any of this. Read the masks instead. On Debian,
check what systemd was told and then what the processes hold, because
`systemctl show` only proves the first:

```bash
systemctl show trawld -p AmbientCapabilities
grep -E 'CapEff|CapPrm|NoNewPrivs' "/proc/$(systemctl show trawld -p MainPID --value)/status"
```

Bit 19 (`0000000000080000`) being absent from the daemon's `CapEff` and `CapPrm`,
with `NoNewPrivs: 1`, is the seal working rather than a fault. Bit 19 belongs to
the monitor. Take its pid from `monitor_pid` in the verdict and read the same
fields from `/proc/<monitor_pid>/status`.

`dumpable` can read 0 even when the verdict is `ready`. An `execve` that gains a
permitted capability from the file is a privileged exec, and the kernel clears
the dumpable flag on one. A kubernetes pod reads 1, because the container's init
process already held the bit and the exec gained nothing. A `docker run` through
a shell reads 0, because the shell had dropped it and trawld's exec took it back.
Either way it only feeds the scope 0 and 1 verdict, where the attach rests on
credentials instead of the capability.

On kubernetes, check the two halves of the grant:

```bash
kubectl exec -n <ns> <pod> -c trawld -- getcap /usr/bin/trawld
kubectl get pod -n <ns> <pod> \
  -o jsonpath='{.spec.containers[?(@.name=="trawld")].securityContext}'
```

The first prints `/usr/bin/trawld cap_sys_ptrace=p`. The second must list
`SYS_PTRACE` among the added capabilities. Without it every capability set in
the container reads zero and capture is inert, whatever `getcap` prints.

When a dump is written, the monitor prints the two numbers that say whether it
is worth keeping:

```
trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1757100000000000000.dmp threads=37 memory_regions=214
```

`threads=0 memory_regions=0` is a denied capture. minidump-writer treats a
refused `PTRACE_ATTACH` as a soft error, so the monitor writes a file with a
valid header and logs this same line. Such a dump is a few tens of kilobytes
against hundreds for a real one, and no debugger can do anything with it. If the
counts cannot be read back the line says
`threads=? memory_regions=? (header unreadable: ...)`, which points at the dump
or the disk rather than at a denial.

## Retention

`TRAWL_CRASH_DUMP_RETAIN` (default 10) caps how many dumps are kept. The
pruning runs at startup, before the handler is installed, and again after
each dump is written: files ending in `.dmp` are sorted by modification time
and the oldest beyond N are deleted.

Because pruning deletes by pattern, not by an inventory of files trawld wrote,
the dump directory must be trawld's alone. Do not park anything named `*.dmp`
there, and do not point `TRAWL_CRASH_DUMP_DIR` at a shared directory.
