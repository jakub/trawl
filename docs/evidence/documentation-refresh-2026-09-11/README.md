# Documentation refresh validation

Checked on 2026-09-11 against source base `92d3c28c917fd820f3e446971e6167e90c6446e7`.
The local agent documentation refresh is a separate commit on the same branch.

## Writing pass

The writing pass starts from `b0eb1bc2` and ends at `cb31153e`, six commits
over 40 files. It applies the technical-writing standard to every page: one
Diátaxis mode per page, commands as instructions with the condition first,
one thought per sentence, no rationale or history in reference pages, and
every route, flag, key, default, path, and shortcut checked against the code
in the same tree.

Seven writer seats edited disjoint file sets in the shared worktree, four on
Fable 5.1 (quickstart and usage, HTTP API, operator installation, diagnostics
and crash dumps) and three on Opus 5 (DSL, the other references, architecture
and contributor pages), all at the session's inherited effort. The seats
reported 68 claims corrected against the source. Among them: the API page
documented `format` as a body field where the export handler reads it from
the query string, the configuration page omitted four keys, the CLI page had
no exit codes or environment variable table, the DSL page omitted ten scalar
functions and stated the streaming predicate too widely, the component
diagram routed ingest through the browser proxy, and the Vector guide did not
set the interpolation variable that Vector 0.57 and later require.

Two adversarial reviews by gpt-6-astra at high effort covered the range from
the immutable base. Job `032a7fa7-85e0-4dbf-afae-6df84f3c17ab` reviewed
`b0eb1bc2..8cabb845` and returned seven findings, all confirmed at the cited
code and fixed in `d04c7ef9`. Job `de0bb0f0-2065-4b1c-9ebf-4f22a9359c2c`
reviewed the final quickstart round, `8cabb845..15bdf9cb`, and returned four
findings, all confirmed and fixed in `cb31153e`. No finding was rejected.

Checks at `cb31153e`:

- `npm run check` passes: 38 HTML pages, 2,866 local links, 25 TOML blocks,
  and all 18 DSL stages.
- All 108 Bash blocks pass `bash -n`. Every page has a title and description.
- A sweep for em and en dashes, curly quotes, decision-record mentions,
  development history, real homelab identifiers, and the name Coastwatch
  outside the development guide finds nothing. The four remaining em dashes
  quote program output verbatim.
- The site holds 52,293 words, from 52,892. The HTTP API page grew from 5,546
  to 9,591 words because every route now has the same block with its errors
  table, and the DSL page held at 9,756 because every rule stayed.

Not done in this pass:

- No sample dataset ships with the site. The build host has no Parquet
  tooling, so the no-server path still needs the reader's own files.
- The tutorial and operator commands were syntax-checked and their quoted
  outputs verified against the source strings. They were not executed.
- `TUI_ROADMAP.md` at the repository root and the `docs/evidence` directories
  are outside the site and were not changed.
- One code observation, not changed: `POST /api/v1/query` answers
  `offset + limit > max_result_rows` with the `ingest_error` code
  (`crates/trawl-server/src/handlers.rs:73`). The API page documents the
  code as it is.

## Clean manual revision

The follow-up revision starts from `9d0ef610`. It removes the public decision
index, roadmap, authentication cutover runbook, and version-specific upgrade
guide. Current references describe behavior directly, without release history.
Runtime validation evidence now lives in this directory, outside the site build.
The sections below record the earlier rewrite and its checks.

Three Astra writers at medium effort edited disjoint reader, operator, and
architecture sections. A Daybreak reviewer at high effort checked the final
content against the same base, using the user's cross-model review preference.
The review corrected the `server_manage` permission description, including its
dependency on `query` for inspecting system-owned work, query-only initialization
of a missing data root, disk-pressure diagnostics, and fresh role creation in the
API key example. The reviewer verified those corrections and returned no remaining
findings.

Validation for this revision:

- `TRAWL_DOCS_PREVIEW=1 npm run check` passes for 38 HTML pages, 2,737 internal
  links, 21 TOML blocks, and all 18 DSL stages.
- All 90 Bash and shell examples pass `bash -n`.
- Rendered page text contains no ADR mentions, legacy labels, old storage archive
  names, or release-change callouts.
- All four removed pages and the former public runtime record return HTTP 404
  from the HTTPS preview after the server is ready.
- [Chromium checks](clean-manual-browser-checks.json) cover five routes, desktop
  and mobile overflow, search, and certificate verification. Exact-phrase `"ADR"`
  and `legacy` searches return no results; `repin` returns 20. No JavaScript page
  errors were observed. Unquoted `ADR` can match the word `a` through search's
  fallback matching, so it is not an absence check.
- The preview runs at `https://fractal.reverse-manta.ts.net:8446/` through
  Tailscale Serve. LAN HTTP access remains on port 4321.

The first-query and restore runtime drills were not repeated for this editorial
revision. Their earlier outcomes and limits remain recorded below. No package
versions, application code, repository decision records, or releases changed.

## Scope

The rewrite retains Astro and Starlight and organizes 41 documentation pages
under Start here, Use Trawl, Operate Trawl, Reference, Architecture, and Contribute.
A separate 404 page provides recovery links. All 15 original page routes remain.

Three Astra workers at medium effort wrote disjoint reader, operator, and
architecture sections in the shared worktree. The root wrote navigation,
styles, contributor guides, checks, and integration fixes. A Daybreak worker at
high effort reviewed the candidate against the immutable source base. Daybreak
replaced the usual Claude review to respect the saved quota preference.
This was a cross-model review, not a cross-family review.

The review found and verified corrections for package names, source-build
coverage, backup read privileges, restored filesystem ownership, Vector drop-in
assumptions, and an unignored Python cache. No material findings remained after
those corrections.

## Site checks

- `TRAWL_DOCS_PREVIEW=1 npm run check` passes with 42 HTML files, 3,335 local
  links, 21 TOML blocks, and all 18 DSL stages.
- All 238 original heading anchors remain across the 15 original routes.
  The comparison used Astro's Markdown processor against the source base,
  then checked the generated IDs in built HTML.
- All 91 Bash and shell code blocks pass `bash -n`.
- Three documented Helm variants render, including API-only and both browser
  origin/cookie arrangements. The complete deployment YAML block was also
  extracted and rendered. This does not prove a Kubernetes rollout.
- `git diff --check` passes.
- The lockfile was installed with `npm ci`. The local build used Node 26.8.1,
  Astro 7.1.1, and Starlight 0.41.3. The revised CI declares Node 22; no hosted
  CI run was triggered.

The published APT index was read at
`https://trawl.sh/apt/dists/stable/main/binary-amd64/Packages`.
It listed `trawl-cli` and `trawl-server` at `0.4.0-1`, plus a legacy `trawld`
package at `0.1.0-1`. The corrected guide selects the current package names.
No APT installation or release download was performed.

## Tutorial and restore

The [runtime record](runtime-checks.json)
contains the exact outcomes and limits. The first-query Bash examples ran with
PostgreSQL 18, isolated database identities, TLS, and a private filesystem tree.
Existing Trawl 0.4.0 development binaries reported revision `a347a177`. Their
cached DuckDB shared-library path was supplied for this check. The source delta
to the review base in the checked server/core/auth crates was confined to syslog
conversion, which this HTTP tutorial did not exercise.

The tutorial produced three events, an exact count of three, the expected
error row, and a readable Parquet export. The restore drill stopped its sole
writer, dumped both databases, archived the filesystem, restored separate
databases and a separate tree, and verified catalog and API identity. It then
verified three restored events, a fourth new event, and four events after restart.

The first restart assertion ran immediately after health became OK and saw only
the three compacted events. The new event remained in WAL. A bounded poll in the
next drill waited for compaction and observed all four. The recovery guide now
separates dependency health from expected-data readiness.

The drill did not exercise package installation, systemd units, UID restoration,
browser cookies, saved reports, repin, or a shared-Fleet production cutover.
The test container, daemons, and private credentials were removed afterward.

## Browser and preview

[browser-checks.json](browser-checks.json) records desktop and mobile checks
with the installed Chromium and the repository's Playwright dependency. Six
representative routes rendered without horizontal overflow. Search returned
catalog and recovery results for `repin`. Mobile navigation opened the first-query
page, and no JavaScript page errors were observed. The screenshots capture
light and dark themes, mobile layout, and search.

The local Astro preview binds IPv4 port 4321. HTTP requests to loopback, the
host's LAN address, tailnet address, and MagicDNS hostname returned 200 with the
preview banner. These requests ran on the serving host, not on a separate client.
An unlisted Host header returned 403. Repository-file and Vite filesystem paths
returned 404. The build includes `noindex, nofollow`; only built documentation
is served. No release, Git push, PR, or site publication occurred.
