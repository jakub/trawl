---
title: Crash dumps
description: Enable minidump capture for trawld on Debian or Helm, find and read a dump, and look up the variables, paths, and capabilities that capture uses.
---

When trawld dies on `SIGSEGV`, `SIGABRT`, or `SIGBUS`, it can write a minidump:
a `.dmp` file with the crashed process's threads, stacks, and nearby memory. Capture is off until you enable it.
[What enabling crash dumps changes](#what-enabling-crash-dumps-changes) states
the security trade-off.

## Procedures

### Enable capture on Debian

1. Copy the packaged drop-in into place and restart trawld.

   ```bash
   sudo install -D -m 0644 \
     /usr/share/doc/trawl-server/examples/crashdump.conf \
     /etc/systemd/system/trawld.service.d/crashdump.conf
   sudo systemctl daemon-reload && sudo systemctl restart trawld
   ```

   The drop-in grants the capability and sets the two environment variables:

   ```ini
   [Service]
   AmbientCapabilities=CAP_SYS_PTRACE
   Environment=TRAWL_CRASH_DUMP_DIR=/var/lib/trawl/cores
   Environment=TRAWL_CRASH_DUMP_RETAIN=10
   ```

2. Read the startup verdict.

   ```bash
   journalctl -u trawld | grep crash_dump
   ```

   You see one line with `readiness="ready"`. Other values are under
   [Startup verdict](#startup-verdict).

To write dumps somewhere else, edit `TRAWL_CRASH_DUMP_DIR` in your copy of the
drop-in and create that directory yourself, owned by `trawl` with mode `0700`.
Do not replace `/var/lib/trawl/cores` with a symlink. `systemd-tmpfiles`
replaces anything at that path that is not a directory on the next boot.

### Enable capture on Helm

1. Set `crashDump.enabled` on the release. `persistence.enabled` must already
   be `true`, or the chart refuses to render. Set `TRAWL_IMAGE_TAG` to an
   image built from the same revision as your chart checkout. Pass that tag
   explicitly when enabling or disabling capture.

   ```bash
   helm upgrade --install "$TRAWL_RELEASE" ./chart/trawl --reuse-values \
     --set-string image.tag="${TRAWL_IMAGE_TAG:?Set the matching image tag}" \
     --set crashDump.enabled=true
   ```

2. Confirm the two halves of the grant.

   ```bash
   kubectl exec -n "$TRAWL_NAMESPACE" "$TRAWL_POD" -c trawld -- getcap /usr/bin/trawld
   kubectl get pod -n "$TRAWL_NAMESPACE" "$TRAWL_POD" \
     -o jsonpath='{.spec.containers[?(@.name=="trawld")].securityContext}'
   ```

   The first prints `/usr/bin/trawld cap_sys_ptrace=p`. The second lists
   `SYS_PTRACE` under `capabilities.add`.

3. Read the startup verdict from the trawld container log, or query it.

   ```bash
   trawl -p "$TRAWL_PROFILE" query 'service=trawld event_type=crash_dump last=24h | table _time, readiness, ptrace_scope, missing'
   ```

   `readiness` reads `ready`.

Keep `trawld` as the container's first process. A wrapper in front of it drops
the capability at its own exec, and the verdict reads `denied`. Neither
built-in Pod Security profile admits `SYS_PTRACE`, so the namespace needs no
enforcement, an exemption, or a policy of your own that allows the capability.

### Find the dumps

Dumps land in `TRAWL_CRASH_DUMP_DIR`, named `trawld-crash-<nanoseconds>.dmp`.
On Debian that is `/var/lib/trawl/cores`. On Helm it is the `cores` PVC at
`crashDump.mountPath`.

When the monitor writes a dump, it prints one line on stderr:

```text
trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1757100000000000000.dmp threads=37 memory_regions=214
```

`threads=0 memory_regions=0` means the attach was refused and the file holds
only a header. `threads=? memory_regions=? (header unreadable: ...)` points at
the file or the disk.

trawld keeps at most `TRAWL_CRASH_DUMP_RETAIN` dumps. At startup and after each
dump it deletes the oldest `*.dmp` files by modification time. Keep nothing else
named `*.dmp` in that directory.

### Read a dump

1. Confirm that the dump has content. The `wrote minidump` line reports a
   nonzero `threads` count.
2. Copy the file as the `trawl` user or root to a workstation you control. No
   other account, `trawl-web` included, can read the directory.
3. Open the file in a minidump reader.
4. When you ask for help, share the stack summary, never the file.

Read the masks instead of forcing a crash.

```bash
systemctl show trawld -p AmbientCapabilities
grep -E 'CapEff|CapPrm|NoNewPrivs' "/proc/$(systemctl show trawld -p MainPID --value)/status"
```

Bit 19, mask `0000000000080000`, is absent from trawld's `CapEff` and `CapPrm`,
and `NoNewPrivs` reads `1`. That is the seal, not a fault. The monitor, at the
verdict's `monitor_pid`, holds bit 19.

### Disable capture and delete dumps

1. Remove the grant and restart. `daemon-reload` alone leaves the running
   trawld holding `CAP_SYS_PTRACE`.

   On Debian:

   ```bash
   sudo rm /etc/systemd/system/trawld.service.d/crashdump.conf
   sudo systemctl daemon-reload && sudo systemctl restart trawld
   ```

   On Helm:

   ```bash
   helm upgrade --install "$TRAWL_RELEASE" ./chart/trawl --reuse-values \
     --set-string image.tag="${TRAWL_IMAGE_TAG:?Set the matching image tag}" \
     --set crashDump.enabled=false
   ```

2. Delete the dumps. Disabling does not remove them, and nothing prunes them
   once capture is off.

   ```bash
   sudo find /var/lib/trawl/cores -maxdepth 1 -type f -name '*.dmp' -delete
   ```

   Use `find`: your shell expands a glob as your own user, and the `0700`
   directory hides the files from it.

Dumps are outside the [backup procedure](/operate/backup-restore/).

## Reference

### Environment variables

| Variable | Default | Effect |
| --- | --- | --- |
| `TRAWL_CRASH_DUMP_DIR` | unset | Directory for `.dmp` files. Unset means capture is off. trawld creates a missing directory with mode `0700` and does not change an existing one. |
| `TRAWL_CRASH_DUMP_RETAIN` | `10` | Maximum number of `.dmp` files kept. |

Capture is Linux-only.

### Files and paths

| Path | Channel | Role |
| --- | --- | --- |
| `/usr/share/doc/trawl-server/examples/crashdump.conf` | Debian | The shipped drop-in. Inactive until copied. |
| `/etc/systemd/system/trawld.service.d/crashdump.conf` | Debian | The active drop-in. |
| `/usr/lib/tmpfiles.d/trawl.conf` | Debian | Creates `/var/lib/trawl/cores` as `0700 trawl:trawl` on every configure and boot, replacing anything there that is not a directory. |
| `/var/lib/trawl/cores` | Both | The default dump directory. |
| `/usr/bin/trawld` in the image | Helm | Carries the file capability `cap_sys_ptrace=p`. |
| `cores` PVC | Helm | Mounted at `crashDump.mountPath` in the trawld container only. Separate from the data PVC. |

The chart values are `crashDump.enabled`, `crashDump.size`,
`crashDump.storageClass`, `crashDump.mountPath`, and `crashDump.retain`. Their
defaults are `false`, `2Gi`, `""`, `/var/lib/trawl/cores`, and `10`. The
[chart README](https://github.com/jakub/trawl/blob/main/chart/trawl/README.md)
lists every value.

### Capabilities the unit grants

| Channel | Grant | Effect |
| --- | --- | --- |
| Debian | `AmbientCapabilities=CAP_SYS_PTRACE` in the drop-in | trawld and the monitor both start with the bit. `NoNewPrivileges=true` in `trawld.service` does not remove an ambient capability. |
| Helm | `SYS_PTRACE` in the trawld container's `capabilities.add`, plus `cap_sys_ptrace=p` on `/usr/bin/trawld` | The container's init process holds the bit, and the file capability carries it across trawld's exec of the monitor. `allowPrivilegeEscalation` stays `false`. Only the trawld container changes. |

After startup only the monitor holds `CAP_SYS_PTRACE` effective. trawld has
dropped it from its own sets and set `no_new_privs`.

### The monitor process

| Step | What happens |
| --- | --- |
| Start | trawld re-execs its own binary as the monitor before any thread exists. `PR_SET_PDEATHSIG` ends the monitor when trawld exits. |
| Raise | The monitor prints `trawl-crashdump monitor: cap_sys_ptrace raise ok` or `... raise failed: <error>` on stderr, then binds an abstract Unix socket named from trawld's pid. A failed raise does not stop it. |
| Identity check | trawld reads `SO_PEERCRED` on its held connection. If the peer is not its own child under its own uid, capture does not arm: `failed` with `reason="monitor_identity"`. |
| Crash | The handler writes `trawld: FATAL signal caught - writing minidump to crash-dump dir` on stderr, asks the monitor for a dump, waits for the reply, and re-raises the signal. The exit status is the original signal: `status=11/SEGV` in systemd, 139 in a shell. |
| Dump | The monitor attaches to the pid the kernel reports on the connection, never a pid the client names. It writes the file with mode `0600`, prints the `wrote minidump` line, and prunes to the retain count. |
| Malformed client | Any local process in the same network namespace can connect to the socket. A frame the monitor cannot parse makes it exit. trawld does not re-check the monitor, so the next crash writes no dump. |

### Startup verdict

trawld logs one `crash_dump` event after its tracing subscriber exists. The
event carries `readiness`, `ptrace_scope`, `monitor_pid`,
`monitor_cap_eff_ptrace`, `monitor_cap_prm_ptrace`, `monitor_no_new_privs`,
`self_cap_eff_ptrace`, `self_cap_prm_ptrace`, `self_no_new_privs`, `dumpable`,
`ptracer_set`, `dir`, `retain`, `missing`, and `reason`.

| `readiness` | Meaning | Handler installed |
| --- | --- | --- |
| `ready` | The capability, the yama scope, and the exec rules allow the attach. SELinux, AppArmor, and seccomp are not probed. | Yes |
| `denied` | A prerequisite is missing. A crash writes a dump with zero threads. `missing="CAP_SYS_PTRACE"` names the bit. No `missing` with `ptrace_scope=3` means the kernel refuses every tracer. | Yes |
| `indeterminate` | A probe input was unreadable. Check that `monitor_pid` is alive and read its masks. | Yes |
| `failed` | Capture did not arm. `reason` is `current_exe`, `dump_dir`, `spawn_monitor`, `monitor_unreachable`, `monitor_identity`, or `attach_handler`. trawld keeps running. | No |

A failed seal never reaches this event. trawld prints a line on stderr that
begins with `[trawld] crash-dump seal failed` and exits 1.

### yama `ptrace_scope`

| `/proc/sys/kernel/yama/ptrace_scope` | Debian | Helm |
| --- | --- | --- |
| `0` | Works | Works |
| `1` | Works, through `PR_SET_PTRACER` naming the monitor | Works, same mechanism |
| `2` | Works, through `CAP_SYS_PTRACE` | Works, through `CAP_SYS_PTRACE` |
| `3` | Never | Never |

Scope 3 has no workaround.

## What enabling crash dumps changes

Enabling capture places `CAP_SYS_PTRACE` inside the trawl service. That
capability replaces the kernel's uid check for `ptrace`, so its holder can read
and write the memory of any process it can see, root-owned services included.
After startup only the monitor holds the bit, and trawld cannot regain it. That
limits what an ordinary bug in the query path can reach, but it is no boundary
against a compromised daemon, because the monitor is trawld's child, runs the
same binary under the same uid, and attaches on request. Count the capability
as held by the whole service. A dump is a verbatim copy of process memory, so
the `0700` directory and the `0600` files are the only guard on the API tokens,
the TLS private key, and the database password inside it. On a dedicated log
host that trade is reasonable. On a host that runs anything else you care
about, decide with that reach in mind.
