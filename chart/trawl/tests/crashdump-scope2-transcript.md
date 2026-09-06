# crash-dump capture on kubernetes at yama ptrace_scope 2

Taken by hand on a workstation, once. There is no harness and no CI job behind
this file, by ADR-0023 ruling 8: proving the kubernetes case needs a real
cluster, a node whose kernel.yama.ptrace_scope is 2, and a root-only sysctl
write, and none of that belongs on a shared runner. `ci/crashdump-image.sh`
covers the container image at whatever scope the runner happens to have. This
covers the chart, on kubernetes, at the scope that made issue #21 worth
opening.

Two cases, both against the same image and the same cluster. The positive is
the chart as it renders with `crashDump.enabled: true`. The negative removes
CAP_SYS_PTRACE from the pod spec and leaves everything else alone, which is the
one thing that actually stops the capture.

`$WT` below is the worktree this ran from. `kubectl` and `helm` always carried
`--context kind-trawl-issue21` and `-n trawl`; the lines here are shortened to
keep them readable.


## what this run pins

    git
      image built from       3e68ed38  (no crate or Dockerfile change since)
      chart and docs at      8d7649fe
      transcript written at  bc62e32c, committed on 6cbe2a48
      the two commits in between touched CI, docs and the chart README, and
      left the trawld securityContext rendering byte-identical to the block
      recorded below.

    image
      trawl-issue21-kind:3e68ed38
      sha256:48efb49f3c7b0e60e7ec8cadbd5dd6f8ec6c404602dafb17a5e83c3a50cf0bd1

    host and tools
      kernel      7.2.3-1-cachyos
      docker      29.7.2
      kind        v0.32.0
      node image  kindest/node:v1.36.1, kubernetes v1.36.1, containerd 2.3.1
      helm        v4.2.2+gb05881c

    host changes made by hand for this run, both reverted afterwards
      1. the apparmor profile `update-alternatives` was disabled and unloaded.
         kind runs its node container privileged, so the processes inside are
         apparmor-unconfined, and an unconfined process that execs a path with
         a loaded profile gets attached to it. This host runs the apparmor.d
         full-system policy, whose `update-alternatives` profile matches
         /{,usr/}bin/update-alternatives. The kind entrypoint runs exactly that
         binary to select the iptables backend, so it came up under a profile
         written for host paths and died mapping the container's own libc:
         "cannot apply additional memory protection after relocation:
         Permission denied". The entrypoint sets errexit, so the node never
         reached systemd and no cluster could be created.
      2. kernel.yama.ptrace_scope went 1 -> 2 at 06:13:53Z, back to 1, then
         2 again at 17:04:09Z for the run recorded below, and back to 1 after.
         Nothing in this transcript writes that sysctl. It is read, in the node,
         before each phase.


## phase A: build the image, bring up the cluster and postgres

    $ docker run --rm -v $WT:/src:ro -v issue21-cargo-target:/target \
        -v issue21-cargo-registry:/usr/local/cargo/registry \
        -w /src -e CARGO_TARGET_DIR=/target rust:1.98-bookworm \
        cargo build --locked -p trawl-server --bin trawld -p fleet-admin
        Finished `dev` profile [unoptimized + debuginfo] target(s) in 3m 38s
      this builds trawld ONLY. --bin is a global target filter in cargo, not
      scoped to the -p in front of it, so fleet-admin was silently skipped and
      cargo still exited 0.

    $ docker run --rm ... rust:1.98-bookworm cargo build --locked -p fleet-admin --bin fleet-admin
        Finished `dev` profile [unoptimized + debuginfo] target(s) in 10.84s

    $ docker run --rm -v issue21-cargo-target:/target:ro -v $WT/docker-ctx/amd64:/out \
        --user 1000:1000 debian:bookworm-slim \
        sh -c 'cp /target/debug/trawld /target/debug/fleet-admin /out/'
      plus two `#!/bin/sh` stubs named trawl-admin and trawl-web, chmod +x on
      all four. The stubs exist because the Dockerfile COPY names them and
      neither runs here.

    $ strings docker-ctx/amd64/trawld | grep -c 'crash-dump capture ready; capability and yama checked'
    1

    $ docker build --platform linux/amd64 -t trawl-issue21-kind:3e68ed38 $WT
      the Dockerfile's own getcap read-back is inside the RUN, so a build that
      completes has already asserted the file capability took.

    $ docker image inspect --format '{{.Id}}' trawl-issue21-kind:3e68ed38
    sha256:48efb49f3c7b0e60e7ec8cadbd5dd6f8ec6c404602dafb17a5e83c3a50cf0bd1

    $ docker run --rm --entrypoint getcap trawl-issue21-kind:3e68ed38 /usr/bin/trawld
    /usr/bin/trawld cap_sys_ptrace=p

    $ kind create cluster --name trawl-issue21
     ✓ Preparing nodes
     ✓ Starting control-plane
     ✓ Installing CNI
     ✓ Installing StorageClass
      this is the command the apparmor profile above had to be disabled for.

    $ kind load docker-image trawl-issue21-kind:3e68ed38 --name trawl-issue21
    Image: "trawl-issue21-kind:3e68ed38" with ID "sha256:48efb49f..." not yet present on node, loading...

    $ docker exec trawl-issue21-control-plane ctr -n k8s.io run --rm --net-host \
        docker.io/library/trawl-issue21-kind:3e68ed38 capcheck getcap /usr/bin/trawld
    /usr/bin/trawld cap_sys_ptrace=p
      the security.capability xattr survived `docker save` into containerd's
      snapshot. If it had not, every case below would silently have become the
      denied one.

    $ kind load docker-image postgres:18 --name trawl-issue21
    ERROR: failed to load image: ... ctr: content digest sha256:e72a7bf80fa1...: not found
    $ printf 'FROM postgres:18\n' | docker build --platform linux/amd64 -t postgres:18-kind -
    $ kind load docker-image postgres:18-kind --name trawl-issue21
      kind imports with --all-platforms. The local postgres:18 is a
      multi-platform index whose non-amd64 blobs were never pulled, so the
      import fails on a digest that does not exist here. A one-line rebuild
      flattens it to a single platform. Same bytes, different tag, and the
      manifests below name postgres:18-kind for that reason.

    $ kubectl apply -f pg.yaml
    namespace/trawl created
    secret/fleet-db created
    secret/trawl-db created
    service/pg created
    deployment.apps/pg created
    $ kubectl -n trawl exec deploy/pg -- createdb -U trawl trawl
      fleet-db carries DATABASE_URL, trawl-db carries TRAWL_DATABASE_URL, both
      pointing at postgres://trawl:trawl@pg.trawl.svc:5432/. emptyDir storage.
      This is a throwaway keystore for one afternoon, not a deployment shape
      anyone should copy.

    $ kubectl get nodes -o wide
    NAME                          STATUS   ROLES           VERSION   OS-IMAGE                       KERNEL-VERSION            CONTAINER-RUNTIME
    trawl-issue21-control-plane   Ready    control-plane   v1.36.1   Debian GNU/Linux 13 (trixie)   7.2.3-1-cachyos (amd64)   containerd://2.3.1

    $ docker exec trawl-issue21-control-plane cat /proc/sys/kernel/yama/ptrace_scope
    2
      the node is a container on this kernel, so the node's scope is the host's.


## phase B, first attempt: what it falsified

The chart at 3e68ed38 forced `allowPrivilegeEscalation: true` whenever crash
dumps were enabled, on the reasoning that `false` sets no_new_privs and the
kernel then ignores the image's `cap_sys_ptrace+p` file capability, leaving
SYS_PTRACE in the bounding set where no non-root process can raise it. Three
probes at scope 2 said otherwise.

    1. installed as rendered, escalation true: monitor CapEff bit 19 set,
       verdict ready ptrace_scope=2, SIGSEGV wrote 33 threads / 34 regions.
    2. `kubectl patch statefulset` to allowPrivilegeEscalation false, SYS_PTRACE
       still added: monitor CapEff bit 19 STILL set, this time with the
       monitor's NoNewPrivs=1, verdict still ready, SIGSEGV wrote 34 threads /
       35 regions. The prediction was a denied verdict and an empty dump.
    3. escalation false and SYS_PTRACE removed from the pod spec: every
       capability mask zero, verdict denied with missing="CAP_SYS_PTRACE",
       SIGSEGV wrote 0 threads / 0 regions.

`crictl inspect` of case 2 shows why:

    noNewPrivileges: True
    capabilities: {"bounding": ["CAP_SYS_PTRACE"], "effective": ["CAP_SYS_PTRACE"], "permitted": ["CAP_SYS_PTRACE"]}

containerd grants an added capability to the container's init process as
permitted and effective, not bounding-only. trawld therefore holds the bit
before it execs the monitor, and commoncap lets the file capability carry it
across that exec under no_new_privs, because the new permitted set is a subset
of the old one and nothing is gained. The forced escalation was answering a
question the runtime had already answered.

Commit 8d7649fe dropped the forced escalation and amended ADR-0023 ruling 7.
Everything below is the corrected chart.


## phase B, take two: the chart as it renders

    $ docker exec trawl-issue21-control-plane cat /proc/sys/kernel/yama/ptrace_scope
    2

    $ helm upgrade trawl $WT/chart/trawl -f values-enabled.yaml --wait --timeout 5m
    Release "trawl" has been upgraded. Happy Helming!
    STATUS: deployed
    REVISION: 3
      values-enabled.yaml sets image.repository/tag/pullPolicy=Never,
      persistence.enabled, crashDump.enabled, web.enabled=false and the two
      existingSecret names. Everything else is a chart default.
      The first upgrade attempt timed out: every statefulset rollout on this
      cluster wedges the outgoing pod in Terminating while kubelet retries
      "mounted volumes=[kubernetes.io/configmap/<uid>-config]: context deadline
      exceeded" on the configmap subPath mount, with the container and sandbox
      already gone. `kubectl delete pod trawl-0 --force --grace-period=0`
      clears it and the replacement starts in about 5 seconds. That is a
      kubelet volume-teardown stall, unrelated to crash dumps, and it happened
      on every rollout in this session.

    $ helm template trawl $WT/chart/trawl -f values-enabled.yaml   # StatefulSet, container trawld
              securityContext:
                allowPrivilegeEscalation: false
                capabilities:
                  add:
                  - SYS_PTRACE
                  drop:
                  - ALL
                readOnlyRootFilesystem: true
                runAsNonRoot: true

    $ kubectl get pod trawl-0 -o jsonpath='{.spec.containers[?(@.name=="trawld")].securityContext}'
    {"allowPrivilegeEscalation":false,"capabilities":{"add":["SYS_PTRACE"],"drop":["ALL"]},"readOnlyRootFilesystem":true,"runAsNonRoot":true}
      rendered and live agree.

    $ kubectl exec trawl-0 -c trawld -- sh -c '<walk /proc for processes named trawld>'
    pid=1 ppid=0
    pid=14 ppid=1

    $ kubectl exec trawl-0 -c trawld -- cat /proc/1/status
    Name:	trawld
    Pid:	1
    PPid:	0
    Uid:	1000	1000	1000	1000
    Gid:	1000	1000	1000	1000
    CapInh:	0000000000000000
    CapPrm:	0000000000000000
    CapEff:	0000000000000000
    CapBnd:	0000000000080000
    CapAmb:	0000000000000000
    NoNewPrivs:	1
    Seccomp:	0

    $ kubectl exec trawl-0 -c trawld -- cat /proc/14/status
    Name:	trawld
    Pid:	14
    PPid:	1
    Uid:	1000	1000	1000	1000
    Gid:	1000	1000	1000	1000
    CapInh:	0000000000000000
    CapPrm:	0000000000080000
    CapEff:	0000000000080000
    CapBnd:	0000000000080000
    CapAmb:	0000000000000000
    NoNewPrivs:	1
    Seccomp:	0
      0x80000 is bit 19, CAP_SYS_PTRACE. The monitor holds it effective. The
      daemon holds it in neither effective nor permitted, so it cannot raise it
      again, and its no_new_privs is 1, so it cannot exec its way back to the
      file capability either. Both processes carry no_new_privs here, which is
      the whole point of the 8d7649fe change.

    $ kubectl logs trawl-0 -c trawld    # ANSI stripped
    trawl-crashdump monitor: cap_sys_ptrace raise ok
    2026-09-06T17:10:06.768624Z  INFO trawld: crash-dump capture ready; capability and yama checked, LSM policy not probed event_type="crash_dump" readiness="ready" ptrace_scope=2 monitor_pid=14 monitor_cap_eff_ptrace=true monitor_cap_prm_ptrace=true monitor_no_new_privs=true self_cap_eff_ptrace=false self_cap_prm_ptrace=false self_no_new_privs=true dumpable=true ptracer_set=true dir="/var/lib/trawl/cores" retain=10

    $ kubectl exec trawl-0 -c trawld -- sh -c 'rm -f /var/lib/trawl/cores/*.dmp'
      the cores PVC still held the dumps from the three falsifying probes. This
      clears it so the listings below mean what they say.

    $ kubectl exec trawl-0 -c trawld -- sh -c 'kill -SEGV 1'
      the image has no kill binary, so this is the shell builtin. pid 1 in a
      pid namespace ignores a default-disposition signal, so trawld survives
      the re-raise and keeps running. The dump is the evidence, not the exit.

    $ kubectl logs trawl-0 -c trawld    # two seconds later
    trawld: FATAL signal caught - writing minidump to crash-dump dir
    trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788714658869330323.dmp threads=34 memory_regions=35

    $ kubectl exec trawl-0 -c trawld -- ls -l /var/lib/trawl/cores
    total 804
    -rw------- 1 1000 1000 819203 Sep  6 17:10 trawld-crash-1788714658869330323.dmp


## phase B, take two: the capability removed

    $ cat no-caps-patch.json
    {"spec":{"template":{"spec":{"containers":[{"name":"trawld",
      "securityContext":{"allowPrivilegeEscalation":false,
      "capabilities":{"add":[],"drop":["ALL"]}}}]}}}}

    $ kubectl patch statefulset trawl --type strategic -p "$(cat no-caps-patch.json)"
    statefulset.apps/trawl patched
      a strategic merge, so it replaces the capability list and leaves the rest
      of the container alone. The chart cannot render this: it adds SYS_PTRACE
      whenever crash dumps are on. That is the point. An operator reaches this
      state by running with crash dumps enabled under an admission policy that
      strips the capability, or by patching it away as here.

    $ kubectl get pod trawl-0 -o jsonpath='{.spec.containers[?(@.name=="trawld")].securityContext}'
    {"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true,"runAsNonRoot":true}

    $ kubectl exec trawl-0 -c trawld -- cat /proc/1/status
    Name:	trawld
    Pid:	1
    PPid:	0
    Uid:	1000	1000	1000	1000
    Gid:	1000	1000	1000	1000
    CapInh:	0000000000000000
    CapPrm:	0000000000000000
    CapEff:	0000000000000000
    CapBnd:	0000000000000000
    CapAmb:	0000000000000000
    NoNewPrivs:	1
    Seccomp:	0

    $ kubectl exec trawl-0 -c trawld -- cat /proc/14/status
    Name:	trawld
    Pid:	14
    PPid:	1
    Uid:	1000	1000	1000	1000
    Gid:	1000	1000	1000	1000
    CapInh:	0000000000000000
    CapPrm:	0000000000000000
    CapEff:	0000000000000000
    CapBnd:	0000000000000000
    CapAmb:	0000000000000000
    NoNewPrivs:	1
    Seccomp:	0
      every mask is zero, bounding included. With nothing in the parent's
      permitted set, the monitor's exec would have to GAIN the bit from the
      file capability, and no_new_privs is exactly what forbids that.

    $ kubectl logs trawl-0 -c trawld    # ANSI stripped
    2026-09-06T17:11:24.817422Z  WARN trawld: crash-dump capture DENIED: a crash would write a minidump with no threads event_type="crash_dump" readiness="denied" ptrace_scope=2 monitor_pid=14 monitor_cap_eff_ptrace=false monitor_cap_prm_ptrace=false monitor_no_new_privs=true self_cap_eff_ptrace=false self_cap_prm_ptrace=false self_no_new_privs=true dumpable=true ptracer_set=true dir="/var/lib/trawl/cores" retain=10 missing="CAP_SYS_PTRACE"

    $ kubectl exec trawl-0 -c trawld -- sh -c 'kill -SEGV 1'

    $ kubectl logs trawl-0 -c trawld    # two seconds later
    trawld: FATAL signal caught - writing minidump to crash-dump dir
    trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788714717843145042.dmp threads=0 memory_regions=0

    $ kubectl exec trawl-0 -c trawld -- ls -l /var/lib/trawl/cores
    total 876
    -rw------- 1 1000 1000 819203 Sep  6 17:10 trawld-crash-1788714658869330323.dmp
    -rw------- 1 1000 1000  72254 Sep  6 17:11 trawld-crash-1788714717843145042.dmp
      the denied crash still wrote a dump, still 0600, and it is 72 KB against
      819 KB. minidump-writer treats a refused PTRACE_ATTACH as soft: the file
      has a valid header and no process in it.


## reading

The positive case proves the thing issue #21 said was impossible. On a node at
kernel.yama.ptrace_scope=2, with the chart's own render and no privileged
container anywhere, the monitor holds CAP_SYS_PTRACE effective while trawld
holds it in neither effective nor permitted and carries no_new_privs, and a
SIGSEGV produces a real dump: 34 threads, 35 memory regions, 819 KB, mode 0600
on the dedicated PVC. The capability reaches the monitor from the image's
`cap_sys_ptrace+p` file capability plus the pod spec's added SYS_PTRACE, and
the daemon gives it back before it does any work. The negative case proves the
verdict is honest rather than decorative. Take the capability out of the pod
spec and every mask reads zero, trawld says so at startup with
readiness="denied" and missing="CAP_SYS_PTRACE" instead of claiming to be
armed, and the crash that follows writes the 72 KB, zero-thread file that is
the signature of a refused attach. An operator who greps for the verdict learns
which of the two they are running before they need the dump.

This is a one-off, per ADR-0023 ruling 8. It is not wired into CI and there is
no script to re-run: reproducing it means a kind cluster, a root sysctl write
on a machine you own, and the commands above in order.
