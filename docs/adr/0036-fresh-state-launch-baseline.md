# Launch with fresh Trawl and Fleet state

status: amended (2026-11-08): owned-residue detection stays looser than partition validity, see the Amendment

Trawl 1.0 starts with fresh application databases, a fresh shared Fleet keystore,
and a fresh owned data directory. Replace the pre-1.0 database conversion chains
with direct initial schemas, and refuse incompatible existing state without
changing it. Coastwatch will use the resulting Trawl revision and the same Fleet
baseline.

This decision supersedes the automatic storage-epoch set-aside and report-result
carry-over rules in ADR-0009 and ADR-0013. It does not authorize a reset of an
existing installation. Operators retain existing state and provision new state
explicitly; startup does not move, delete, or adopt an incompatible corpus.

Keep normal forward database migrations and current crash recovery. Storage
epoch validation, catalog identity, incomplete jobs, staged writes, and recovery
markers still protect state created by the new product. A fresh launch does not
make an interrupted current operation obsolete.

Fleet's initial schema preserves the current permission model and starts with no
keys, roles, or grants. Verify both Trawl and Coastwatch against that schema before
anchoring Coastwatch to the resulting revision. Coastwatch's own application
schema is outside this shared-state change.

## Amendment (2026-11-08): owned-residue detection stays looser than partition validity

Prep record for the untangle prep of 2026-11-08 (defect D13).

"Startup does not move, delete, or adopt an incompatible corpus." To keep
that promise, boot asks whether an unmarked data root could hold trawl's own
residue. A top-level directory with a date shape counts as owned
(`epoch.rs:323-325`, `:475-484`), so a query-only node refuses the root
instead of admitting it as a read-only archive (`epoch.rs:338-340`). The shape
check accepts impossible dates such as `2026-13-45`.

ADR-0009's amendment of this date gives partitions a strict calendar grammar.
Epoch must not use it here. With the strict grammar, an unmarked root whose
only entry is `2026-13-45` would change from refused to admitted. The
owned-residue check is a separately named, conservative classifier in the
same layout owner, and it accepts exactly what `is_date_partition` accepts
today. Owned residue grants no deletion authority: retention uses the strict
grammar, so `{env}/2026-13-45` is never a retention candidate.

**Considered, not adopted.** One date predicate for both questions. Retention
asks "is this a partition I may delete?" and epoch asks "could this be data
I wrote?". One predicate makes one of the two answers wrong.
