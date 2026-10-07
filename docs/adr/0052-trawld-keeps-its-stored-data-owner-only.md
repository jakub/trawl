# trawld keeps its stored data owner-only

status: accepted (2026-10-06), prep record for #282

A walk of the published v0.9.1 manual on a fresh Debian 13 host found every stored log readable by every local user. The package creates `/var/lib/trawl` at 0750, but `trawld.service` declares `StateDirectory=trawl` and `LogsDirectory=trawl` with no mode, so systemd resets both directories to 0755 at every start. trawld then creates its data root, WAL, Parquet, markers and scheduled results under the inherited umask 0022: directories 0755, files 0644. User `nobody` could list partitions and read WAL lines, including journal entries the host limits to the `adm` and `systemd-journal` groups. Even with the documented 0750 state directory, `trawl-web` would read the corpus through group `trawl`. No test ever started trawld and then looked at the modes. The tarball path and the container never read the unit file, so a unit-only fix would cover one install channel of three.

## Decision

**Only trawld reads its stored data.** The data root, the WAL directory, and everything trawld creates beneath them are owner-only. Group `trawl` exists so `trawl-web` can read two things, the cookie key and the generated certificate, and both already carry explicit modes (0640 and 0644 in a 0755 `tls/`, ADR-0048). The proxy reaches stored events only through trawld's authenticated API. A compromised proxy, a backup agent or another local user gains nothing from the disk. This is the posture PostgreSQL takes with its data directory.

**trawld sets its own umask.** At startup, before it spawns a thread or creates a file, trawld sets the process umask to 077. Every install channel gets the same modes: the Debian unit, a tarball supervisor, and the container. Files whose modes the design requires to differ are already moded explicitly and keep their modes: `tls/` 0755, `cert.pem` 0644, `tls-key/` 0700, `key.pem` 0600, the query debug log 0600, crash dumps 0600 in a 0700 `cores/`. DuckDB `COPY ... TO` writes Parquet with `0666 & ~umask`, so the umask is the only creation-time control for Parquet, and it now yields 0600.

**trawld closes its roots at every start, and refuses to start if it cannot.** Before it opens storage, trawld checks the data root and, when it lies outside the data root, the WAL directory. A root with any group or other permission bit is tightened to owner-only through a no-follow directory handle. Closing the root makes every older 0644 file beneath it unreachable for anyone but the owner, so no recursive walk runs. A recursive chmod would race a running writer and follow planted links. If a root cannot be tightened, because another user owns it or the filesystem refuses, trawld exits with a named error that gives the path, its owner, its mode, and the fix. A server that would expose stored logs never serves. `trawld --doctor` runs the same check without changing anything. A root it would tighten passes with the reason `will_tighten`, and a root it could not tighten fails.

**The Debian unit owns the two systemd-managed directories.** `trawld.service` sets `StateDirectoryMode=0750`, because `trawl-web` must still pass through `/var/lib/trawl` to the cookie and `tls/`, and `LogsDirectoryMode=0700`, because only trawld writes `[server] log_file`. systemd applies both at every start, so an upgrade fixes an existing installation the first time it restarts. No maintainer script changes modes.

**A standing check proves the modes after a real start.** The release `verify` job, which already installs the package on a systemd host, starts trawld against a local PostgreSQL and asserts the modes of the state directory, the data root, the WAL, a stored Parquet file, a WAL stage file and the log directory, and that user `nobody` and user `trawl-web` cannot read a stored event. Static checks in `packaging.sh` pin the unit directives. The umask and the root check get native tests in the trawld crate.

## Considered options

**Unit file only (`UMask=` plus the directory modes)**, rejected. It covers the Debian package and leaves the tarball and container channels on umask 0022. It also leaves a data root moved outside `/var/lib/trawl` open.

**Group-readable data (umask 027, roots 0750)**, rejected by the operator. It keeps other users out, but `trawl-web` and any future member of group `trawl` could read the corpus straight from disk. Nothing needs that access.

**Warn and keep serving when a root cannot be tightened**, rejected by the operator. The exposure would stay until someone read the warning. Refusing to start follows the fail-closed rulings of ADR-0041 and ADR-0048.

**Repair existing files recursively at upgrade**, rejected. A privileged walk under a directory trawld can write is a link-planting and race hazard, and it would flatten the explicit modes of `tls/`, the cookie and `cores/`. Closing the root removes the exposure without touching the files beneath it.

## Consequences

A backup agent must run as the trawl user or as root. Homelab volsync already runs its mover as uid 1000, the chart's trawld uid. The deployment guide's file table gives every row a mode and says which entries trawld creates at first start. The tarball section no longer asks the supervisor for a umask.

## Amendment (2026-10-06)

The implementation of #282 settled the rules the body left open.

**Which roots trawld closes.** The data root on every node, ingest or query-only. On an ingest node, the WAL directory unless the data root provably holds it. A name under the data root is not proof: `data/wal` can be a symlink out of the root, or a mount that other users reach by another path. The WAL directory is held by the root only when trawld reaches it from the open data root by descriptor, one `openat(O_DIRECTORY | O_NOFOLLOW)` per component, and every directory on the way is on the data root's filesystem and, on Linux, in its mount. A path with a `..` component is never held. A WAL directory the root holds is never chmodded. Any other WAL directory is a root of its own. The repin siblings, `data.repin-next` and `data.repin-aside`, are closed when they exist. A query-only node whose root does not exist leaves it absent.

**A root trawld creates is checked once it exists.** The umask makes a new root owner-only only where the filesystem lets it. A fixed-mode mount or a default ACL on the parent decides the mode instead. So boot closes the roots again after the epoch gate creates the data root and after it creates the WAL directory, before any producer or listener starts.

**Only the final path component is held no-follow.** trawld opens the last component of each root with `O_NOFOLLOW` and judges and tightens through that handle. The directories above it are operator-trusted storage, as ADR-0041 rules, so a symlink among them is followed. The components between the data root and a WAL directory under it are not trusted this way: they decide whether the root holds the WAL directory, as above. A root whose final component is a symlink refuses, at boot and in the doctor. Boot followed it before. This is a deliberate change.

**How trawld tightens.** It clears the group and other bits, `mode & !0o077`, through the handle, and keeps the owner bits and the setgid and sticky bits, which Kubernetes `fsGroup` sets. After the change it reads the mode again and refuses if any group or other bit survives, as on a filesystem that ignores `fchmod`. A root owned by another user refuses before any change, even when trawld runs as root.

**Files outside the roots.** The query engine spills into a directory it makes for each database in the system temp directory, with a random name and mode 0700, and removes it when the database closes. `DuckDB` opens its spill files by predictable names without `O_EXCL`, so the shared temp directory would reuse a file another user planted or an older process left readable. Compaction and the conform pass already spill under the data root. `[server] log_file` opens like the query debug log: 0600 at creation, and an existing looser file is tightened, since it can sit outside every directory trawld or systemd closes.

**The doctor predicts, boot decides.** `will_tighten` is a read-only prediction from the root's owner and mode, plus a read-only-filesystem check. Boot does not pre-judge the filesystem. Its `fchmod` is the authority, and its result is what refuses or serves.

## Amendment (2026-10-07)

Review of #282 moved the start-time close and widened what it covers.

**trawld closes the roots before it waits on PostgreSQL.** The first close runs once the configuration is loaded, before the database admission that takes the sole-writer lock. The earlier placement, after the lock, kept a corpus open for as long as a database was down, across every restart. The lock is not needed: the close only removes group and other bits, so a second trawld that runs it and then loses the lock has changed nothing the winner would not. The checks after the epoch gate creates the data root and after boot creates the WAL directory stay where they are.

**A repin closes its staging roots when it creates them.** A repin makes `data.repin-next` and `data.repin-aside` while trawld serves, after the start-time close. Each is closed through the same no-follow handle as soon as it exists, before anything is linked, written or renamed into it. A refused shadow root fails the job with the corpus untouched. A refused aside root comes after the cutover marker, so it ends the process like any other swap failure, and the boot replay refuses the start until the operator fixes the root.

**An unknown mount does not prove containment.** On Linux a WAL directory is held by the data root only when `statx` names the mount of every directory on the way. A kernel before 5.8 does not, and the device alone cannot tell a bind mount from a plain directory, so the WAL directory is then a root of its own.

**A WAL directory or repin sibling that is not a directory refuses the start**, at boot and in the doctor. A data root that is not a directory stays the epoch gate's to refuse.

**trawld checks the spill directory and the diagnostic logs on their descriptors.** The spill directory is opened no-follow after it is made, and refused unless trawld's euid owns it and it has no group or other bit. `[server] log_file` and the query debug log are judged on the opened descriptor after the chmod, whatever the chmod returned: a file another user owns, or one with a group or other bit left, refuses the open. A file another user owns is refused even when it is already tight, since its owner can loosen it at any time. The deployment guide asks for a `TMPDIR` in which other users cannot rename entries, such as the sticky `/tmp`.
