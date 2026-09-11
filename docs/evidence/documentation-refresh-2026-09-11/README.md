# Documentation refresh validation

Checked on 2026-09-11 against source base `92d3c28c917fd820f3e446971e6167e90c6446e7`.
The local agent documentation refresh is a separate commit on the same branch.

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

The [runtime record](../../public/validation/documentation-refresh-2026-09-11.json)
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
