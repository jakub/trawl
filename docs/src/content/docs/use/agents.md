---
title: Ask Trawl from Claude
description: Connect Claude Code or Claude Desktop to a Trawl server through the MCP server, and follow a search URL on your phone.
---

Claude asks Trawl questions in plain language through the
[MCP server](/reference/mcp/). Claude runs `trawl mcp` on your machine, and
`trawl mcp` talks to your Trawl server with one API key. This page sets that up
for Claude Code and Claude Desktop.

You need `trawl` on the machine where Claude runs, and a Trawl server you can
reach from it. [Install](/getting-started/) `trawl` first if you have not.

## Give Claude its own key

Use a key that only Claude holds, so you can read its name in the audit trail
and revoke it alone. A key that reads logs needs three permissions:

- `query` runs searches and reads the results they keep.
- `schema_read` lists the fields of a service.
- `validate` checks a query without running it.

Leave out `export`, `stream`, and `query_cancel`. The MCP server has no tool
that uses them. Add `server_manage` only if you want `orient` to report how far
back each environment's data reaches. That permission also reads the dashboard
and cancels any key's query.

To let Claude keep a case file, add `investigation_read` and
`investigation_write`. An [investigation](/reference/mcp/#investigation) is the
agent's case file: the question it works on, every search it ran for it, the
rows it kept as evidence and why, and its findings. You read the same
investigation from your own key, in the browser or through your own profile,
while Claude works, and the kept evidence still reads after the source logs
leave the corpus, until its retention ends. [Create a key for an agent](/operate/access/#create-a-key-for-an-agent)
has the role with all five permissions, and the read-only role for you.

1. Ask the operator for a key with these permissions, or create one. The steps
   are in [Create a key for an agent](/operate/access/#create-a-key-for-an-agent).
2. Check what the key can do with the `whoami` call in
   [Keep a key in a curl config file](/operate/access/#keep-a-key-in-a-curl-config-file).
   The response lists the permissions.

## Save a profile for Claude

`trawl mcp` reads its server and key from a named profile, never from the
environment. Create a profile named `agent`, as
[Keep more than one server in profiles](/start/connect/#keep-more-than-one-server-in-profiles)
shows:

```toml
[profiles.agent]
url = "https://logs.example.com:5514"
token = "PASTE_THE_AGENT_KEY_HERE"
```

```bash
chmod 0600 ~/.config/trawl/config.toml
```

Use the API URL, not the browser URL. If you keep the key in a file instead,
leave `token` out of the profile and pass `--token-file PATH` to `trawl mcp`.
Give the profile one key source only. `trawl mcp` refuses two.

Check that the profile works before you involve Claude:

```bash
trawl doctor -p agent
```

## Add the server to Claude Code

1. Register the command:

   ```bash
   claude mcp add trawl -- trawl mcp --profile agent
   ```

   To make the server available in every project, add `--scope user` before
   the name: `claude mcp add --scope user trawl -- trawl mcp --profile agent`.
2. Start `claude` and run `/mcp`. Expect `trawl` listed as connected.
3. Ask a question:

   ```text
   Which services logged errors in the last hour?
   ```

Claude calls `orient` first. The reply tells it your environments, services,
and hosts, the time now, what the key may do, and the query language. Claude
then calls `search`, and answers from the summary and facets. It asks for more
rows with `read`, which does not run the query again.

`trawl mcp` refuses to start when `TRAWL_URL`, `TRAWL_TOKEN`, `TRAWL_PROFILE`, or
`TRAWL_INSECURE` is set in the shell that starts `claude`, and Claude reports
that the server failed. Unset them, then reconnect with `/mcp`.

## Add the server to Claude Desktop

1. Open the Claude Desktop configuration file. In **Settings**, open
   **Developer** and select **Edit Config**. The file is
   `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS
   and `%APPDATA%\Claude\claude_desktop_config.json` on Windows.
2. Add the server under `mcpServers`:

   ```json
   {
     "mcpServers": {
       "trawl": {
         "command": "trawl",
         "args": ["mcp", "--profile", "agent"]
       }
     }
   }
   ```

   If Claude Desktop cannot find `trawl`, give `command` as the full path to the
   binary, such as `/usr/local/bin/trawl`.
3. Quit Claude Desktop and start it again. Expect the `trawl` tools in the
   tools menu of a new conversation.

Claude Desktop runs the same command, so the profile and the refusals are the
same as for Claude Code.

## Read a result

Every answer starts with a sentence about the outcome, then the facts you need
to trust the rows:

- **Rows produced and captured.** The first says how many rows the query
  returned. The second says how many the server kept. If it kept fewer, the
  answer says it truncated.
- **What the query itself cut.** The answer says whether the query sampled,
  limited, or deduplicated rows.
- **What this reply left out.** The answer says how many cells it abbreviated,
  how many columns it left out, and how many captured rows follow.

Ask Claude to show the rest of the rows, or to show a cut cell in full, and it
reads the saved result. A saved result expires. The answer names the time it is
readable until. After that, Claude runs the query again.

Claude often adds a hint, such as one service dominating the rows, a sender
whose events arrive late, or a field the catalog holds no pin or observation
for. Each hint names
the rows it was computed over.

## Follow a lead across rows, services, and hours

After a search, ask about what you found. "What happened around row 3?" calls
`context`, which reads the rows just before and after that row on the same
environment and host. "Where else does 10.0.4.17 appear this week?" calls
`sightings`, which counts the matches in each service and environment, with the
first and last time seen, and names the fields that hold the value. "Did auth
failures rise this hour against the last?" calls `compare`, which counts the
same filter in both hours from one read and ranks the groups that changed most.
Each answer is a new saved result that Claude can page with `read`, and each
says what its counts are over. The key needs `query` only. The tools are in
[the MCP server reference](/reference/mcp/#correlation-tools).

## Follow a search URL on your phone

When the server has a link origin, each search answer ends with a search URL.
Open it on your phone, signed in to the browser UI, to see the same query over
the same time range in Search.

The operator sets the origin once in the trawld configuration:

```toml
[server]
link_origin = "https://trawl.example.com"
```

Use the address people open in a browser. See
[`link_origin`](/reference/configuration/#server). Without it, the answer says
there is no search URL and why. Claude cannot build a link by itself.

## If the connection fails

- **Claude reports the server failed at start.** Run
  `trawl mcp --profile agent < /dev/null` in a terminal. A refusal prints one
  line that names the cause, such as a variable that is set or a profile that is
  missing, and exits with `2`. A working profile prints
  `trawl: the MCP session did not start: the client closed the connection
  before initializing` and exits with `1`, because the closed stdin ended the
  session before a client spoke: that line means the setup is right and the
  problem is in Claude's configuration. The exit codes are in
  [the MCP server reference](/reference/mcp/#exit-codes).
- **The first tool call is refused.** The refusal names an error code and a
  request id. A `forbidden` code means the key lacks a permission from the
  list above. A rate-limit refusal names how long to wait.
- **The server is unreachable.** `trawl mcp` does not contact the server at
  startup, so the failure shows on the first call. Run
  `trawl doctor -p agent` to see which check fails.
