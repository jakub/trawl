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
signal. systemd and the kubelet see the same exit code they always did (139
for `SIGSEGV`) and restart trawld as usual.

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
sudo rm /var/lib/trawl/cores/*.dmp
```

The copy under `/etc` is yours. Package upgrades never refresh it, so if a
later release changes the shipped example your installed drop-in stays as it
was. After upgrading `trawl-server`, compare the two:

```bash
diff /usr/share/doc/trawl-server/examples/crashdump.conf \
     /etc/systemd/system/trawld.service.d/crashdump.conf
```

## Enabling on kubernetes (helm)

```bash
helm upgrade trawl chart/trawl --set crashDump.enabled=true
```

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

What it cannot do is match the Debian channel's capability coverage, and the
difference decides whether capture works on a scope 2 node.

`capabilities.add` in a pod spec fills the container's **bounding** set. For a
container that does not run as root, that is all it fills: the effective and
permitted sets stay empty, because kubernetes has no way to hand a process an
**ambient** capability. The feature that would do it, KEP-2763, is not GA, and
ambient capabilities otherwise come from a file capability on the binary, which
the trawl image does not carry. So trawld and the monitor it re-execs run with
`SYS_PTRACE` in the bounding set and nothing in `CapEff`.

That is enough at scope 0 and scope 1, where the attach is permitted by
`PR_SET_PTRACER` naming the monitor rather than by any capability. At scope 2
the kernel wants the capability itself, the monitor does not have it, and you
get the failure this page describes below: a `.dmp` appears, the log says
`wrote minidump`, and the file holds zero threads. That is
[issue #21](https://github.com/jakub/trawl/issues/21), and the fix is a file
capability on the binary plus `allowPrivilegeEscalation`.

Until then: on kubernetes, crash dumps work at yama scope 0 and 1 and do not
work at scope 2. Check the node's `ptrace_scope` before relying on them.

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

Enabling capture grants trawld and its monitor `CAP_SYS_PTRACE`. Read that
literally. `CAP_SYS_PTRACE` is not "may attach to processes owned by the trawl
user". It is the capability the kernel checks *instead of* the uid comparison:
with it, `ptrace_may_access` short-circuits, and trawld can attach to any
process it can see. Root-owned services included. sshd, your database, the
agent holding your keys.

Attaching means reading and writing another process's memory and registers, so
this is not a read-only power. A compromised trawld with this capability can
take over a root process rather than merely inspect it.

`NoNewPrivileges=true` in the unit does not contain any of this. It stops a
process gaining *new* privileges through `execve`, and the ambient capability
is one trawld already holds.

So the honest framing is: turning on crash dumps moves trawld from an
unprivileged daemon to one that can compromise the whole host if it is
compromised itself. On a dedicated log box that is a defensible trade for being
able to debug a crash. On a machine running anything you care about
independently, it is a real decision, and it is why the package ships the
drop-in inert rather than enabling capture for every install.

One more thing to weigh on a multi-user host: the daemon and its monitor talk
over an abstract unix socket, which has no filesystem permissions, so any local
user in the same network namespace can connect to it. Hardening that is tracked
against the crashdump crate, separately from this page.

## yama ptrace_scope

`/proc/sys/kernel/yama/ptrace_scope` decides whether the monitor's attach is
allowed at all.

The two channels do not answer the same at every value, so the table splits
them.

| value | policy | debian | kubernetes |
|-------|--------|--------|------------|
| `0` | classic ptrace permissions | works | works |
| `1` | attach limited to declared descendants | works: trawld calls `PR_SET_PTRACER` naming its own monitor | works, same mechanism |
| `2` | admin-only attach | works: `CAP_SYS_PTRACE` from the drop-in is what "admin" means here | does not work yet, see [#21](https://github.com/jakub/trawl/issues/21) |
| `3` | no attach, ever | never works. The capability does not exempt anyone | never works |

Scope 3 is a known limitation and there is no workaround: the setting is
one-way until reboot, and no privilege lifts it. If your hosts run scope 3,
crash dumps are not available there.

Scope 2 on kubernetes is a different kind of gap, and a fixable one. The
capability reaches the container's bounding set and never its effective set,
for the reason given under the helm section, so the monitor's attach is refused
exactly as it would be with no capability at all. The symptom is the empty dump
described below rather than an error.

## Checking that it can work

There is no startup probe yet. trawld prints its enabled line whenever
`TRAWL_CRASH_DUMP_DIR` is set:

```
trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
```

That line means the monitor came up, not that the attach will be permitted.
The ptrace grant is issued lazily, at crash time. So under scope 3, or with a
half-applied drop-in that set the environment but not the capability, trawld
logs exactly the same thing — and worse, a crash can still produce a `.dmp`
that looks healthy from the outside. When the monitor cannot ptrace the
crashed process it writes the dump anyway, minus every thread and memory
region: a small file with a valid header and nothing a debugger can use. The
journal even records the usual `wrote minidump` line. A denied capture is
only visible by opening the dump, or by its size (tens of kilobytes against
hundreds for a real one).

Do not try to force a crash to test this. Check the capability instead:

```bash
systemctl show trawld -p AmbientCapabilities
```

The output should name `cap_sys_ptrace`. Pair that with the enabled line in
`journalctl -u trawld` and the yama value above, and you have covered every
part that can silently fall off. Issue #21 tracks turning this into a real
startup warning.

## Retention

`TRAWL_CRASH_DUMP_RETAIN` (default 10) caps how many dumps are kept. The
pruning runs at startup, before the handler is installed, and again after
each dump is written: files ending in `.dmp` are sorted by modification time
and the oldest beyond N are deleted.

Because pruning deletes by pattern, not by an inventory of files trawld wrote,
the dump directory must be trawld's alone. Do not park anything named `*.dmp`
there, and do not point `TRAWL_CRASH_DUMP_DIR` at a shared directory.
