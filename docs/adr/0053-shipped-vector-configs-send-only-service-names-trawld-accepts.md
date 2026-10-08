# Shipped Vector configs send only service names trawld accepts

status: accepted (2026-10-08), prep record for #294

A from-scratch install of v0.9.2 on Debian 13 lost every journal event from a templated systemd unit. `journal_enriched` in `config/vector/debian/base.toml` builds `service` from `_SYSTEMD_UNIT` and strips only the unit-type suffix. `postgresql@17-main`, `serial-getty@ttyS0` and `user@1000` keep their `@`. trawld refuses each event as `invalid_chars` and answers HTTP 200 with the refusals in the body. Vector's `http` sink reads only the status code, so it logs nothing, and `trawl_ingest_events_rejected_total{reason="invalid_chars"}` reached 68 within minutes on a host with six templated services. The same failure reaches the `SYSLOG_IDENTIFIER` and `_COMM` fallbacks (`(sd-pam)`, `kworker/0:1`), systemd-escaped units (`systemd-fsck@dev-disk-by\x2duuid-…`), unit names over 128 bytes, `/var/log` path components, UniFi APP-NAMEs and the Docker image fallback. The collector test in `scripts/test-vector-collector.py` sends its output to a stub that always answers 200, and its fixture asserts `serial-getty@ttyS0`, a value trawld can never accept.

ADR-0013 ruling 4 keeps per-event rejection on HTTP "because an HTTP sender can fix and resend". A forwarding collector that reads only status codes cannot. The configs trawl ships are such a collector.

## Decision

**The shipped configs own the validity of what they send.** Every transform in `config/vector/debian/` that computes `service` from data ends in a gate that is exactly `trawl_config::is_valid_service_name`: the regex `^[A-Za-z0-9_-][A-Za-z0-9._-]{0,127}$`, which is 1 to 128 ASCII bytes from `[A-Za-z0-9._-]`, never starting with a dot. trawld does not change. Its charset, its per-event rejection and its 200 response stay as ADR-0009, ADR-0013 and ADR-0049 decide them. The transforms with a constant `service` (`apache`, `fail2ban`, `mysql`, `nginx`, `postgresql`, `redis`) need no gate.

**The journal service is the first valid candidate.** The transform reads four journal fields: `U = _SYSTEMD_UNIT`, `UU = _SYSTEMD_USER_UNIT`, `I = SYSLOG_IDENTIFIER` and `C = _COMM`. It tries three candidates in order and takes the first that passes the gate:

1. The unit name. Take `UU` when it is non-empty and not `init.scope`, else `U`. Remove one trailing `.service`, `.scope`, `.slice`, `.socket` or `.target`, the list the config strips today. Then remove everything from the first `@`.
2. `I`. If the whole value is one pair of parentheses around non-empty text, use the text inside.
3. `C`, under the same parenthesis rule.

When no candidate passes, `service` is `journal-unidentified`. The UFW override that sets `ufw` still runs afterwards. Some results: `postgresql@17-main.service` gives `postgresql`; `serial-getty@ttyS0.service` gives `serial-getty`; a user unit `pipewire.service` under `user@1000.service` gives `pipewire`; the user manager's own lines (`UU = init.scope`) give `user`; `(sd-pam)` with no unit gives `sd-pam`; an escaped mount unit with `I = mount` gives `mount`; a unit name the guide's `systemd-run --unit` proof creates stays as it is.

Each cut follows a boundary that systemd defines: the unit-type suffix, the template `@`, and the parentheses systemd puts around a helper it has forked but not yet run. Nothing is rewritten, unescaped or truncated, so the config never invents a name. When the unit fails the gate, the next candidate usually names the real program: on a desktop journal, `app-…-gtk\x2dlaunch-….scope` with `I = steam` gives `steam`. Stopping at the first failing candidate would file those lines under the fallback.

**A name nothing can supply goes to a marked fallback.** The fallbacks are `journal-unidentified`, `varlog-unidentified`, `docker-unidentified` and `syslog-unidentified`. They replace `unknown` and the bare `varlog` and `docker` literals. A Vector sender cannot write `_repairs`, so the label itself carries the mark, and it must not be a name a real program uses: `docker` and `syslog` are. ADR-0009 calls `service=unknown` worse than a rejection, because it merges unrelated logs with no trace. For a status-only collector, the rejection is invisible loss, and a marked fallback that keeps the originals on the event loses nothing.

**Every original stays on the event, verbatim.** Journal events carry `systemd_unit`, `systemd_user_unit`, `syslog_identifier` and `comm` whenever the source field is non-empty, and the transform deletes the underscore originals, now including `_SYSTEMD_USER_UNIT`. These are the names trawld's canonicalization would give the journal fields anyway. A field named `unit` would collide with journald's own `UNIT` field, the unit a systemd manager message is about: trawld's case fold keeps the exact-lowercase spelling and drops `UNIT` into `_raw`. There is no separate instance field. `systemd_unit=postgresql@*` and `stats count by systemd_unit` already reach the instance, and a second parse of the unit name could disagree with the first. UniFi events carry `syslog_appname`. Varlog keeps `file`. Docker keeps `image` and `container_name`, and copies the compose and swarm service labels into `docker_compose_service` and `docker_swarm_service` before it deletes `.label`. Two origins may share a `service` only when the shared value is a grammar-delimited part of both originals or a marked fallback, and the originals on each event tell them apart.

**A user unit names its service (operator ruling, 2026-10-08).** `_SYSTEMD_USER_UNIT` is chosen by the user who owns the unit. Any local user can create a user unit named `sshd.service` and file its lines under `service=sshd`. Before this decision a local user's processes landed in `session-N.scope` or `user@N.service`, which only root controls. The operator accepted that change: the trusted alternative files nearly every line on a desktop or user-service host under `user`. `systemd_unit=user@N.service` stays on every such line, so a query can separate a user's `sshd` from the system's.

**The other transforms get the same gate.** `trawl_varlog` keeps its path rule and falls back to `varlog-unidentified`, with `file` kept. `trawl_docker` keeps its priority order: compose label, swarm label, container name, image. Its image branch follows image-reference grammar: drop any `@digest`, take the last `/` component, then drop any `:tag`. Today it splits on the first `:`, so `registry.example:5000/org/redis:7` gives the wrong but valid `registry.example`. A name that fails the gate falls back to `docker-unidentified`. `trawl_unifi` gates both the UniFi model name and the plain APP-NAME and falls back to `syslog-unidentified`, with `syslog_appname` kept. VRL has no user-defined functions, so each transform carries its own copy of the gate.

**The noise filter matches the original unit.** With `TRAWL_SUPPRESS_HOMELAB_NOISE=true`, `trawl_journal` drops events whose `systemd_unit` is `serial-getty@ttyS0.service`. Other serial gettys still arrive.

**CI proves acceptance against the real canonicalizer.** The `repo-checks` job runs Vector but has no trawld. The `test` job runs trawld but has no Vector. A committed capture joins them. The Vector step runs the shipped configs over fixtures for every source, UniFi syslog included, and compares the delivered events to `crates/trawl-server/tests/fixtures/vector-capture/` byte for byte. It rewrites the file only on an explicit regeneration switch. A test in the `test` job posts that file to a real trawld's ingest preview and requires every event accepted and none rejected. A contract test asserts that each shipped gate is the same regex and that it agrees with `is_valid_service_name` on every one-byte string from 0x00 to 0xFF, on lengths 0, 1, 127, 128 and 129, and on `.`, `..` and `.x`. The stub's 200 proves only transport.

**Refused events raise an alert.** A wrong config copy, an edited drop-in or a third-party sender can still lose events this way. The alert packs gain a warning on any per-event rejection reason, amending ADR-0034. The rejection samples in trawld's `ingest_rejections` warning stay, so the operator can see which value was refused.

## Considered options

**Widen the server charset to admit `@`**, rejected. It fixes only templates: `(sd-pam)`, `:` and long names still fail. It makes each instance a service (`user@N` per uid, `systemd-fsck@<uuid>`), against ADR-0009's rule that a path dimension stays low in cardinality. It also reaches syslog `derive_service`, WAL and marker name parsing, and the service literals spliced into Parquet globs.

**Repair `service` on the server for HTTP**, rejected. trawld would invent an identity for every HTTP sender, which ADR-0009 forbids, and it would hide sender bugs.

**Answer a non-2xx status when an event is refused**, rejected. Vector drops a whole 4xx batch, valid events included (ADR-0043), and retries a 5xx that can never pass until its buffer stalls. It also breaks the documented API and preview contract.

**Rewrite bad bytes, unescape systemd escapes, or truncate long names**, rejected. That is the lossy sanitizer ADR-0009 deleted. Distinct units collide (`a@b` and `a-b`), and residue such as `dbus-:1.N-…` multiplies services.

**Send only the selected candidate as the original**, rejected. One `service_original` column would hold a unit on one event and an identifier on the next, and it would lose `sudo` inside a session scope.

**Strip more unit-type suffixes** (`.mount`, `.timer`, `.swap` and others), rejected. Those names pass the gate today, and stripping renames valid services.

**Run Vector and trawld together in a new CI job**, rejected. The Rust test would need an external Vector binary, so it would fail or skip silently in every local and pre-push run. The committed capture gives the same proof inside the jobs that exist.

**Remove the rejection samples from the daemon log**, rejected by the operator. They are trawld's only record of which value was refused. A sample quotes at most a bounded value from an authenticated sender, in a structured field of owner-only telemetry.

## Consequences

Services change for events that arrived before: children of `user@N` take their user-unit name, and `unknown` becomes a marked fallback. Saved queries and filters on the old names need updating. The events lost before were never stored, so nothing needs migrating.

The Debian package ships the configs as examples. A collector keeps losing events until its operator copies the new files into `/etc/vector/vector.d/` and restarts Vector. The sender guide and the release note say so.

`Rejection.message` no longer says its text never belongs in a log line. It says a bounded quote may appear in the structured `samples` field of the `ingest_rejections` warning.

A new drop-in that derives `service` must end in the gate. The contract test checks the copies it knows about. The sender guide states the rule for the rest.
