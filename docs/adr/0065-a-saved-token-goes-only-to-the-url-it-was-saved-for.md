# A saved token goes only to the URL it was saved for

status: accepted (2026-11-08), prep record for the untangle defect D11

Ordinary CLI commands send a saved API token to whatever URL the command names. `Config::apply_profile` copies a profile's `url` and `token` into `[server]` one key at a time (`crates/trawl-cli/src/config.rs:259-264`), so a profile with its own `url` and no `token` keeps `[server].token`. `Config::apply_overrides` then replaces the URL again for `--url` or `TRAWL_URL` (`config.rs:280-287`). `Config::load_token` returns the direct token or the flattened `[server].token` (`config.rs:332-342`), and `connection` builds the client from the two (`crates/trawl-cli/src/lib.rs:696-707`). `trawl --url https://lab2 query ...` therefore sends the homelab key to lab2. Ordinary commands also accept a URL with userinfo, a query, or a fragment.

`trawl doctor` and `trawl mcp` already refuse exactly these hazards (`crates/trawl-cli/src/doctor/resolve.rs:18-38`, ADR-0047, ADR-0058). The untangle prep of 2026-11-08 asked the human what ordinary commands should do. The question and the answer, verbatim:

> D11: ordinary commands like `trawl query --url https://other-host` send the token saved for your normal server to that other host. A profile with no token of its own also borrows it. `trawl doctor` and `trawl mcp` already refuse this. What should ordinary commands do?
>
> Options offered: Token stays with its URL (Recommended) / Same rule as doctor / Keep it, document it
>
> A: "Token stays with its URL (Recommended)"

This record makes "its URL" exact.

## Decision

**Every credential carries its source and the API base it is bound to.** The resolver records each candidate before it applies any destination override:

- `[server].token` is bound to `[server].url`. When `[server].url` is absent, it is bound to the default `https://localhost:5514`.
- A profile's own `token` is bound to the profile's effective URL: its own `url`, or `[server].url` when it sets none.
- A profile with no `token` keeps `[server].token` as a candidate, still bound to `[server].url`. The candidate is usable only when the selected base equals that binding. A trust-only profile, such as `[profiles.dev] insecure = true`, keeps working. A profile whose `url` names another server does not carry the key there.
- `TRAWL_TOKEN` is bound to `TRAWL_URL` when both are set. Otherwise it is bound to the saved effective base, selected before `--url` or `TRAWL_URL` applies.
- A command-line `--token`, or the new ordinary `--token-file PATH`, goes to the final target. Giving both is a usage error. A command-line credential wins over `TRAWL_TOKEN` and over saved candidates.

**The final target is the first of** a command-line `--url`, `TRAWL_URL`, the profile's effective URL, and `[server].url`. A saved or exported credential goes to the final target only when its binding equals that target. A command-line `--url` that differs from every binding needs a command-line `--token` or `--token-file`, even when `TRAWL_TOKEN` is set. `TRAWL_URL` without `TRAWL_TOKEN` reuses a saved credential only for an equal base. An exported token is saved state, just as `config.toml` is.

**Provenance comes from clap.** clap merges `--url` with `TRAWL_URL` and `--token` with `TRAWL_TOKEN` into one value (`lib.rs:35-44`), so the merged `Option` cannot say which one the user typed. The ordinary path reads `ArgMatches::value_source` (clap 4, `Cargo.toml:47`) to tell `CommandLine` from `EnvVariable`. The trial's refusal and warning lists derive the environment names from the same clap metadata instead of hard-coding them (`crates/trawl-cli/src/trial/profile.rs:30-32`, `trial/lifecycle.rs:1482-1488`).

**A binding refusal is not token absence.** When no credential qualifies but one was saved or exported for another base, the command fails before any request. The message names the credential's source and the target's source, says that the API bases differ, which can be a path prefix alone, and names `--token` and `--token-file` as the remedies. It never prints the token, the key file's path, or a rejected URL. `trawl validate` validates locally only when no credential exists anywhere. A binding refusal stays an error, so a mismatch never turns into a silent local result. A selected credential source that is empty, unreadable, or invalid fails and never falls through to the next source.

**Every URL passes the doctor's shape check before any key is read.** Each target and each saved URL goes through `check_url` (`doctor/resolve.rs:243`): no userinfo, query, fragment, encoded `?` or `#`, backslash, whitespace, or control character, and the scheme is `http` or `https`. A saved URL that used to work with userinfo or a query string is now refused.

**Base equality.** Two bases are equal when they parse, with the parser the HTTP client uses (the `url` crate, already a workspace dependency at `Cargo.toml:146`), to the same scheme, host, effective port, and path prefix. Parsing lowercases the scheme and a DNS host and serializes address literals one way. A default port (443 for `https`, 80 for `http`) is the same as no port. One trailing `/` on the path is trimmed. Path case and percent-encoding are kept. Every other difference counts as a different base and fails closed: a DNS alias, `localhost` against `127.0.0.1`, a trailing dot, another port, another prefix. The request uses the same validated base that the comparison used.

**Shared primitives, separate policies.** One CLI connection module holds the primitives: the URL check, base identity, a key with its source, the bounded key-file reader (`doctor/resolve.rs:634`), and connection assembly. Four policy entrypoints use them: ordinary commands, the doctor (ADR-0047), `trawl mcp` (ADR-0058), and the trial (ADR-0045). No caller passes a set of switches that turns a stricter policy into a looser one. The doctor and `trawl mcp` keep their stricter rules and their refusal texts: their profiles need their own `url` and never take `[server].token`, even when the bases match.

**One configuration read.** The resolver returns the parsed `Config` with the connection. `trawl mcp` takes `ui.timezone` from that snapshot. Today `display_zone` opens the file a second time with `Config::load` (`crates/trawl-cli/src/mcp.rs:220-224`), so a file replaced in between gives a zone from another file, and a deleted file gives the defaults.

**The TUI resolves before terminal mode.** The TUI already builds its client before `enable_raw_mode` (`crates/trawl-cli/src/tui/mod.rs:866-869`). A binding refusal therefore prints to a normal terminal.

**The trial refuses `--token-file`.** `-p trial` already refuses `--url`, `--token`, `TRAWL_URL`, and `TRAWL_TOKEN` (`trial/profile.rs:67-80`). It refuses the new `--token-file` too. ADR-0045's amendment of 2026-11-08 records this.

## Premise checks

Checked at `b71f9e6e`. `62ee208b..b71f9e6e` changes only `crates/fleet-dev`, `fleet-dev.toml`, and `Cargo.lock`.

- `config.rs:259-264` copies a profile's `url` and `token` separately. `config.rs:332-342` falls back to the flattened `[server].token`.
- `lib.rs:696-707` is the one connection function that every ordinary command and the TUI use.
- `lib.rs:35-44` binds `TRAWL_URL` and `TRAWL_TOKEN` to `--url` and `--token` with clap's `env`. The ordinary path calls `Cli::parse_from` and drops the `ArgMatches`.
- `doctor/resolve.rs:243` holds `check_url`, and `:634` holds the bounded `key_from_file`. `check_url` keeps an explicit default port, so base equality needs its own default-port step.
- `crates/trawl-client/src/client.rs:248` and `:273` build clients with `redirect::Policy::none()`, so a key cannot leave its target through a redirect.
- `mcp.rs:220-224` reads the config again in `display_zone`.
- `trial/profile.rs:67-80` refuses only the URL and token inputs, because `--token-file` does not exist for ordinary commands.
- `doctor/mod.rs:256` and `mcp.rs` each declare their own `--token-file`. A global `--token-file` beside them collides in clap, so the flag gets one coherent definition.

## Considered and rejected

- **The doctor's rule for ordinary commands** ("Same rule as doctor"). The human chose "Token stays with its URL". The doctor's rule refuses `TRAWL_URL` and `TRAWL_TOKEN` outright and makes every profile name its own key.
- **A profile that sets `url` never inherits `[server].token`, even for an equal base.** This adds a rule beyond the ruling. The ruling constrains where a token goes, not how tables are shaped. The binding check alone stops the leak.
- **Counting `TRAWL_TOKEN` as `--token`.** clap merges the two. An old exported token would follow a newly typed `--url`, which is the leak the ruling forbids.
- **Textual URL comparison.** It needs no parser, but it compares a string that the client never sends. Comparing the parsed base that builds the request keeps the two in step.
- **Origin-only equality.** Two installations behind one proxy share an origin and differ by path prefix. Origin equality would send one's key to the other.
- **One resolver with a mode enum.** A flag set could relax the doctor's or `trawl mcp`'s stricter selection by accident. Separate entrypoints over shared primitives cannot.
- **Deriving the trial's environment names from clap in a later change.** The new `--token-file` has to reach the trial's refusals in the same change, and deriving the names from clap metadata is how a later flag stays covered.
- **Printing both origins in the refusal.** When the bases differ only by path prefix, the two origins read the same and explain nothing.

## Consequences

- `Config::load_token` is deleted. Only the connection module and the trial overlay read `[server].token`.
- The documented flag and environment equivalence narrows: `TRAWL_TOKEN` no longer authorizes a different command-line `--url`. `reference/cli.md`, `reference/configuration.md`, and `start/connect.md` say so, and say how profiles bind tokens.
- A config URL with userinfo, a query, or a fragment stops working for every command.
- No persisted state changes. `config.toml` keeps its format.
