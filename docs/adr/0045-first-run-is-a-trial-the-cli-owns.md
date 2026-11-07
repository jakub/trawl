# First run is a trial the CLI owns; an installation starts fresh

status: accepted (2026-09-25), prep record for #203; amended (2026-09-25, 2026-09-27) during the #203 run, by ADR-0048 (2026-09-28), and (2026-11-06) by the prep record for #354, see the Amendments

Before a first query, an evaluator needs five executables, PostgreSQL with two databases, two roles, two keys, a certificate, and a hand-written config. No install channel creates a key. A fresh `apt install` starts `trawld` against placeholder database URLs, and systemd restarts it every five seconds. This record adds a **trial**: a disposable installation that the `trawl` CLI creates, runs, and deletes on one Linux machine. It also makes the Debian package install without starting anything.

## Decision

**The CLI owns the trial.** `trawl trial` has five verbs: `up`, `status`, `key`, `stop`, and `down`. `up` renders a Compose project into the trial directory and runs it with the published image `ghcr.io/jakub/trawl:<CLI version>` and `postgres:18`. It creates the databases, applies the Fleet schema, mints the keys, starts `trawld` and `trawl-web`, loads sample data, and prints the browser and API addresses. A Compose bundle alone cannot mint a key or hand it to the user. fleet-dev (ADR-0010) is a development controller that needs a source checkout. The CLI already ships everywhere a client runs.

**One trial per Docker engine, and the trial acts only on what it labels.** The Compose project is `trawl-trial`. Every container, network, and volume carries the trial's random id. The trial refuses to act on a resource with a different id, refuses a remote Docker context, and never adopts a listener that already holds its port. `up` resumes an existing trial without new keys or new samples, and refuses to resume on an image digest other than the one it recorded. `down` lists what it will delete and asks for confirmation, or takes `--yes`. It then deletes the containers, volumes, network, and trial directory. Nothing restarts at boot.
*Amended 2026-11-06 ([an unfinished teardown blocks `up`](#amendment-an-unfinished-teardown-blocks-up-2026-11-06)): `down` writes a teardown marker before its first deletion and removes it last. `up` refuses while the marker exists, and refuses to resume a trial whose volumes are gone.*

**Loopback only.** The API listens on `127.0.0.1:15514` and the browser UI on `127.0.0.1:18090`. PostgreSQL has no published port, and syslog is off. The browser origins are `http://localhost:18090` and `http://127.0.0.1:18090`. Insecure cookies are allowed because the browser leg is plain HTTP on loopback. Other local processes can reach both ports, and the host's root and Docker administrators can read every secret. The trial does not protect against them.

**Clients verify the trial's certificate.** The trial generates its own certificate with a SAN for the in-network service name. The CLI and `trawl-web` pin it through a new CA setting: `ca_cert` on a CLI profile, and an upstream CA path on `trawl-web`. No insecure flag is part of the trial. With the CA setting in place, the CLI warns when `insecure` is on. `trawl-web` accepts `TRAWL_WEB_INSECURE_UPSTREAM` only for a loopback upstream, which is what the Debian package and the Helm chart use.
*Amended 2026-09-25 ([the Debian package pins trawld's certificate](#amendment-the-debian-package-pins-trawlds-certificate-2026-09-25)): only the Helm chart uses the loopback switch. The Debian package pins trawld's generated certificate through `[web] upstream_ca_path`.*
*Amended 2026-09-28 by [ADR-0048](0048-trawl-web-always-verifies-trawld.md): the loopback switch is removed. The Helm chart pins trawld's certificate too.*

**Secrets travel as files, never as environment.** The CLI pipes each secret on stdin into a mode-0400 file in the volume of the container that reads it. The PostgreSQL superuser password never leaves the PostgreSQL volume. Separate `fleet` and `trawl` owner roles own the two databases, as in a durable installation. The host keeps only the two tokens, at mode 0600, and the public certificate.

**Two keys.** The operator key is a human key with every permission except `ingest`: `query`, `schema_read`, `validate`, `saved_query`, `export`, `stream`, `query_cancel`, `server_manage`, and `schema_write`. The list is fixed, so a permission added later never widens a trial silently. The evaluator owns this installation. Under ADR-0025 a control the key cannot use is absent, so without `server_manage` the Health page, stats, and repin never appear. The ingest key is a service key with `ingest` only. Neither key expires. `up` prints the token file path, never the token. `trawl trial key` prints the operator token and nothing else.
*Amended 2026-10-24 by [ADR-0059](0059-an-investigation-is-grant-visible-and-its-evidence-outlives-the-corpus.md): the operator key also holds `investigation_read` and `investigation_write`, so it keeps every permission except `ingest`.*

**`-p trial` is reserved.** The profile name `trial` reads the URL, the token, and the CA from the trial directory. The CLI never edits `~/.config/trawl/config.toml`. `-p trial` fails if `TRAWL_URL` or `TRAWL_TOKEN` is set, or if the config file defines its own `trial` profile. Either one would point the command somewhere else without the user seeing it.

**Sample data is on by default.** A seeded generator posts about 2000 events across a fixed set of services. The set includes `web`, so the web UI's quick-start examples return rows. Per-service counts are fixed, so a documented query has one exact answer. The trial records its intent before it posts, and marks the samples complete only after it verifies the counts. Ingest has no idempotency key, so the trial refuses to post again over a partial or unknown result. `--no-sample-data` skips the step.

**No promotion.** A trial never becomes a durable installation. To move on, the user installs fresh from a package, the tarball, or the Helm chart, and follows the deployment guide. The trial's keys and sample data do not carry over (ADR-0036).

**The Debian package installs inert.** `trawl-server` installs `trawld` and `trawl-web` disabled and stopped. The operator sets the database URLs, then runs `systemctl enable --now trawld trawl-web`.

**Linux is the supported trial platform.** CI proves the trial on Linux amd64 and arm64 from the release-built artifacts. GitHub's macOS runners cannot run Docker. The binary has no OS check, but the docs claim only Linux until a macOS run is evidenced.

**The image and chart are anonymously pullable.** Both are public packages on ghcr.io, and CI checks an anonymous pull.

## Considered options

**A shipped Compose bundle**, rejected: Compose cannot mint a key, deliver it, or print addresses, so the user still runs `fleet-admin` by hand. A `.env.example` with placeholders repeats the Debian `CHANGE_ME` problem.

**Direct Docker calls instead of Compose**, rejected: the CLI would rebuild the start order, readiness waits, and cleanup that Compose provides. The ownership rules apply either way.

**Disabled certificate checks in the trial**, rejected: if `trawld` stops and another process takes port 15514, a client that does not check sends that process the key.

**Secrets in container environment**, rejected: they appear in `docker inspect` and `docker compose config` output, which users paste into bug reports.

**A marked block in the user's config file**, rejected: it edits a file that dotfile managers often own, and `TRAWL_TOKEN` still overrides it.

**A seven-permission reader key**, rejected: it hides the operations half of the UI on a server the evaluator owns.

**Three opt-in sample events**, rejected: the histogram, facets, and quick-start examples show nothing.

**A promotion command**, rejected: it turns a loopback, self-signed evaluation into an exposed production server.

## Amendment: the Debian package pins trawld's certificate, 2026-09-25

The Decision said the Debian package uses `TRAWL_WEB_INSECURE_UPSTREAM`. It did not. The package shipped the variable commented out in `/etc/default/trawl-web` and set no CA, so `trawl-web` checked trawld's self-signed certificate against the platform roots. A stock install could not sign in. The human decided during the #203 run to pin the certificate and keep the switch off.

The packaged `trawld.toml` sets `[web] upstream_ca_path` to `/var/lib/trawl/tls/cert.pem`. `trawld` writes that certificate on its first start when `tls_cert_path` is not set. Until the file exists, `trawl-web` refuses to start, and systemd restarts it every 5 seconds. An operator who sets `tls_cert_path` points `upstream_ca_path` at the CA that issued that certificate. To use the switch instead, the operator removes `upstream_ca_path`, because `trawl-web` refuses to start with both set.

The Helm chart keeps the loopback switch. There, `trawl-web` runs beside `trawld` in the same pod and reaches it on loopback.

## Amendment: the image tag drops build metadata, 2026-09-27

The Decision names the image `ghcr.io/jakub/trawl:<CLI version>`. The release workflow publishes the version without its semver build metadata, because `docker/metadata-action` renders the tag from the semver version and a Docker tag cannot contain `+`. So `1.1.0-rc.1+build.7` is published, and pulled by the trial, as `1.1.0-rc.1`. The chart keeps the full version.

## Amendment: the loopback switch is removed, 2026-09-28

[ADR-0048](0048-trawl-web-always-verifies-trawld.md) removes `TRAWL_WEB_INSECURE_UPSTREAM`. The Helm chart pins trawld's certificate in every TLS mode, so no channel skips verification. `trawl-web` now starts before the pinned certificate exists and loads it when trawld writes it, so the Debian unit no longer restarts every 5 seconds before trawld's first start.

## Amendment: an unfinished teardown blocks `up`, 2026-11-06

The Decision says `down` deletes the containers, volumes, network, and trial directory. It deletes them one step at a time and records no progress. When `down` is interrupted or fails after `volume rm`, `state.json` still records every phase as done. The next `up` trusts the database phase and starts PostgreSQL over a new, empty volume that holds no superuser password file. PostgreSQL exits, and every rerun of `up` fails the same way with a plain Docker error (#354). The prep for #354 decided the rulings below. No human fork arose.

1. **`down` writes a teardown marker.** The marker is `$XDG_STATE_HOME/trawl/trial.teardown`, beside the trial directory and `trial.lock`, outside the directory that `down` deletes. `down` writes it under the lifecycle lock, after the approval, the engine check, and the ownership scan, and before the first Docker deletion. The write is atomic and durable, at mode `0600`. The marker holds the trial id, and the engine id when the state records one. If the write fails, `down` deletes nothing. A declined approval writes no marker.
2. **`down` removes the marker last.** It removes the marker only after the Docker deletions and the directory removal succeed, and after the directory removal is synced to disk. An interruption at any earlier point leaves the marker.
3. **`up` refuses an unfinished teardown.** `up` checks for the marker before it reads the state, and while the marker exists it refuses and changes nothing. The message tells the user to run `trawl trial down` to finish the deletion, then `trawl trial up` to create a new trial. `status` reports the marker. `stop` ignores it.
4. **`up` refuses to resume over missing volumes.** After it settles one-off containers, `up` checks the labelled inventory. When the state records the database phase or any later step, the three volumes `postgres`, `trawld`, and `web` must each exist with this trial's id. A state that records no step may have missing volumes. If the inventory or a probe fails, `up` reports an error and never reads the failure as a missing volume.
5. **Refusal, not repair.** `up` never resets a phase, mints a key again, or posts the samples again to work around missing storage. The remedy is `down`, then `up`. A trial is disposable, so a fresh one loses nothing that the user needs to keep.
6. **`down` always finishes.** `down` keeps the permissive `DownView` reader and deletes by the trial id that the state records. It uses the marker's trial id only when `state.json` is gone. It never refuses because the marker and the state name different ids: that refusal would block the only command that recovers the trial, and the trial id label already keeps another trial's resources safe. The engine check stays. When the marker exists and the trial directory does not, `down` says that it is finishing an interrupted teardown and removes the marker, where it used to report that there is no trial.
7. **Limits.** Volume presence is the only storage check. A volume that an earlier failing `up` already recreated empty passes it, and the user runs `down` once by hand. A volume replaced under the same name, or a corrupt one, is out of scope. The lifecycle lock serializes `trawl trial` commands only, not other Docker administration on the engine.
8. **An older CLI ignores the marker.** To downgrade the CLI, run `trawl trial down` to completion first. Trial data needs no backup.

Considered and rejected:

- **Reset the phases and repair on `up`.** A repair has to mint the keys again and post the samples again over a recorded intent, and `up` has neither path today. `down`, then `up`, reaches the same fresh trial through code that exists.
- **A teardown phase inside `state.json`.** `TrialState` denies unknown fields, so an older CLI would refuse the whole file. The directory removal also deletes the file before the teardown ends.
- **A marker inside the trial directory.** The directory removal deletes it before the removal finishes, so it cannot be the last thing removed.
- **Refuse `down` when the marker and the state disagree.** It leaves the user with no command that recovers the trial.
