---
title: Your first query
description: Start a disposable Trawl trial in Docker with one command, check an exact query result in the browser and the CLI, then delete it.
---

In this tutorial we start a disposable Trawl trial on your Linux machine with
one command, sign in to the browser, run one query with an exact answer in the
browser, the CLI, and the TUI, and delete the trial again. Allow about five
minutes after the CLI is installed, plus the image downloads.

You need:

- Linux with Docker Engine and the Docker Compose v2 plugin, version 2.20 or
  later, reached over the local Unix socket. Your user must be allowed to use
  the socket, for example through the `docker` group.
- The `trawl` CLI. Install `trawl-cli` from APT, or the CLI from a release
  tarball, as [Installation](/getting-started/) shows. The trial needs no
  other Trawl executable: the server side runs from the published container
  image.
- Ports 15514 and 18090 free, and a browser on this machine. `--api-port` and
  `--web-port` on the first `up` move the trial to other ports.

The trial is supported on Linux only. `trawl trial up` checks the Docker
requirements before it creates anything, and its error names what is missing:
the `docker` command, the Compose plugin or its version, an unreachable
engine, or a `DOCKER_HOST` or Docker context that does not point at a
local `unix://` socket.

## Know the trust boundary

The trial is for evaluation on your own machine, not for exposure. The API
and the browser UI listen on `127.0.0.1` only, so no other machine reaches
them. Every other process on this machine can reach both ports. Root on this
machine, and every user who may use the Docker socket, can read every secret
the trial holds: the two API keys, the database passwords, and the private key
of the certificate. Do not forward the trial's ports to another machine, and
do not send it logs you would not show those users.

## Start the trial

```bash
trawl trial up
```

`up` pulls `ghcr.io/jakub/trawl` at your CLI's version and `postgres:18`,
starts PostgreSQL, creates the two databases and their owner roles, applies the
Fleet schema, generates a certificate, mints two API keys, starts `trawld` and
`trawl-web`, checks one authenticated query through the pinned certificate, and
loads 2000 sample events. Progress goes to stderr. Then it prints:

```text
The trial is up.

  Browser   http://localhost:18090
  API       https://127.0.0.1:15514

Sign in to the browser with the operator key; `trawl trial key` prints it.
The key files are readable by your user only:

  operator  ~/.local/state/trawl/trial/operator.token
  ingest    ~/.local/state/trawl/trial/ingest.token

Sample data: 2000 events from 2026-09-24T12:00:00Z to 2026-09-25T12:00:00Z.

Run the documented query:

  trawl -p trial query 'service=checkout _severity>=error | stats count() as errors by service'

It returns one row: service checkout, errors 20.

Next:

  trawl trial key     print the operator token for the browser sign-in
  trawl -p trial      open the terminal UI on the trial
  trawl trial status  show the trial's state
  trawl trial stop    stop the containers; `trawl trial up` resumes
  trawl trial down    delete the trial and everything it made
```

The paths and the sample dates follow your machine. The output never contains
a token. The trial keeps its files in `$XDG_STATE_HOME/trawl/trial`, which is
`~/.local/state/trawl/trial` by default. Nothing restarts at boot, and
nothing edits `~/.config/trawl/config.toml`.

The samples cover the 24 hours before `up` loaded them. The newest events sit
in the last 10 minutes before that moment, and the three `service=tutorial`
events that [Build a query](/use/query-tutorial/) uses sit at the newest
timestamp. With the default retention of 90 days, the samples age out 90 days
after `up`.

## Sign in to the browser

1. Open `http://localhost:18090`. Use `localhost` as written: `127.0.0.1` is
   a different browser origin.
2. Print the operator key and copy it:

   ```bash
   trawl trial key
   ```

   It prints the token and one newline, nothing else. Paste it into
   **API key** and select **Sign in**. The operator key holds every
   permission except `ingest`, so the **Health** page is present.
3. Search opens with **Quick start** and four examples. Each example's **Run**
   uses the selected **Date range**. The default range is the last 15 minutes,
   and the newest samples fall inside it right after `up`. Select `7d` in
   **Date range** to see all 2000 events for the trial's life.
4. Enter `service=checkout _severity>=error | stats count() as errors by service`
   and select **Haul**. Expect one row with service `checkout` and errors
   `20`.

The [browser guide](/reference/web-ui/) explains the time range, filters,
live mode, and saved queries.

## Run the same query in the CLI

The reserved profile `-p trial` reads the URL, the operator token, and the
certificate from the trial directory. It works for every `trawl` command and
never touches your config file.

```bash
trawl -p trial query --format json \
  'service=checkout _severity>=error | stats count() as errors by service'
```

Expect:

```json
{"errors":20,"service":"checkout"}
```

Property order can differ. The query names no time range, and `trawl query`
adds none, so this row holds for the trial's whole life. The four quick-start
examples run the same way:

```bash
trawl -p trial query '* | head 20'
trawl -p trial query '* | stats count() by service'
trawl -p trial query '_severity>=error | stats count() as errors by service | sort -errors | head 10'
trawl -p trial query 'service=web _severity>=warn | timechart span=5m count()'
```

Each returns rows: the first 20 events, one count per service, the services
with the most errors, and one bucket per five minutes of web warnings.

`-p trial` refuses to run when `TRAWL_URL` or `TRAWL_TOKEN` is set, when
`--url` or `--token` is given, or when your config file defines its own
`[profiles.trial]`. Each of those would send the command somewhere else
without you seeing it. It also refuses `--insecure`, because the trial is only
ever reached through its pinned certificate.

### Open the TUI

```bash
trawl -p trial
```

Press F1 for the shortcut list. Enter the documented query and press
Shift+Enter or F5. Expect the same row. Press Ctrl+Q to quit. See
[CLI and TUI workflows](/use/cli-tui/).

### Query the export without a server

Export the three tutorial events to Parquet, then query the file with no
server:

```bash
trawl -p trial query 'service=tutorial' --format parquet --output ~/tutorial.parquet
trawl query --data ~/tutorial.parquet --format json '* | stats count() by service'
```

Expect `{"count":3,"service":"tutorial"}`. Property order can differ. See
[Query local Parquet](/start/local-parquet/), and remove the file when you
are done.

## Stop and resume

```bash
trawl trial status
trawl trial stop
trawl trial up
```

`status` shows the trial id, the addresses, the image ids and digests, the
certificate fingerprint, the sample range as absolute timestamps, the setup
phases, and the containers. It warns when a `TRAWL_*` connection variable is
set. `stop` stops the containers and keeps the databases, the keys, the
samples, and the state. `up` resumes the same trial: the keys and the samples
stay as they were, and no new events are loaded.

## Delete the trial

```bash
trawl trial down
```

`down` prints the containers, volumes, and network it will delete and asks
`Delete all of it? [y/N]`. Without a terminal it exits non-zero and deletes
nothing. Pass `--yes` to skip the question. It deletes only resources that
carry this trial's id, then the trial directory, and exits 0. A second `down`
prints that there is nothing to delete and exits 0.

## From trial to installation

A trial never becomes an installation. When you have seen enough, install
fresh from a package, the tarball, or the Helm chart, and follow
[Deploy Trawl](/operate/deployment/). The table maps each step `up` did for
you onto the step you do yourself.

| The trial did | You do |
| --- | --- |
| Ran `trawld`, `trawl-web`, and PostgreSQL from the container image | Install `trawl-server` from [APT or a tarball](/getting-started/), or the [Helm chart](/operate/deployment/#install-with-helm) |
| Created the `fleet` and `trawl` databases with owner roles and ran `fleet-admin migrate` | [Provision the databases](/operate/deployment/#provision-the-databases) |
| Generated a certificate for loopback and pinned it in `-p trial` and `trawl-web` | [Configure TLS](/operate/access/#configure-tls) with a certificate your clients trust, and set `ca_cert` in their profiles when it is private |
| Minted the `trial-operator` and `trial-ingest` keys | [Create roles and keys](/operate/access/#create-roles-and-keys): personal keys for people, service keys for senders |
| Served the browser at `http://localhost:18090` with insecure cookies | [Set the browser origin](/operate/access/#set-the-browser-origin) behind an HTTPS reverse proxy |
| Resolved `-p trial` from its directory | Save the server as a [profile](/start/connect/#keep-more-than-one-server-in-profiles) in `~/.config/trawl/config.toml` |
| Loaded 2000 sample events | [Connect your log sources](/operate/ingestion/) |
| Started nothing at boot | `sudo systemctl enable --now trawld trawl-web` after the databases are set |

Nothing carries over. The installation starts with empty databases and an
empty data directory. The trial's keys are unknown to it, its sample data is
not imported, its self-signed certificate is not trusted by anyone, and its
loopback-only addresses give way to the addresses you configure. Run
`trawl trial down` when the installation is up, or keep the trial as a
scratch server on your own machine.

## What's next

- [Build a query](/use/query-tutorial/) adds filters, columns, and summaries on the sample data.
- [Search in the browser](/reference/web-ui/) explains the other search controls.
- [Vector integration](/getting-started/vector-integration/) sends logs continuously to an installation.
