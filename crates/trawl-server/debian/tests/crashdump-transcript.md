

## phase 1 header

  working tree is clean

$ git -C $REPO rev-parse HEAD
d5a93878c400cfc490414e33bfde0171ed5889d7

argv: crashdump-harness.sh --allow-host-sysctl --out /tmp/crashdump-evidence-round5.txt

$ date -u
Sun Sep  6 02:30:49 AM UTC 2026

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
  sha256:9d9771ad36e5d09a20771e96d4ef64ee158ff34e5e78556c8fdd93a875dc5b33

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
    Finished `release` profile [optimized] target(s) in 35.23s
  clearing .deb files left in $REPO/target/deb-harness/debian by an earlier run

$ docker run --rm --user 1000:1000 -v $REPO:/w -w /w -v trawl-crashdump-cargo-registry:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/w/target/deb-harness trawl-crashdump-builder:cargo-deb-3.8.0 cargo deb -p trawl-server --no-build --no-strip
/w/target/deb-harness/debian/trawl-server_0.4.0-1_amd64.deb
  installing trawl-server_0.4.0-1_amd64.deb

$ sha256sum $REPO/target/deb-harness/debian/trawl-server_0.4.0-1_amd64.deb
e78133694ab71a6a81ef0a1fddac7cd7fa7059051e4337ff6eadeca608ed9139  $REPO/target/deb-harness/debian/trawl-server_0.4.0-1_amd64.deb

$ dpkg-deb -f $REPO/target/deb-harness/debian/trawl-server_0.4.0-1_amd64.deb Package Version Architecture Depends
Package: trawl-server
Version: 0.4.0-1
Architecture: amd64
Depends: libc6 (>= 2.34), libc6 (>= 2.39), libstdc++6 (>= 14)


## phase 3 up


$ docker network create --label trawl-crashdump-harness-run=3531226-1788661849 trawl-crashdump-net
  network id 1aa61c10daa3

$ docker run -d --name trawl-crashdump-pg --network trawl-crashdump-net --label trawl-crashdump-harness-run=3531226-1788661849 postgres:18@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280
  container id 5e113c883661
  postgres accepting connections after 2s

$ docker exec trawl-crashdump-pg psql -U postgres -v ON_ERROR_STOP=1 -c CREATE ROLE fleet LOGIN PASSWORD 'fleetpw'; -c CREATE DATABASE fleet OWNER fleet; -c CREATE ROLE trawl LOGIN PASSWORD 'trawlpw'; -c CREATE DATABASE trawl OWNER trawl;
CREATE ROLE
CREATE DATABASE
CREATE ROLE
CREATE DATABASE

$ docker build trawl-crashdump-node:trixie   # FROM debian:trixie@sha256:f324c7ff54321e8d9c588493a20244965938ce0aa50bbd1022d38010e9ffc4b1
  sha256:a343695ffcef453d85056ba25b2413d5686ac36e01e882114177a2d503d28b19

$ docker run -d --name trawl-crashdump-node --privileged --cgroupns=private --tmpfs /run --tmpfs /tmp --network trawl-crashdump-net --label trawl-crashdump-harness-run=3531226-1788661849 trawl-crashdump-node:trixie /sbin/init
  container id cc7f1984dfe1
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

[trawl-crashdump-node]
$ rmdir /var/lib/trawl/cores && : > /var/lib/trawl/cores && stat -c 'planted: %F' /var/lib/trawl/cores
$ systemd-tmpfiles --create trawl.conf
$ stat -c 'after tmpfiles: %F %U:%G %a' /var/lib/trawl/cores
planted: regular empty file
Wrong file type 0o100000; rm -rf "/var/lib/trawl/cores"
after tmpfiles: directory trawl:trawl 700
  systemd 257 replaced the planted file with the directory, so 'd=' is honoured here

$ docker exec trawl-crashdump-node systemctl show trawl-web -p InaccessiblePaths
InaccessiblePaths=/var/lib/trawl/cores

$ docker exec trawl-crashdump-node systemctl restart trawl-web
  trawl-web is-active=active after 1s

[trawl-crashdump-node]
$ ps -o pid,user,group,args -p $(systemctl show trawld -p MainPID --value) -p $(systemctl show trawl-web -p MainPID --value)
$ id trawl-web
$ stat -c '%n %U:%G %a' /var/lib/trawl /var/lib/trawl/web.cookie /var/lib/trawl/cores /etc/trawl/trawld.toml
    PID USER     GROUP    COMMAND
    258 trawl    trawl    /usr/bin/trawld --config /etc/trawl/trawld.toml --no-monitor
    400 trawl-w+ trawl    /usr/bin/trawl-web --config /etc/trawl/trawld.toml
uid=995(trawl-web) gid=995(trawl-web) groups=995(trawl-web),996(trawl)
/var/lib/trawl trawl:trawl 755
/var/lib/trawl/web.cookie trawl:trawl 640
/var/lib/trawl/cores trawl:trawl 700
/etc/trawl/trawld.toml root:trawl 640
  the proxy runs as trawl-web; the dump directory is trawl:trawl 0700, denying that uid by mode as well

[trawl-crashdump-node]
$ install -o trawl -g trawl -m 0600 /dev/null /var/lib/trawl/cores/sentinel-not-a-dump
$ printf 'real directory:      '; ls -A /var/lib/trawl/cores | tr '\n' ' '; echo
$ printf 'trawl-web sees:      '; ls -A /proc/400/root/var/lib/trawl/cores | tr '\n' ' '; echo '(nothing)'
$ stat -c 'web.cookie in that namespace: %n %U %a' /proc/400/root/var/lib/trawl/web.cookie
real directory:      sentinel-not-a-dump 
trawl-web sees:      (nothing)
web.cookie in that namespace: /proc/400/root/var/lib/trawl/web.cookie trawl 640
  the sentinel is invisible inside trawl-web's mount namespace, web.cookie is not


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

$ tr "\0" "\n" < /proc/699/environ | grep TRAWL_CRASHDUMP_MONITOR
TRAWL_CRASHDUMP_MONITOR=1

capabilities (bit 19 = CAP_SYS_PTRACE):
  daemon   pid=690    CapEff=0x0000000000080000 bit19=1  CapAmb=0x0000000000080000 bit19=1  NoNewPrivs=1
  monitor  pid=699    CapEff=0x0000000000080000 bit19=1  CapAmb=0x0000000000080000 bit19=1  NoNewPrivs=1
  dumps before the fault: 0   NRestarts=0

$ docker exec trawl-crashdump-node gdb -q -n -batch -p 690 -ex 'set $pc = 0' -ex detach
Using host libthread_db library "/lib/x86_64-linux-gnu/libthread_db.so.1".
0x00007f26d5daf7b9 in syscall () from target:/lib/x86_64-linux-gnu/libc.so.6
[Inferior 1 (process 690) detached]
  a new dump appeared after 1s

$ /root/mdmp-summary /var/lib/trawl/cores/trawld-crash-1788661904739507039.dmp
path=/var/lib/trawl/cores/trawld-crash-1788661904739507039.dmp magic=MDMP bytes=701770 owner=trawl:trawl mode=600 streams=18 threads=34 memory_regions=34
  scope 1: dumps 0 -> 1, 34 threads captured

[trawl-crashdump-node]
$ journalctl -u trawld --no-pager --since '-3min' | grep -iE 'crashdump|minidump|FATAL signal|Main process exited|Scheduled restart' | tail -12
Sep 06 02:31:43 cc7f1984dfe1 trawld[599]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 02:31:44 cc7f1984dfe1 trawld[690]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 02:31:44 cc7f1984dfe1 trawld[690]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 02:31:44 cc7f1984dfe1 trawld[699]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788661904739507039.dmp
Sep 06 02:31:44 cc7f1984dfe1 systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
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

$ tr "\0" "\n" < /proc/1836/environ | grep TRAWL_CRASHDUMP_MONITOR
TRAWL_CRASHDUMP_MONITOR=1

capabilities (bit 19 = CAP_SYS_PTRACE):
  daemon   pid=1827   CapEff=0x0000000000080000 bit19=1  CapAmb=0x0000000000080000 bit19=1  NoNewPrivs=1
  monitor  pid=1836   CapEff=0x0000000000080000 bit19=1  CapAmb=0x0000000000080000 bit19=1  NoNewPrivs=1
  dumps before the fault: 1   NRestarts=0

$ docker exec trawl-crashdump-node gdb -q -n -batch -p 1827 -ex 'set $pc = 0' -ex detach
Using host libthread_db library "/lib/x86_64-linux-gnu/libthread_db.so.1".
0x00007f71f0e047b9 in syscall () from target:/lib/x86_64-linux-gnu/libc.so.6
[Inferior 1 (process 1827) detached]
  a new dump appeared after 1s

$ /root/mdmp-summary /var/lib/trawl/cores/trawld-crash-1788661911236257432.dmp
path=/var/lib/trawl/cores/trawld-crash-1788661911236257432.dmp magic=MDMP bytes=692709 owner=trawl:trawl mode=600 streams=18 threads=34 memory_regions=34
  scope 2: dumps 1 -> 2, 34 threads captured

[trawl-crashdump-node]
$ journalctl -u trawld --no-pager --since '-3min' | grep -iE 'crashdump|minidump|FATAL signal|Main process exited|Scheduled restart' | tail -12
Sep 06 02:31:43 cc7f1984dfe1 trawld[599]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 02:31:44 cc7f1984dfe1 trawld[690]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 02:31:44 cc7f1984dfe1 trawld[690]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 02:31:44 cc7f1984dfe1 trawld[699]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788661904739507039.dmp
Sep 06 02:31:44 cc7f1984dfe1 systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
Sep 06 02:31:49 cc7f1984dfe1 systemd[1]: trawld.service: Scheduled restart job, restart counter is at 1.
Sep 06 02:31:49 cc7f1984dfe1 trawld[1726]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 02:31:50 cc7f1984dfe1 trawld[1827]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 02:31:51 cc7f1984dfe1 trawld[1827]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 02:31:51 cc7f1984dfe1 trawld[1836]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788661911236257432.dmp
Sep 06 02:31:51 cc7f1984dfe1 systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
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
  daemon   pid=2974   CapEff=0x0000000000000000 bit19=0  CapAmb=0x0000000000000000 bit19=0  NoNewPrivs=1
  monitor  pid=2983   CapEff=0x0000000000000000 bit19=0  CapAmb=0x0000000000000000 bit19=0  NoNewPrivs=1
  dumps before the fault: 2   NRestarts=0

$ docker exec trawl-crashdump-node gdb -q -n -batch -p 2974 -ex 'set $pc = 0' -ex detach
Using host libthread_db library "/lib/x86_64-linux-gnu/libthread_db.so.1".
0x00007f6df8aa87b9 in syscall () from target:/lib/x86_64-linux-gnu/libc.so.6
[Inferior 1 (process 2974) detached]
  faulted pid 2974 is gone after 1s
  systemd restarted the unit after 6s
  settling 15s before judging (a working capture lands within a second of the fault)
  1 new dump(s) from this crash; every one must be empty of thread data

$ /root/mdmp-summary /var/lib/trawl/cores/trawld-crash-1788661917824886720.dmp
path=/var/lib/trawl/cores/trawld-crash-1788661917824886720.dmp magic=MDMP bytes=70148 owner=trawl:trawl mode=600 streams=18 threads=0 memory_regions=0
  outcome: 1 file(s) appeared, all with 0 threads and 0 memory regions

[trawl-crashdump-node]
$ journalctl -u trawld --no-pager --since '-3min' | grep -iE 'crashdump|minidump|FATAL signal|Main process exited|Scheduled restart' | tail -12
Sep 06 02:31:50 cc7f1984dfe1 trawld[1827]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 02:31:51 cc7f1984dfe1 trawld[1827]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 02:31:51 cc7f1984dfe1 trawld[1836]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788661911236257432.dmp
Sep 06 02:31:51 cc7f1984dfe1 systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
Sep 06 02:31:56 cc7f1984dfe1 systemd[1]: trawld.service: Scheduled restart job, restart counter is at 1.
Sep 06 02:31:56 cc7f1984dfe1 trawld[2855]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 02:31:57 cc7f1984dfe1 trawld[2974]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
Sep 06 02:31:57 cc7f1984dfe1 trawld[2974]: trawld: FATAL signal caught - writing minidump to crash-dump dir
Sep 06 02:31:57 cc7f1984dfe1 trawld[2983]: trawl-crashdump: wrote minidump /var/lib/trawl/cores/trawld-crash-1788661917824886720.dmp
Sep 06 02:31:57 cc7f1984dfe1 systemd[1]: trawld.service: Main process exited, code=killed, status=11/SEGV
Sep 06 02:32:03 cc7f1984dfe1 systemd[1]: trawld.service: Scheduled restart job, restart counter is at 1.
Sep 06 02:32:03 cc7f1984dfe1 trawld[3180]: trawl-crashdump: enabled (dir=/var/lib/trawl/cores, retain=10)
  restoring the shipped drop-in

[trawl-crashdump-node]
$ install -D -m 0644 /usr/share/doc/trawl-server/examples/crashdump.conf /etc/systemd/system/trawld.service.d/crashdump.conf && systemctl daemon-reload && systemctl restart trawld
  trawld is-active=active after 1s

$ docker exec trawl-crashdump-node systemctl show trawld -p AmbientCapabilities
AmbientCapabilities=cap_sys_ptrace


## phase F attack direction (trawl-web against the dumps)

  disable command matches $REPO/docs/src/content/docs/reference/crash-dumps.md

$ docker exec -i trawl-crashdump-node bash -s   # the docs disable command, verbatim:
sudo rm /etc/systemd/system/trawld.service.d/crashdump.conf
sudo systemctl daemon-reload && sudo systemctl restart trawld
  trawld is-active=active after 1s

$ docker exec trawl-crashdump-node systemctl show trawld -p AmbientCapabilities -p Environment
Environment=
AmbientCapabilities=
  daemon   pid=3820   CapEff=0x0000000000000000 bit19=0  CapAmb=0x0000000000000000 bit19=0  NoNewPrivs=1
  capture is off, trawld is cap-less, and trawld-crash-1788661917824886720.dmp is still on disk

[trawl-crashdump-node]
$ id trawl-web
uid=995(trawl-web) gid=995(trawl-web) groups=995(trawl-web),996(trawl)

$ runuser -u trawl-web -- ls -l /var/lib/trawl/cores
ls: cannot open directory '/var/lib/trawl/cores': Permission denied
exit=2
  listing the dump directory directly denied

$ runuser -u trawl-web -- cat /var/lib/trawl/cores/trawld-crash-1788661917824886720.dmp
cat: /var/lib/trawl/cores/trawld-crash-1788661917824886720.dmp: Permission denied
exit=1
  reading a dump directly denied

$ runuser -u trawl-web -- ls -l /proc/3820/root/var/lib/trawl/cores
ls: cannot access '/proc/3820/root/var/lib/trawl/cores': Permission denied
exit=2
  listing the dump directory through /proc/3820/root denied

$ runuser -u trawl-web -- cat /proc/3820/root/var/lib/trawl/cores/trawld-crash-1788661917824886720.dmp
cat: /proc/3820/root/var/lib/trawl/cores/trawld-crash-1788661917824886720.dmp: Permission denied
exit=1
  reading a dump through /proc/3820/root denied

$ runuser -u trawl -- ls -l /var/lib/trawl/cores   # control: the owner can still read them
total 1440
-rw------- 1 trawl trawl 701770 Sep  6 02:31 trawld-crash-1788661904739507039.dmp
-rw------- 1 trawl trawl 692709 Sep  6 02:31 trawld-crash-1788661911236257432.dmp
-rw------- 1 trawl trawl  70148 Sep  6 02:31 trawld-crash-1788661917824886720.dmp
  the trawl user reads its own dumps; the denials above are the uid split and the 0700 mode, not missing files


## phase result

  crash cases executed: 2 of 2 requested (1 2)
  every assertion passed


## phase down: teardown

  restoring host ptrace_scope to 1
  removing the stub SPA this run created: $REPO/crates/trawl-web-ui/dist/index.html

$ docker rm -f 5e113c883661076f925d7cc0b9c10c8ff241357a0be9135bba93a8b3d34e49ca

$ docker rm -f cc7f1984dfe1c446fc25b92e8ebed720161101e4b476d81c48f91850d27021d3

$ docker network rm 1aa61c10daa387c307d536bd0766a3427f8ac702519eb5b8fa6db7515af3cb83

$ cat /proc/sys/kernel/yama/ptrace_scope   # host, after restore
1

exit status: 0
