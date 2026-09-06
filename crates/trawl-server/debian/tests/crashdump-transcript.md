

## phase 1 header

  working tree is clean

$ git -C $REPO rev-parse HEAD
21ef0c26b9fb0e7114ddef5c3ff5bccc9fd81471

argv: crashdump-harness.sh --allow-host-sysctl --out /tmp/crashdump-evidence-21ef0c26.txt

$ date -u
Sun Sep  6 01:13:36 AM UTC 2026

$ uname -r
7.2.3-1-cachyos

$ docker --version
Docker version 29.7.2, build a7dcaa6fdb
  file paths below are shown as $REPO/..., where $REPO is the worktree this ran from

pinned images:
  rust     rust:1.98-trixie@sha256:620dbcd124499c59e2406d3741574b5c5838cf9eb9656f0c3a03948f79b02959
  debian   debian:trixie@sha256:f324c7ff54321e8d9c588493a20244965938ce0aa50bbd1022d38010e9ffc4b1
  postgres postgres:18@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280
  cargo-deb 3.8.0 (see the header comment for why this is pinned)

$ cat /proc/sys/kernel/yama/ptrace_scope   # host, before anything runs
1


## phase 2 build


$ docker build trawl-crashdump-builder:cargo-deb-3.8.0   # FROM rust:1.98-trixie@sha256:620dbcd124499c59e2406d3741574b5c5838cf9eb9656f0c3a03948f79b02959
  sha256:e0be7c56705c80db19b5ea58a9c38ca98e82c2f3026c90b00bb8b5104034968e

$ docker volume create trawl-crashdump-cargo-registry
  cargo registry cache: docker volume trawl-crashdump-cargo-registry at /usr/local/cargo/registry
  CARGO_TARGET_DIR: $REPO/target/deb-harness (inside the worktree; /tmp here is a 16G tmpfs)
  STUBBED the SPA: wrote a one-line placeholder to $REPO/crates/trawl-web-ui/dist/index.html
  the embedded web UI in this .deb is a stub, not a real build
  this run created it, so teardown removes it again

$ docker run --rm --user 1000:1000 -v $REPO:/w -w /w -v trawl-crashdump-cargo-registry:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/w/target/deb-harness trawl-crashdump-builder:cargo-deb-3.8.0 cargo build --release -p trawl-server -p trawl-admin -p fleet-admin -p trawl-web --bin trawld --bin trawl-admin --bin fleet-admin --bin trawl-web
   Compiling trawl-core v0.4.0 (/w/crates/trawl-core)
   Compiling trawl-web v0.4.0 (/w/crates/trawl-web)
   Compiling trawl-engine v0.4.0 (/w/crates/trawl-engine)
   Compiling trawl-server v0.4.0 (/w/crates/trawl-server)
   Compiling trawl-admin v0.4.0 (/w/crates/trawl-admin)
    Finished `release` profile [optimized] target(s) in 35.51s

$ docker run --rm --user 1000:1000 -v $REPO:/w -w /w -v trawl-crashdump-cargo-registry:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/w/target/deb-harness trawl-crashdump-builder:cargo-deb-3.8.0 cargo deb -p trawl-server --no-build --no-strip
/w/target/deb-harness/debian/trawl-server_0.4.0-1_amd64.deb

$ sha256sum $REPO/target/deb-harness/debian/trawl-server_0.4.0-1_amd64.deb
af31c6f0e397fb2fe90e948eab72f9a3d744ff5e8b4749b8baa66e1a265aa2f1  $REPO/target/deb-harness/debian/trawl-server_0.4.0-1_amd64.deb

$ dpkg-deb -f $REPO/target/deb-harness/debian/trawl-server_0.4.0-1_amd64.deb Package Version Architecture Depends
Package: trawl-server
Version: 0.4.0-1
Architecture: amd64
Depends: libc6 (>= 2.34), libc6 (>= 2.39), libstdc++6 (>= 14)


## phase 3 up


$ docker network create trawl-crashdump-net

$ docker run -d --name trawl-crashdump-pg --network trawl-crashdump-net postgres:18@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280
  postgres accepting connections after 2s

$ docker exec trawl-crashdump-pg psql -U postgres -v ON_ERROR_STOP=1 -c CREATE ROLE fleet LOGIN PASSWORD 'fleetpw'; -c CREATE DATABASE fleet OWNER fleet; -c CREATE ROLE trawl LOGIN PASSWORD 'trawlpw'; -c CREATE DATABASE trawl OWNER trawl;
CREATE ROLE
CREATE DATABASE
CREATE ROLE
CREATE DATABASE

$ docker build trawl-crashdump-node:trixie   # FROM debian:trixie@sha256:f324c7ff54321e8d9c588493a20244965938ce0aa50bbd1022d38010e9ffc4b1
  sha256:c0cc5500e931e55cbfdbba1ce4a88152cae5e5daac6747d20d956c243936d81b

$ docker run -d --name trawl-crashdump-node --privileged --cgroupns=private --tmpfs /run --tmpfs /tmp --network trawl-crashdump-net trawl-crashdump-node:trixie /sbin/init
  systemd up in trawl-crashdump-node after 2s

$ docker exec trawl-crashdump-node systemctl is-system-running
running

[trawl-crashdump-node]
$ dpkg-query -W -f='  ${Package} ${Version}\n' systemd gdb sudo procps libc6
  gdb 16.3-1
  libc6 2.41-12+deb13u3
  procps 2:4.0.4-9
  sudo 1.9.16p2-3+deb13u2
  systemd 257.13-1~deb13u1

$ docker exec trawl-crashdump-pg postgres --version
postgres (PostgreSQL) 18.6 (Debian 18.6-1.pgdg13+2)


## phase 4 install


$ docker cp $REPO/target/deb-harness/debian/trawl-server_0.4.0-1_amd64.deb trawl-crashdump-node:/root/trawl-server.deb

[trawl-crashdump-node]
$ set -o pipefail
$ DEBIAN_FRONTEND=noninteractive apt-get install -y /root/trawl-server.deb 2>&1 | tail -8
(Reading database ... (Reading database ... 5%(Reading database ... 10%(Reading database ... 15%(Reading database ... 20%(Reading database ... 25%(Reading database ... 30%(Reading database ... 35%(Reading database ... 40%(Reading database ... 45%(Reading database ... 50%(Reading database ... 55%(Reading database ... 60%(Reading database ... 65%(Reading database ... 70%(Reading database ... 75%(Reading database ... 80%(Reading database ... 85%(Reading database ... 90%(Reading database ... 95%(Reading database ... 100%(Reading database ... 8408 files and directories currently installed.)
Preparing to unpack /root/trawl-server.deb ...
Unpacking trawl-server (0.4.0-1) ...
Setting up trawl-server (0.4.0-1) ...
Created symlink '/etc/systemd/system/multi-user.target.wants/trawld.service' → '/usr/lib/systemd/system/trawld.service'.
/usr/sbin/policy-rc.d returned 101, not running 'start trawld.service'
Created symlink '/etc/systemd/system/multi-user.target.wants/trawl-web.service' → '/usr/lib/systemd/system/trawl-web.service'.
/usr/sbin/policy-rc.d returned 101, not running 'start trawl-web.service'

$ dpkg-query -W -f='${Status}' trawl-server
install ok installed

$ dpkg-deb -c trawl-server_0.4.0-1_amd64.deb | grep examples/crashdump.conf
-rw-r--r-- 0/0             674 2026-09-05 17:00 ./usr/share/doc/trawl-server/examples/crashdump.conf

[trawl-crashdump-node]
$ cat >> /etc/default/trawld <<'EOF'
$ FLEET_DATABASE_URL=postgres://fleet:fleetpw@trawl-crashdump-pg:5432/fleet
$ TRAWL_DATABASE_URL=postgres://trawl:trawlpw@trawl-crashdump-pg:5432/trawl
$ EOF
$ chown root:trawl /etc/default/trawld && chmod 0640 /etc/default/trawld
$ grep -c '^[A-Z_]*DATABASE_URL=' /etc/default/trawld
2

$ docker exec -e DATABASE_URL=... trawl-crashdump-node /usr/bin/fleet-admin migrate
fleet-admin: migrations applied

$ docker exec trawl-crashdump-node systemctl restart trawld
  trawld is-active=active after 1s

$ docker exec trawl-crashdump-node systemctl show trawld -p MainPID -p ActiveState -p SubState
MainPID=258
ActiveState=active
SubState=running


## phase 5 A inert


$ docker exec trawl-crashdump-node systemctl show trawld -p AmbientCapabilities -p Environment
Environment=
AmbientCapabilities=

$ test -e /etc/systemd/system/trawld.service.d
  absent — the package installs no drop-in
  trawld   pid=258    CapEff=0x0000000000000000 bit19=0  CapAmb=0x0000000000000000 bit19=0  NoNewPrivs=1
  CAP_SYS_PTRACE (bit 19) is clear in both CapEff and CapAmb

$ docker exec trawl-crashdump-node stat -c %F %U %G %a /var/lib/trawl/cores
directory trawl trawl 700


## phase 6 B enable

  enable command matches $REPO/docs/src/content/docs/reference/crash-dumps.md

$ docker exec -i trawl-crashdump-node bash -s   # the docs enable command, verbatim:
sudo install -D -m 0644 \
  /usr/share/doc/trawl-server/examples/crashdump.conf \
  /etc/systemd/system/trawld.service.d/crashdump.conf
sudo systemctl daemon-reload && sudo systemctl restart trawld
  trawld is-active=active after 1s

$ docker exec trawl-crashdump-node systemctl show trawld -p AmbientCapabilities -p Environment
Environment=TRAWL_CRASH_DUMP_DIR=/var/lib/trawl/cores TRAWL_CRASH_DUMP_RETAIN=10
AmbientCapabilities=cap_sys_ptrace


## phase C/D fault at yama ptrace_scope=1


$ docker exec trawl-crashdump-node cat /proc/sys/kernel/yama/ptrace_scope   # shared kernel: this IS the host value
1

$ docker exec trawl-crashdump-node systemctl restart trawld
  trawld is-active=active after 1s

$ docker exec trawl-crashdump-node cat /proc/sys/kernel/yama/ptrace_scope
1
  monitor process found after 1s

$ tr "\0" "\n" < /proc/470/environ | grep TRAWL_CRASHDUMP_MONITOR
TRAWL_CRASHDUMP_MONITOR=1

capabilities (bit 19 = CAP_SYS_PTRACE):
  daemon   pid=461    CapEff=0x0000000000080000 bit19=1  CapAmb=0x0000000000080000 bit19=1  NoNewPrivs=1
  monitor  pid=470    CapEff=0x0000000000080000 bit19=1  CapAmb=0x0000000000080000 bit19=1  NoNewPrivs=1
  dumps before the fault: 0   NRestarts=0

$ docker exec trawl-crashdump-node gdb -q -n -batch -p 461 -ex 'set $pc = 0' -ex detach
Using host libthread_db library "/lib/x86_64-linux-gnu/libthread_db.so.1".
0x00007fcdf8a409ee in ?? () from target:/lib/x86_64-linux-gnu/libc.so.6
[Inferior 1 (process 461) detached]
  a new dump appeared after 1s

$ /root/mdmp-summary /var/lib/trawl/cores/trawld-crash-1788657269193137251.dmp
path=/var/lib/trawl/cores/trawld-crash-1788657269193137251.dmp magic=MDMP bytes=723497 owner=trawl:trawl mode=600 streams=18 threads=33 memory_regions=33
  scope 1: dumps 0 -> 1, 33 threads captured

[trawl-crashdump-node]
$ journalctl -u trawld --no-pager --since '-3min' | grep -iE 'crashdump|minidump|FATAL signal|Main process exited|Scheduled restart' | tail -12
Sep 06 01:14:28 0eee13ae84fd trawld[392]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 01:14:28 0eee13ae84fd trawld[461]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 01:14:29 0eee13ae84fd trawld[461]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 01:14:29 0eee13ae84fd trawld[470]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788657269193137251.dmp
Sep 06 01:14:29 0eee13ae84fd systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
  trawld is-active=active after 6s
  NRestarts 0 -> 1 (Restart=on-failure brought trawld back)


## phase C/D fault at yama ptrace_scope=2


$ docker exec trawl-crashdump-node cat /proc/sys/kernel/yama/ptrace_scope   # shared kernel: this IS the host value
1

$ docker exec trawl-crashdump-node bash -c 'echo 2 > /proc/sys/kernel/yama/ptrace_scope'
  raised 1 -> 2, confirmed by readback (restored to 1 by the exit trap)

$ docker exec trawl-crashdump-node systemctl restart trawld
  trawld is-active=active after 1s

$ docker exec trawl-crashdump-node cat /proc/sys/kernel/yama/ptrace_scope
2
  monitor process found after 1s

$ tr "\0" "\n" < /proc/1636/environ | grep TRAWL_CRASHDUMP_MONITOR
TRAWL_CRASHDUMP_MONITOR=1

capabilities (bit 19 = CAP_SYS_PTRACE):
  daemon   pid=1627   CapEff=0x0000000000080000 bit19=1  CapAmb=0x0000000000080000 bit19=1  NoNewPrivs=1
  monitor  pid=1636   CapEff=0x0000000000080000 bit19=1  CapAmb=0x0000000000080000 bit19=1  NoNewPrivs=1
  dumps before the fault: 1   NRestarts=0

$ docker exec trawl-crashdump-node gdb -q -n -batch -p 1627 -ex 'set $pc = 0' -ex detach
Using host libthread_db library "/lib/x86_64-linux-gnu/libthread_db.so.1".
0x00007f1ce4de07b9 in syscall () from target:/lib/x86_64-linux-gnu/libc.so.6
[Inferior 1 (process 1627) detached]
  a new dump appeared after 1s

$ /root/mdmp-summary /var/lib/trawl/cores/trawld-crash-1788657275642362783.dmp
path=/var/lib/trawl/cores/trawld-crash-1788657275642362783.dmp magic=MDMP bytes=710600 owner=trawl:trawl mode=600 streams=18 threads=35 memory_regions=35
  scope 2: dumps 1 -> 2, 35 threads captured

[trawl-crashdump-node]
$ journalctl -u trawld --no-pager --since '-3min' | grep -iE 'crashdump|minidump|FATAL signal|Main process exited|Scheduled restart' | tail -12
Sep 06 01:14:28 0eee13ae84fd trawld[392]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 01:14:28 0eee13ae84fd trawld[461]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 01:14:29 0eee13ae84fd trawld[461]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 01:14:29 0eee13ae84fd trawld[470]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788657269193137251.dmp
Sep 06 01:14:29 0eee13ae84fd systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
Sep 06 01:14:34 0eee13ae84fd systemd[1]: trawld.service: Scheduled restart job, restart counter is at 1.
Sep 06 01:14:34 0eee13ae84fd trawld[1470]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 01:14:35 0eee13ae84fd trawld[1627]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 01:14:35 0eee13ae84fd trawld[1627]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 01:14:35 0eee13ae84fd trawld[1636]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788657275642362783.dmp
Sep 06 01:14:35 0eee13ae84fd systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
  trawld is-active=active after 6s
  NRestarts 0 -> 1 (Restart=on-failure brought trawld back)


## phase E negative (capability removed)


$ docker exec trawl-crashdump-node cat /proc/sys/kernel/yama/ptrace_scope   # shared kernel: this IS the host value
2

  Control: the same drop-in with AmbientCapabilities removed. The two
  Environment lines stay, so trawld still starts its monitor and still logs
  "trawl-crashdump: enabled". The assertion is that NO dump created by this
  crash carries thread data. A zero-thread file is a pass, and is what
  actually happens.

[trawl-crashdump-node]
$ cat > /etc/systemd/system/trawld.service.d/crashdump.conf <<'EOF'
$ [Service]
$ Environment=TRAWL_CRASH_DUMP_DIR=/var/lib/trawl/cores
$ Environment=TRAWL_CRASH_DUMP_RETAIN=10
$ EOF
$ cat /etc/systemd/system/trawld.service.d/crashdump.conf
[Service]
Environment=TRAWL_CRASH_DUMP_DIR=/var/lib/trawl/cores
Environment=TRAWL_CRASH_DUMP_RETAIN=10

$ docker exec trawl-crashdump-node systemctl daemon-reload

$ docker exec trawl-crashdump-node systemctl restart trawld
  trawld is-active=active after 1s

$ docker exec trawl-crashdump-node systemctl show trawld -p AmbientCapabilities -p Environment
Environment=TRAWL_CRASH_DUMP_DIR=/var/lib/trawl/cores TRAWL_CRASH_DUMP_RETAIN=10
AmbientCapabilities=
  monitor process found after 1s
  daemon   pid=2775   CapEff=0x0000000000000000 bit19=0  CapAmb=0x0000000000000000 bit19=0  NoNewPrivs=1
  monitor  pid=2784   CapEff=0x0000000000000000 bit19=0  CapAmb=0x0000000000000000 bit19=0  NoNewPrivs=1
  dumps before the fault: 2   NRestarts=0

$ docker exec trawl-crashdump-node gdb -q -n -batch -p 2775 -ex 'set $pc = 0' -ex detach
Using host libthread_db library "/lib/x86_64-linux-gnu/libthread_db.so.1".
0x00007f8da9b377b9 in syscall () from target:/lib/x86_64-linux-gnu/libc.so.6
[Inferior 1 (process 2775) detached]
  faulted pid 2775 is gone after 1s
  systemd restarted the unit after 6s
  settling 15s before judging (a working capture lands within a second of the fault)
  1 new dump(s) from this crash; every one must be empty of thread data

$ /root/mdmp-summary /var/lib/trawl/cores/trawld-crash-1788657282161344800.dmp
path=/var/lib/trawl/cores/trawld-crash-1788657282161344800.dmp magic=MDMP bytes=71023 owner=trawl:trawl mode=600 streams=18 threads=0 memory_regions=0
  outcome: 1 file(s) appeared, all with 0 threads and 0 memory regions

[trawl-crashdump-node]
$ journalctl -u trawld --no-pager --since '-3min' | grep -iE 'crashdump|minidump|FATAL signal|Main process exited|Scheduled restart' | tail -12
Sep 06 01:14:35 0eee13ae84fd trawld[1627]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 01:14:35 0eee13ae84fd trawld[1627]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 01:14:35 0eee13ae84fd trawld[1636]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788657275642362783.dmp
Sep 06 01:14:35 0eee13ae84fd systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
Sep 06 01:14:40 0eee13ae84fd systemd[1]: trawld.service: Scheduled restart job, restart counter is at 1.
Sep 06 01:14:40 0eee13ae84fd trawld[2656]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 01:14:41 0eee13ae84fd trawld[2775]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 01:14:42 0eee13ae84fd trawld[2775]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 01:14:42 0eee13ae84fd trawld[2784]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788657282161344800.dmp
Sep 06 01:14:42 0eee13ae84fd systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
Sep 06 01:14:47 0eee13ae84fd systemd[1]: trawld.service: Scheduled restart job, restart counter is at 1.
Sep 06 01:14:47 0eee13ae84fd trawld[2981]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
  restoring the shipped drop-in

[trawl-crashdump-node]
$ install -D -m 0644 /usr/share/doc/trawl-server/examples/crashdump.conf /etc/systemd/system/trawld.service.d/crashdump.conf && systemctl daemon-reload && systemctl restart trawld
  trawld is-active=active after 1s

$ docker exec trawl-crashdump-node systemctl show trawld -p AmbientCapabilities
AmbientCapabilities=cap_sys_ptrace


## phase result

  crash cases executed: 2 of 2 requested (1 2)
  every assertion passed


## phase down: teardown

  restoring host ptrace_scope to 1
  removing the stub SPA this run created: $REPO/crates/trawl-web-ui/dist/index.html

$ docker rm -f trawl-crashdump-node trawl-crashdump-pg

$ docker network rm trawl-crashdump-net

$ cat /proc/sys/kernel/yama/ptrace_scope   # host, after restore
1

exit status: 0
