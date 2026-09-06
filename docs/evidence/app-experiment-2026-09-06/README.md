# App experiment validation

Tested implementation: `6ec670ccd63c97e257b5e56677123a1e38da8c4f`.
Base: `d6c87a26157bb0e55889b25f45f91c52720d663c`.

The branch adds disposable app experiments, buffers hot-snapshot writes,
batches live-tail notifications, and coordinates hot/cold publication.
The agent quick start and detailed runbook explain creating, testing, and
removing future experiments. ADR-0026 states the publication guarantee.

## Results

- Full workspace: 4,013 tests passed, one skipped. Nextest run
  `a8d56483-2d9b-4e47-abeb-ad9dbeb715bc` used an owned Postgres 18 container.
  Its removal succeeded after the run.
- Postgres admission guard: all 20 database-using targets belong to the
  bounded `postgres` group.
- Normal commit hook: workspace Clippy with default features and warnings
  denied passed. Formatting passed.
- Generator and oracle tests: four passed. They reject loss, duplication,
  changed values, truncation, and a short browser page.
- Real lifecycle tests: two passed. A port collision preserved the existing
  listener. SIGTERM during a verified hold removed the instance resources.
- Full app: seed 49, 20,000 events, batch size 1,000, target rate 2,000/s.
  Every event was accepted, with zero rejections or ambiguous batches.
  Exact IDs and status values match after ingestion, compaction, and restart.
  Browser live tail verified all 19,000 post-subscription events. Browser
  search and the session after restart passed. All cleanup flags are true.

[The retained report](run-seed-49.json) contains phase hashes, counts,
measurements, and source/binary identities. Local paths and process IDs are
omitted; its `evidence` field records the original report hash and omissions.
Raw traces and logs remain in the private local evidence archive.

## Commands

Cargo commands used the same disk-backed `CARGO_TARGET_DIR`. The workspace
suite received `DATABASE_URL` for its owned, temporary Postgres container,
with no `FLEET_DATABASE_URL` override.

```bash
cargo run -p fleet-admin -- migrate
cargo nextest run --workspace --no-default-features \
  --features fleet-auth/keystore,fleet-auth/axum,fleet-auth/fast-hash \
  --test-threads 4 --no-fail-fast
cargo xtask pg-admission-guard -- --no-default-features \
  --features fleet-auth/keystore,fleet-auth/axum,fleet-auth/fast-hash
node --test scripts/app-experiment/workload.test.mjs
bin/app-experiment --seed 49 --events 20000 --batch-size 1000 --rate 2000
node scripts/app-experiment/lifecycle.test.mjs
```

The first full workspace run exposed a fixture calling the synchronous
ingestion writer from an async runtime thread. The fixture now awaits
`spawn_blocking`, as production syslog ingestion does. The complete rerun
passed. Assertions were preserved.

These measurements use a debug daemon and release SPA. The paced workload
checks correctness at the requested rate; it does not establish maximum
throughput or steady-state query latency. The ordinary app scenario disables
daily rollup. Deterministic Rust tests separately cover publication waits,
recovery failures, cancellation, and repin admission.
