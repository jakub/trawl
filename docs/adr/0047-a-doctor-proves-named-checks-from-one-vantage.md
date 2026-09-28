# A doctor proves named checks from one vantage and never writes

status: accepted (2026-09-27), prep record for #199

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
