# Advertised controls are functional or absent: disposition of the web UI placeholders

status: accepted (2026-09-05) — prep ruling record for #100 (item 7)

The web UI shipped with 13 visible controls that promise a capability and
deliver a "coming soon" title or toast: five Settings rail items that all
route to one placeholder card, a ⌘K search box and a notification bell with
no handler, two disabled user-menu rows, a ⌘⇧L hint nothing binds, a Help
rail item, and three toolbar actions (history Export, history Clear,
results-toolbar Save) that toast instead of acting. The 2026-09-04 browser
audit (`visual-evidence/ui-audit-2026-09-04/README.md`) counted them as
broken promises, not partial screens. fleet-ui chrome is shared with
coastwatch, which consumes it at a pinned trawl revision.

## Decision

**A control that is visible in shipped chrome works. A capability with no
server contract has no control. Nothing in the rail, topbar or a toolbar
says "coming soon".**

Per affordance:

- **Settings rail: Health and Schema stay, Sources / Retention / Users & API
  leave.** Health becomes a page over the routes the footer already reads
  (`/health`, `/stats`, `/dashboard`), permission-gated section by section
  the way the footer is. Schema deep-links to the existing inspector under
  Search, so the active mode flips to Search on click; that is the honest
  destination. The three that leave need a configuration API that does not
  exist (a config file is not one) and return with that API, each through
  its own prep.
  *Amended 2026-09-08 (slices G1/G3 prep): the Health page also carries
  query management — the running-queries list with per-row Cancel —
  under the EXISTING authority model (`/queries` reads under the query
  permission; cancel is ServerManage-any / QueryCancel-own-only by exact
  key id). The wire gains a server-computed `own` flag per entry so the
  Cancel control renders only where the caller could succeed; a
  confirmation guards it. Sequencing is also reversed from the #100
  table: the rail shrink cannot ship a Health entry that reaches a
  placeholder, so the Health page (G3) lands first and points today's
  entry at the real page; the rail shrink (G1) follows.*
  The Settings mode keeps `/settings` as its destination. That exact
  route resolves to `/settings/health` through same-document router
  navigation that replaces the intermediate history entry. Entering
  Settings from Search adds one final Health entry, so Back returns to
  Search without a redirect loop or a shell reload. Health comes first
  in the Settings rail, followed by Schema. Removed section paths render
  the existing NotFound page; they have no equivalent destination.
- **Topbar: the bell leaves; ⌘K becomes a real palette in fleet-ui.** The
  palette's first command set is routes only: the consumer's mode tabs and
  rail items, the data `Shell` already receives. Both `Meta+K` and
  `Ctrl+K` open it; the hint renders the platform's glyph. Actions,
  saved queries and search are not commands until a later prep says so.
- **User menu: Profile and API tokens leave; ⌘⇧L binds.** The chord is a
  window-level keydown in fleet-ui, beside the Escape arbitration that is
  already there.
  *Amended 2026-09-07 (slice D prep, ADR-0028): ⌘⇧L does NOT bind.*
  Ctrl/Cmd+Shift+L is Bitwarden's default autofill chord and Safari's
  ⇧⌘L "search with Google"; a page-level keydown cannot pre-empt an
  extension command, so the chord would autofill for some operators and
  toggle the theme for others, which is the broken promise this ADR bans.
  The hint chip leaves with the two rows; the theme item stays a plain
  menu command; no replacement chord is chosen here.
- **Help links to the docs site.** The native link opens `https://trawl.sh`
  in a new tab with `rel="noopener noreferrer"`. It stays in the rail's
  bottom slot, outside the command palette's internal route inventory.
  An in-app help page is not planned.
- **Results-toolbar Save reuses the editor's save modal.** One save flow,
  and the modal's input is the editor buffer (the audit's "Save after
  editing" finding), never the last executed query.
  Both Save controls capture that buffer when activated. The modal's
  preview and submission use the same captured text; URL filters and
  range are excluded. A readable URL change does not retarget an open
  modal. Closing discards the capture, and reopening captures the new
  buffer. A malformed link prevents opening Save and closes an already
  open modal under ADR-0027. This does not cancel a request already
  submitted or change the server's saved-query permissions and DSL
  admission rules.
- **History Export is client-side.** It serialises the loaded rows under the
  current filter as CSV or JSON through the existing download helper. It is
  not a second server export lane.
- **History Clear is a key clearing its own rows, under the existing query
  permission.** `query_history` is keyed by `key_id` and read under the
  same permission that runs a query; a key that may write its history may
  delete it. History is the client's convenience list. The operator's
  record of what ran is the opt-in query debug log (`server.query_log`),
  which this route never touches. No new permission, no cross-key
  deletion, a confirmation in the UI.

## Consequences

- Removing a topbar affordance changes coastwatch's chrome on its next
  `TRAWL_REV` bump. The bump PR carries the change; nothing here waits for
  it.
- The five-item Settings rail table in `state/section.rs` shrinks to two,
  and its `from_url` first-match rule stops hiding four dead entries.
- Each disposition lands through a #100 child slice with browser evidence;
  this ADR records the product decision so no child re-opens it.
