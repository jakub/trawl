# Fresh schema contract tests

The production migration directories each contain the current initial schema
and future forward migrations. These fixtures are test evidence only. No
production runner reads them.

`fixtures/trawl/` and `fixtures/fleet/` contain the exact migration bytes from
commit `c7b9afc03f8b5048293e7b4207b4dfc9ff76c213`. They provide an independent
reference for the final schema and actual old migration histories. Do not edit
them to make a new schema pass equivalence checks.

`cases.rs` is compiled by Trawl's `schema_pg` test and Fleet's `migration_pg`
test. It applies both old and new SQL to owned PostgreSQL schemas, then compares
columns and their order, types, defaults, nullability, identity settings,
constraint names and definitions, index definitions and predicates, sequences
and ownership, and every seed row. Only generated pin timestamps and the
catalog UUID value are excluded from the seed comparison.

The tests also snapshot schema, ledger, table rows, and sequence state around
refused migrations. Cases cover partial/full/dirty old histories, nonempty
untracked databases, tampered checksums, dirty current history, unknown older
and future versions, retry after refusal, cancellation during lock acquisition,
idempotent restart, and an isolated forward-migration fixture.

Run only against a disposable PostgreSQL instance owned by the test run. Set
`DATABASE_URL` explicitly to that instance; SQLx creates per-test databases.
Never use the persistent development cluster or a production DSN.

```bash
DATABASE_URL="$OWNED_POSTGRES_URL" cargo test -p fleet-auth --test migration_pg
DATABASE_URL="$OWNED_POSTGRES_URL" cargo test -p trawl-server --test schema_pg
```

Fleet runtime tests additionally call `validate_schema` and `KeyStore::connect`
on old, malformed, and current histories. Validation uses a read-only snapshot
and never applies migrations. Operational callers use `fleet_auth::migrate`;
`MIGRATOR` remains available for SQLx test infrastructure.
