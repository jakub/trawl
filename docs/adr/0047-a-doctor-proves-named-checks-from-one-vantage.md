# A doctor proves named checks from one vantage and never writes

status: accepted (2026-09-27), prep record for #199; amended (2026-09-28) for the server-host doctors; amended (2026-11-08) for the web-origin probe bodies; amended (2026-11-23): rulings moved from code comments, see the Amendments

`trawld --check-config` proves that a configuration file parses and names every required setting. It never connects to anything. An installation can pass it while a database rejects the service's credentials, a client distrusts the certificate, or the browser address is not in `public_origins`. Today the operator finds these by working through a manual script in the deployment guide: curl the health endpoint, curl `whoami` with a key, run a query, sign in from a browser. This record adds a **doctor**: a command that runs those checks and reports each one.

## Decision

**Each vantage has its own doctor.** A process checks only the configuration it consumes, from the place it runs:

- `trawl doctor` runs where a client runs. It reads the CLI configuration and checks the connection to one API origin and, if one is named, one browser origin.
- `trawld --doctor --config PATH` runs on the server host. It reads `trawld.toml` and the process environment and checks the two databases, the data directory, the certificate, and its own listener. It holds no API key and sends none.
- `trawl-web --doctor --config PATH` runs where the web proxy runs. It checks the proxy's own configuration and proves that the proxy's upstream trust reaches trawld's health endpoint.

No doctor reads another process's environment, sources a systemd `EnvironmentFile`, or searches for secrets. The report names the user it ran as. An operator runs the server-host doctors as the service user and with the service environment. Running as another user is allowed, and the report says what that user could not see.

**Read-only means no intentional change to the installation.** A doctor never migrates, initializes, repairs, takes trawld's sole-writer lock, writes a probe file, runs a query, ingests, or mints a session. Database checks run inside read-only transactions. Three side effects remain, and the docs name them: an authenticated call updates the key's last-used time, a probe spends rate-limit budget, and a 5xx a probe provokes emits its `http_failure` event (ADR-0040). The doctor does not send a test event. Sending and verifying one belongs to the source walkthrough (#198), because ingest is permanent and has no idempotency key (ADR-0045).

**Every check ends in one of four outcomes.** The vocabulary follows ADR-0033:

- `complete`: the doctor observed the named assertion hold.
- `failed`: the doctor observed evidence against it.
- `not_configured`: the user did not select what the check needs, for example no key or no browser origin.
- `not_sampled`: the doctor could not look. The reason is named: permission denied, rate limited, timed out, or `blocked_by` a failed prerequisite.

A check that could not look is never reported as passed or as failed. The run's verdict is `pass` when every selected check is `complete` or `not_configured`. It is `fail` when any check is `failed`. Otherwise it is `incomplete`. The exit status is 0 for `pass`, 1 for `fail`, and 3 for `incomplete`. 2 stays the usage-error status. A value the health endpoint reports that the doctor does not recognize is shown as reported and cannot pass.

**The output names sources, never values.** Text and a versioned JSON form carry the same facts: each check's stable id, outcome, reason, source, and a next action. A source is a name, such as `FLEET_DATABASE_URL from the environment` or `[storage] database_url in /etc/trawl/trawld.toml`. No output contains a database URL, a key, a key prefix, a certificate body, a response body, or driver error text.

**The client doctor sends a key only where the user bound it.** `trawl doctor` takes its target from `--url` or `--profile`, never from `TRAWL_URL`, `TRAWL_PROFILE`, `TRAWL_TOKEN`, or `TRAWL_INSECURE`. A CLI profile binds its URL, key, and trust together and refuses flags that would change any of them. A bare `--url` takes a key only from a source the user names on the command line, `--token-env NAME` or `--token-file PATH`. It never falls back to a key saved for another server. With no key named, the identity check is `not_configured` and the run can still pass. The key goes only to the API origin's `/api/v1/whoami`, only after TLS verification succeeds. When the connection is configured `insecure`, the TLS check cannot pass and no key is sent. Redirects are refused, and a TLS failure is never retried without verification.

**The browser path is proven in two halves, and neither half sends a key to the browser origin.** From the client, `trawl doctor --web-url ORIGIN` proves that the origin answers and that `trawl-web` accepts it as a public origin. It sends a login request with that `Origin` and an empty key, and it recognizes `trawl-web`'s own responses by status and body, not by status alone. On the web proxy's host, `trawl-web --doctor` proves that the proxy's upstream trust reaches trawld. A client alone cannot prove the proxy-to-trawld hop. That limit is accepted, because a doctor exists to find a wrong address, and a wrong address must never receive a key.

## Considered options

**One doctor that reads everything**, rejected. The client, trawld, and trawl-web read different files and different environments. On a Helm pod the web sidecar's settings are visible only to the sidecar. A single command would check a configuration that no running process uses.

**Sending the key through the web proxy to `whoami`**, rejected. It proves the whole browser path from a laptop, and a browser sends the key to the same place at sign-in. But a doctor runs precisely when an address may be wrong, and it would hand the key to that address.

**A new anonymous diagnostic endpoint on `trawl-web`**, rejected. It would let any caller make the public proxy probe trawld and would need its own rate limiting and caching. The existing login route already answers the origin question without a key, and the proxy's host can prove the upstream hop itself.

**Honoring `TRAWL_URL` and `TRAWL_TOKEN` as other commands do**, rejected. The environment can silently point a command at another server or give it another server's key. The doctor makes the target and key visible on the command line. `-p trial` keeps its own rules (ADR-0045).

**A synthetic ingest check**, rejected. See the source walkthrough (#198).

**Exit 1 for every outcome that is not a pass**, rejected. A monitor needs to tell "broken" from "could not look" without parsing JSON.

**Calling the commands `check-connection` and `--check-installation`**, rejected by the operator in favor of `doctor`, the name people search for. `fleet-dev doctor` checks a developer's machine and keeps its meaning. The glossary separates the two.

## Delivery

Two slices, each end to end. The first, #199, ships `trawl doctor` with the shared report and exit contract, and it fixes the client so a 503 health response keeps its per-check body. The second ships `trawld --doctor` and `trawl-web --doctor`, including a read-only schema check for the app-state database beside the Fleet keystore's existing one. It gets its own prep, with this record as its settled design.

## Amendment: the server-host doctors, 2026-09-28

The prep of the second slice settled how the server-host doctors read an installation. The rules above still hold. These add to them:

- **State that trawld's boot creates is checked for admission.** trawld creates or brings current some state on its own start: an empty or behind app-state schema, an absent or empty data root on an ingest node, and an absent generated certificate. A check on that state asserts that trawld's boot will accept it. The outcome is `complete` with the reason `will_initialize`. Every state the boot refuses stays `failed`: an empty or behind Fleet schema, which needs `fleet-admin migrate`; an unsupported epoch; an owned root without an epoch; a data root that belongs to another catalog; and a dirty, ahead, or foreign migration ledger. Trust checks keep their own assertions. A fresh installation before its first start therefore exits 3, because its listener cannot be sampled. It exits 1 only when something must be fixed before starting. Without this rule, every fresh installation and every upgrade before its restart would fail on state the operator can only fix by starting trawld. Correction: on an ingest node the boot does not refuse a data root that belongs to another catalog. It adopts the archive through the lossy conformance pass. The doctor still reports `failed` on every node type, a human decision made during the #269 run on 2026-09-28.
- **Root sees content, not access.** A doctor run as root reports what it read: a certificate parses, an epoch is current. A check that asserts the running user can read or write something is `not_sampled` with the reason `ran_as_root`. A root run cannot exit 0.
- **Migration locks are observed, never taken.** The schema is read in one read-only snapshot. A current schema is `complete` even while a migrator holds its lock. A fresh or behind schema while the migrator's lock is held is `not_sampled` with the reason `migration_in_progress`. A dirty ledger is always `failed`. trawld's writer lock is reported as held or not observed. It does not prove that trawld is running on this host.
- **The listener proves the certificate on disk.** trawld's doctor connects to its own listener and accepts only the exact certificate that its configuration names or generated. It makes no claim about host names; the client doctor proves those. A served certificate that differs from the file is `failed`. A file that changes while the doctor reads it gives `not_sampled`.
- **Fewer values reach the output.** A report may name the configuration and credential files the user selected, and the running user and uid. It never shows certificate names, listener addresses, or catalog identifiers.
- **trawl-web has no unverified mode.** ADR-0048 removes the loopback switch. A web doctor with no persistent session key reports `not_configured` and says that sessions end on every restart. An upstream URL that carries credentials is refused before any request.

The shared report and exit contract lives in a crate that the CLI, trawld, and trawl-web all use. The second slice ships as three pull requests after #199 and #265: `trawld --doctor`; the trust change in ADR-0048; then `trawl-web --doctor`.

## Amendment (2026-11-08): the web-origin probe recognizes trawl-web's envelope

The untangle prep of 2026-11-08 (defect D14) makes trawl-web answer every refusal with the `ErrorResponse` envelope ([ADR-0066](0066-the-spa-reads-every-failure-through-one-envelope-decoder.md)). The Decision says the browser half "recognizes `trawl-web`'s own responses by status and body, not by status alone". That rule stands. The bodies it recognizes change in the same change that changes trawl-web:

- **Accepted origin:** HTTP 400 with code `bad_request` and message `bad request`.
- **Rejected origin:** HTTP 403 with code `forbidden` and message `cross-origin request rejected`.

The probe no longer accepts the legacy `{"error":"bad request"}` and `{"error":"cross-origin request rejected"}` bodies (`crates/trawl-cli/src/doctor/web.rs:38-40`, `:116-151`). Any other body, the legacy one included, still means the origin is not a trawl-web login endpoint. `reference/cli.md` documents the new bodies.

Considered and not adopted:

- **Accepting both forms.** No trawl-web of this release emits the legacy body, and the doctor ships with trawl-web. A second accepted shape only widens what the doctor mistakes for trawl-web.
- **Matching the status and code without the message.** The message is a fixed string trawl-web writes, and checking it keeps the probe as specific as it is today.

## Amendment (2026-11-23): rulings moved from code comments

A comment audit on 2026-11-23 removed these rulings from the code, where they were the only record. They were already in force. None is a new decision.

**The client doctor**

- **`trawl doctor` sends the key only after its unkeyed `GET /api/v1/health` returned trawld's health body under verified `https` trust.** That body is a parsed `checks` map and a `version`, under the HTTP status that trawld pairs with the body's `status`: 200 with `ok` or `degraded`, 503 with `unavailable`. Any other answer blocks `api.identity`. A redirect, a 404, or another answer that is not this body fails `api.health`. A 429, a 503 `request_limit_reached` refusal, or a timeout leaves `api.health` `not_sampled`. A verified certificate proves the host, not the service. Only a body that trawld always sends shows that the address is trawld before the key leaves the client.
- **A bare `--url` reads no `config.toml`.** No saved key and no saved `insecure` setting applies, and trust is the system roots unless `--insecure` is on the command line. `trawl doctor` is dispatched before the ordinary configuration load, so nothing saved reaches the run. A saved setting belongs to a profile's server. Applied to a bare URL, it could weaken trust for a different server or hand that server a key, the same hazard that the refusal of `TRAWL_URL` and `TRAWL_TOKEN` prevents.
- **`trawl doctor --web-url` verifies the web origin's TLS against the system roots only.** A private CA on the web origin is not supported, and a plain `http` `--web-url` is accepted only for a loopback host. The web origin is what a browser opens, so a certificate that a browser would not trust is a finding, not a configuration to honor.
- **The `web.origin` probe depends on the order of checks in trawl-web's login route.** `login` in `crates/trawl-web/src/routes/auth.rs` checks the `Origin` header before the key, and it answers an empty key without calling trawld. That order lets an empty-key login answer the `public_origins` question with no key and no trawld. It is also why the probe cannot show that trawl-web reaches trawld. A change to that order must change the probe.

**The server-host doctors**

- **`trawld --doctor` and `trawl-web --doctor` check only a configuration file that `--config` names on the command line.** A path that comes only from `TRAWL_CONFIG`, or trawl-web's default path when a file is there, is a usage error with exit status 2. The error message quotes neither path, and the doctor writes nothing. The client doctor refuses `TRAWL_URL` for the same reason: the environment can point a command at a file that no running process uses.
- **`trawld --doctor` judges the configuration and the data root through the validators, classifiers, and decoders that boot and recovery call.** Configuration goes through `check_loaded`, which `--check-config` also uses. The epoch, catalog identity, conformance, repin, publication, and rollup markers go through boot's and recovery's own code, such as `epoch::classify_data_root`, `conform::judge_archive_identity`, `marker::parse_marker`, and `rollup_marker::classify`. A second implementation could drift, and the doctor would then admit state that boot refuses, or refuse state that boot admits. Only the reads are the doctor's own, in `crates/trawl-server/src/doctor/fsread.rs`. It reads `EPOCH`, `CATALOG`, and `REPIN` through `read_bounded`. It opens the publication and rollup markers for boot's decoders through `open_for_decoder`, which opens only a regular file for reading. A FIFO, a symlink, or an oversized file under the data root then costs one row, and the run continues.
- **The doctor trusts the database server that trawld's configuration names, and guards only against a URL that names another kind of server by accident.** Under `sslmode` `disable` or `allow`, it first sends SQLx's startup message itself, with no password. A first answer with a type other than `R`, `E`, or `v`, or with a declared length over 8 KiB (`FIRST_ANSWER_CAP`), is `failed` with the reason `not_postgres`, and SQLx never connects. SQLx buffers as many bytes as a message declares, so an HTTP or SSH banner read as a frame reserves about 1.4 GB. A hostile server already owns trawld's state, so the doctor does not guard against one. The accepted cost is one extra login attempt per probe on a real server. A PAM stack with a lockout module such as `pam_faillock` may count it as one failure.
- **The doctor trusts the database credential files that the operator selects.** Those are `PGPASSFILE`, `~/.pgpass`, and the TLS files. Before SQLx reads a password file, the doctor checks only that it is a regular file of at most 1 MiB (`PASSFILE_CAP`). The doctor reads each TLS file once, bounded, and passes the bytes to SQLx, so SQLx never opens a TLS path. The checks catch accidents, such as a FIFO or a multi-gigabyte file at the path. A person who can replace a password file between the doctor's check and SQLx's read is already inside trawld's trust boundary. The owner made this ruling on 2026-09-29.
- **`server.data.identity` accepts a data root whose `CATALOG` marker names the app-state database's catalog, or a root that holds no parquet.** The doctor runs before an ingest boot would adopt a foreign archive through the lossy conformance pass, so it still catches a wrong database URL or a mismatched restore while either can be fixed. This is the reason for the 2026-09-28 amendment's `failed` on every node type.
- **A report may name two paths besides the selected configuration and credential files.** `trawl-web --doctor` may show the upstream CA file that the operator selected. `trawld --doctor` may name an unresolved rollup marker by the env, day, and service that its path spells. Each is shown only after it validates. A report shows no other path, no marker content, no data root, and no WAL directory. The CA file is an operator selection like the credential files. The operator must know which rollup marker is unresolved to act on it, and the validation keeps arbitrary path text out of the report.
- **An access check is one whose `complete` says the running user may read or write something.** Only trawld's data root and trawl-web's `cookie_secret_path` file are access checks (`ServerCheck::is_access`, `WebCheck::is_access`). Database authentication, certificate reads, and a cookie key from the environment are content. A root run of `trawl-web --doctor` always reports `proxy.identity` as `not_sampled` with the reason `ran_as_root`, and still shows uid 0. With a cookie key from the environment, trawl-web has no access check, so without that row a root run could exit 0.

**Corrections**

- **Correction.** `trawld --doctor` never shows a health check name or value as the listener sent it. It shows a name only as its own copy from `HEALTH_CHECK_NAMES`, and a known value only as its own literal. All unknown names collapse into one `failed` `server.listener.health._invalid` row that gives only their count. An unknown value is `failed` and is not shown. `trawl doctor` quotes an unknown name or value only when it is identifier-shaped (`is_health_key`, `is_quotable_value`) and does not contain the client's key, and it counts other names in `api.health._invalid`. Even an identifier-shaped value may be a secret, and the server-host doctor holds no key to screen it against. This corrects the Decision's sentence "A value the health endpoint reports that the doctor does not recognize is shown as reported and cannot pass." An unrecognized value still cannot pass.
