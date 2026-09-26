# First run is a trial the CLI owns; an installation starts fresh

status: accepted (2026-09-25), prep record for #203

Before a first query, an evaluator needs five executables, PostgreSQL with two databases, two roles, two keys, a certificate, and a hand-written config. No install channel creates a key. A fresh `apt install` starts `trawld` against placeholder database URLs, and systemd restarts it every five seconds. This record adds a **trial**: a disposable installation that the `trawl` CLI creates, runs, and deletes on one Linux machine. It also makes the Debian package install without starting anything.

## Decision

**The CLI owns the trial.** `trawl trial` has five verbs: `up`, `status`, `key`, `stop`, and `down`. `up` renders a Compose project into the trial directory and runs it with the published image `ghcr.io/jakub/trawl:<CLI version>` and `postgres:18`. It creates the databases, applies the Fleet schema, mints the keys, starts `trawld` and `trawl-web`, loads sample data, and prints the browser and API addresses. A Compose bundle alone cannot mint a key or hand it to the user. fleet-dev (ADR-0010) is a development controller that needs a source checkout. The CLI already ships everywhere a client runs.

**One trial per Docker engine, and the trial acts only on what it labels.** The Compose project is `trawl-trial`. Every container, network, and volume carries the trial's random id. The trial refuses to act on a resource with a different id, refuses a remote Docker context, and never adopts a listener that already holds its port. `up` resumes an existing trial without new keys or new samples, and refuses to resume on an image digest other than the one it recorded. `down` lists what it will delete and asks for confirmation, or takes `--yes`. It then deletes the containers, volumes, network, and trial directory. Nothing restarts at boot.

**Loopback only.** The API listens on `127.0.0.1:15514` and the browser UI on `127.0.0.1:18090`. PostgreSQL has no published port, and syslog is off. The browser origins are `http://localhost:18090` and `http://127.0.0.1:18090`. Insecure cookies are allowed because the browser leg is plain HTTP on loopback. Other local processes can reach both ports, and the host's root and Docker administrators can read every secret. The trial does not protect against them.

**Clients verify the trial's certificate.** The trial generates its own certificate with a SAN for the in-network service name. The CLI and `trawl-web` pin it through a new CA setting: `ca_cert` on a CLI profile, and an upstream CA path on `trawl-web`. No insecure flag is part of the trial. With the CA setting in place, the CLI warns when `insecure` is on. `trawl-web` accepts `TRAWL_WEB_INSECURE_UPSTREAM` only for a loopback upstream, which is what the Debian package and the Helm chart use.

**Secrets travel as files, never as environment.** The CLI pipes each secret on stdin into a mode-0400 file in the volume of the container that reads it. The PostgreSQL superuser password never leaves the PostgreSQL volume. Separate `fleet` and `trawl` owner roles own the two databases, as in a durable installation. The host keeps only the two tokens, at mode 0600, and the public certificate.

**Two keys.** The operator key is a human key with every permission except `ingest`: `query`, `schema_read`, `validate`, `saved_query`, `export`, `stream`, `query_cancel`, `server_manage`, and `schema_write`. The list is fixed, so a permission added later never widens a trial silently. The evaluator owns this installation. Under ADR-0025 a control the key cannot use is absent, so without `server_manage` the Health page, stats, and repin never appear. The ingest key is a service key with `ingest` only. Neither key expires. `up` prints the token file path, never the token. `trawl trial key` prints the operator token and nothing else.

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
