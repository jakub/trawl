# Launch with fresh Trawl and Fleet state

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
