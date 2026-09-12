---
name: trawl-ops
description: Diagnose and operate a selected Trawl server, including health, bounded queries, catalog conflicts, repin jobs, and retention recovery.
---

# Trawl operator tasks

Select the named profile or explicit server configuration. Do not infer a
target from the default profile or a remembered host. Use the existing
credential and TLS trust configuration without printing secrets or adding
`--insecure` automatically.

Check the installed version and relevant `trawl COMMAND --help`. Use
`env -u TRAWL_TOKEN` for help-only calls because Clap can print environment
values. [lib.rs](../../../crates/trawl-cli/src/lib.rs) owns commands and
[policy.rs](../../../crates/trawl-server/src/policy.rs) owns permissions.
Do not infer permissions from a role's name.

## Pick the task

| Task | Starting point |
| --- | --- |
| Health and identity | `GET /api/v1/health` and authenticated `GET /api/v1/whoami`; inspect checks, not only HTTP status |
| Query diagnosis | Bounded CLI query with an explicit time window and row limit; `/api/v1/stats` and `/api/v1/queries` when needed |
| Catalog conflicts | `schema fields`, `schema field FIELD`, `schema conflicts`; then the [CLI schema guide](../../../docs/src/content/docs/reference/cli.md) |
| Repin or pin reclamation | The same CLI guide and the installed subcommand help; [repin modules](../../../crates/trawl-server/src/repin/) explain recovery |
| Retention suppression | Server logs, [retention.rs](../../../crates/trawl-server/src/retention.rs), and [epoch.rs](../../../crates/trawl-server/src/epoch.rs) |
| Crash capture | [Crash-dump guide](../../../docs/src/content/docs/reference/crash-dumps.md) |

Use the [API reference](../../../docs/src/content/docs/reference/api.md) for
route shapes; confirm the owning handler when changing state. Queries can
write history and telemetry even when they leave the corpus unchanged.
Current event and repair names come from
[envelope.rs](../../../crates/trawl-server/src/ingest/envelope.rs).

## Schema changes and recovery

Repin is a whole-corpus operation requiring `schema_write` on an ingest-enabled
node. Confirm the field and target type. For a new operation, start with
`schema repin FIELD --to TYPE --dry-run`; it avoids corpus rewriting but still
records a job. Its result is
an estimate, not a reservation of the corpus. Choose a severity dialect from
the sender's meaning, never from the overlapping numeric ranges.

Use `--yes` for an authorized noninteractive execution. Add `--force` only for
accepted loss or a requested resurrection pass, with the applicable
`--max-nulled-rows` and `--max-ambiguous-rows` bounds. Use `--wait` or poll
`schema repin-status`, which returns the running job or the newest job and
accepts no job ID. Match its ID to the submitted operation and inspect its
field, target type, mode, and result. Status inspection needs `schema_read`.
If submission timed out without an ID, use actor, timing, and request details
as correlation evidence, not proof. If the status is ambiguous or another job
has replaced the original, its outcome remains unknown; inspect server logs
or the job store before retrying. Cancellation requests need subsequent status
verification and cannot unwind cutover.

`schema gc-pins --dry-run` reports candidates; without it the command deletes
catalog entries. `schema ack` changes acknowledgement state, not data quality.
For query cancellation, identify the exact query and verify the result.

Both `.pre-schema-v2` and `.pre-epoch-3` archives can suppress disk-pressure
deletion. Inspect the logged path and active repin state. Repin markers and
staging roots are recovery state, not routine cleanup targets. A timeout or
lost response leaves mutation outcome unknown; inspect state before retrying.
