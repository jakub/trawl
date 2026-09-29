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

Run `trawl doctor` first. It checks the connection step by step, from where you
run it, and names the first step that fails. Give it the profile you use:

```bash
trawl doctor -p lab
```

With only `[server]` in the file, give the API URL instead. The doctor then
checks everything but the key, because `--url` never reads the config file:

```bash
trawl doctor --url https://logs.example.com:5514
```

Unset `TRAWL_URL`, `TRAWL_PROFILE`, `TRAWL_TOKEN`, and `TRAWL_INSECURE`
first, because the doctor refuses to run when one is set. To check the browser
URL too, add `--web-url` with the browser origin. Every failed line ends with
a next action. The table maps each check to its usual fix.

| Check | If it fails |
| --- | --- |
| `connection.config` | Fix the source it names: the config file, the profile, its `url`, `ca_cert`, or the token file |
| `api.transport` | Check the API host and port, the network route, and that `trawld` runs |
| `api.tls` | Set `ca_cert` in the profile to the server's CA, as below. Check that the URL uses `https` and the hostname in the certificate. Turn `insecure` off |
| `api.health` | Check that the URL names the `trawld` API, not the browser URL or a proxy |
| `api.health.<key>` | Ask the operator. The server reports that subsystem as failing |
| `api.identity` | Get a complete, unexpired, unrevoked key. A key with no permissions needs a role |
| `web.transport` | Check the browser origin's host and port, and that its certificate is from a CA your system trusts |
| `web.origin` | Ask the operator to add the browser origin to `trawl-web`'s `public_origins` |

A line that is `not_sampled` with `blocked` waits on an earlier check. Fix
that check first. The [CLI reference](/reference/cli/#doctor-mode) lists every
check and outcome.

The doctor does not check two problems. When an action is denied, the key
lacks the permission that action needs, such as `query` or `schema_read`.
When a query returns no rows, check the time range, the service spelling, and
whether logs have arrived.

For a self-signed or private-CA server, copy its CA certificate to your
machine and set `ca_cert = "~/.config/trawl/lab-ca.pem"` under `[server]` or in
the profile. `trawl` then trusts only that CA for the connection and still
checks the hostname. `insecure = true` turns verification off completely, and
`trawl` prints a warning on every run. Use it only for a short test. Continue
with
[Build a query](/use/query-tutorial/) or [CLI and TUI workflows](/use/cli-tui/).
