---
title: Connect to a server
description: Sign in to an existing Trawl server from the browser or the CLI.
---

Ask the operator for a URL and an API key. Browser URLs point at `trawl-web`
and API URLs at `trawld`. The key's permissions, such as `query` and
`schema_read`, decide what you can do, not its role name.

## Sign in from the browser

1. Open the browser URL.
2. Enter your key in **API key** and select **Sign in**.
3. On Search, enter `last=15m | head 20` and select **Haul**, or press Ctrl+Enter.

Expect up to 20 events in **Events** and **Connected** in the status bar.
`trawl-web` keeps your key in an encrypted session cookie, and a shared Fleet
session may have signed you in already. If a control is missing, ask the
operator about the key's permissions. The [browser guide](/reference/web-ui/)
covers the rest.

## Configure the CLI

[Install](/getting-started/) `trawl`, then create `~/.config/trawl/config.toml`,
private to your account.

```bash
install -d -m 0700 ~/.config/trawl
```

```toml
[server]
url = "https://logs.example.com:5514"
token = "PASTE_YOUR_API_KEY_HERE"
```

```bash
chmod 0600 ~/.config/trawl/config.toml
trawl query 'last=15m | head 20'
```

Expect up to 20 events and a footer with the number of rows returned, such
as `3 row(s)`, or `no results` when nothing matched. Use the API URL, not the
browser URL. Keep `--token` off the command line, because shell history keeps
it. `TRAWL_TOKEN` in the environment overrides the file.

## Keep more than one server in profiles

A profile overlays `[server]` for one command.

```toml
[server]
url = "https://logs.example.com:5514"
token = "MAIN_READER_KEY"

[profiles.lab]
url = "https://lab-logs.example.com:5514"
token = "LAB_READER_KEY"
```

```bash
trawl --profile lab query 'last=15m | head 20'
trawl --profile lab
```

The first command queries `lab` and the second opens the TUI against it.
`TRAWL_PROFILE` selects a profile too. `--config /path/to/client.toml` reads a
different file. Name the server every time you change something.

## Fix a connection problem

| Symptom | Check |
| --- | --- |
| Connection refused or timed out | API host and port, network route, `trawld` running |
| Certificate validation error | Hostname in the URL, certificate chain, your trust store |
| Authentication rejected | Complete token, key expiry or revocation, inherited `TRAWL_TOKEN` |
| Permission denied | The permission the action needs, such as `query` or `schema_read` |
| A query returns no rows | Time range, service spelling, whether logs have arrived |

For a self-signed or private-CA server, copy its CA certificate to your
machine and set `ca_cert = "~/.config/trawl/lab-ca.pem"` under `[server]` or in
the profile. `trawl` then trusts only that CA for the connection and still
checks the hostname. `insecure = true` turns verification off completely, and
`trawl` prints a warning on every run. Use it only for a short test. Continue
with
[Build a query](/use/query-tutorial/) or [CLI and TUI workflows](/use/cli-tui/).
