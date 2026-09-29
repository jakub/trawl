# Sender proof in a disposable Debian 13 VM (issue #198)

`transcript.txt` is the standard output of [`run.py`](run.py), run from a
clean checkout at the tested commit. It runs the proof blocks of two pages
byte-for-byte:

- `docs/src/content/docs/getting-started/vector-integration.md`
- `docs/src/content/docs/operate/ingestion.md`

Each executable block carries a `<!-- proof:NAME -->` marker on the line
before its fence. `run.py` reads the markers from the checkout and stops
before any build if one is missing, duplicated, or not in its inventory.
Only the `*-vars` blocks take site values. The transcript shows each
substitution.

## Tested commit

- Commit C: `TODO: full SHA`
- Guard: `git diff --quiet C HEAD -- . ':!docs/evidence'` exits 0 at the
  PR head. Any later change outside `docs/evidence/` needs a new run.

## Environment

| Item | Value |
| --- | --- |
| Image | `debian-13-genericcloud-amd64-20260914-2601.tar.xz`, sha512 `ba03aae0…f26a0` (full value in `run.py` and the transcript) |
| Guest kernel | TODO, from the transcript |
| trawld, trawl | TODO: `trawld --version`, which names C's short SHA with no `*` |
| Vector | 0.57.0, `vector_0.57.0-1_amd64.deb` from the GitHub release, sha256 `ee24ecf7…1e88e` |
| nginx, Docker, PostgreSQL, UFW | TODO, from the versions phase |
| Host | TODO: `uname -sr`, qemu and Docker versions from the header phase |

Vector comes from the release artifact, pinned by hash, rather than from
the guide's apt repository, which serves the newest version. This is
evidence setup, not a documented install path.

## Reproduce

```bash
python3 docs/evidence/2026-09-28-issue-198-sender-proof/run.py --build > transcript.txt
```

`--build` builds the three distribution `.deb` files from HEAD in the pinned
container that `crates/trawl-server/debian/tests/crashdump-harness.sh` uses.
The build writes to `target/issue-198-deb`. `--packages DIR` reuses built
packages instead. `--image-cache DIR` holds the cloud image and the Vector
package, and defaults to `~/.cache/trawl-evidence`. `--keep` leaves the VM
running. `run.py` refuses a dirty working tree unless `--allow-dirty` is set.
A run with `--allow-dirty` is not evidence.

Standard error carries progress. The exit status is 0 when every assertion
passed, 1 when any failed, and 2 when the run stopped early.

## What the run does

1. Checks the working tree and the proof-block inventory. Builds or reads
   the packages.
2. Verifies the image against the pinned sha512. Makes a sparse copy under
   `target/`, grows it to 20 GB, and boots it under qemu/KVM. cloud-init
   reads its seed over SMBIOS from a loopback HTTP server, which stops after
   boot. SSH uses per-run keys and a pinned host key.
3. Installs PostgreSQL, the trawl packages, nginx, Docker, UFW, and Vector
   in the guest. Mints an ingest key and a reader key into root-only files.
4. Writes one journald, nginx, and Docker line before Vector starts, then
   runs `proof:vector-start`.
5. History: runs `proof:history-finder`, then checks that the journald line
   arrived, and that the nginx and Docker lines from before the start are
   absent while their post-start controls are present.
6. Recipes: journald, nginx, Docker, UFW, and the syslog appliance. The UFW
   packet comes from network namespace `peer-ufw` (10.198.1.2). The
   appliance is SIMULATED: network namespace `peer-dev` at 192.0.2.1, the
   address `proof:syslog-config` names, sends one RFC 5424 message.
7. Negatives: a wrong key (401), `TRAWL_ENV=Prod` (`invalid_env`), and
   `TRAWL_ENV=staging` (`env_not_allowed`). Each is followed by a restore
   and a positive control.
8. Records versions and hashes. Checks that neither key appears in the
   transcript or in this directory. Each key goes from the guest to
   `grep -f -` on standard input, with a positive control first.

## Host boundary

The host's Docker daemon ran only the build container. qemu ran as an
unprivileged user with user-mode networking and one loopback port forward.
No host service was reconfigured, and no saved trawl CLI profile on the
host was read. The VM, its disk, and the build output lived under
`target/` and were removed after the run.
