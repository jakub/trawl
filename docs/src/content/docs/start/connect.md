---
title: Connect to a server
description: Connect the browser or CLI to an existing Trawl installation.
---

Ask the operator for a browser URL or API URL and an API key with the
permissions you need. Those URLs can differ: browsers use `trawl-web`, while
the CLI talks directly to `trawld` over HTTPS. A name such as `reader` is a
role label, not a fixed permission tier.

## Use the browser

Open the browser URL and sign in with your API key. The browser proxy keeps
the token in an encrypted session cookie; subsequent browser requests use
that session. If your Fleet installation shares sessions, you may already
be signed in through another Fleet application.

Start on Search and enter a bounded query such as:

```text
last=15m | head 20
```

Use **Haul** to run it. Follow the [browser guide](/reference/web-ui/) for
filters, history, saved queries, and reports. If an action is unavailable,
check the key's permissions with the operator instead of changing its role
name locally.

## Configure the CLI

Install `trawl`, then create `~/.config/trawl/config.toml` with your editor.
Keep the directory private and the file readable only by your account:

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

Use the operator's API URL, not the browser login URL. The example hostname
is a placeholder. Avoid putting the token in `--token` commands that remain
in shell history. The CLI also accepts `TRAWL_TOKEN`; an inherited value
overrides the config token.

## Keep servers in named profiles

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

`TRAWL_PROFILE` selects a profile too. Explicitly select the target before
running an operational command. A profile can point at live data.
Use `--config /path/to/client.toml` to keep a separate configuration file.

## Resolve connection problems

| Symptom | Check |
| --- | --- |
| Connection refused or timed out | API hostname, port, network route, and daemon status |
| Certificate validation error | Correct hostname, certificate chain, and client trust store |
| Authentication rejected | Complete token, expiration/revocation, and an inherited `TRAWL_TOKEN` |
| Permission denied | The permission required by this action, such as `query` or `schema_read` |
| A successful query returns no rows | Time range, service spelling, and whether logs have arrived |

For a local self-signed test server, `insecure = true` in `[server]` disables
certificate verification. Use that only for the intended test connection;
permanent connections should use a trusted certificate.

Continue with [Build a query](/use/query-tutorial/) or
[CLI and TUI workflows](/use/cli-tui/).
