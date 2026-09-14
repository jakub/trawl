# CLI release fixture

`cli.parquet` is a fixed, uncompressed Parquet file with three synthetic log
rows. The release smoke checks exact values, a filtered count, export, and
reopening the export. It needs no server, credentials, or running collector.

SHA-256: `becbc3b5398da1c7065b34789d1ae8bd66f14bf31c52e2914cd77cf65ecbc22f`

The fixture was created with official DuckDB 1.5.5 using this SQL:

```sql
COPY (
  SELECT * FROM (VALUES
    (1, 'launch', 'ready', 10),
    (2, 'launch', 'request failed', 30),
    (3, 'worker', 'complete', 20)
  ) AS events(id, service, message, duration_ms)
) TO 'cli.parquet' (FORMAT PARQUET, COMPRESSION UNCOMPRESSED);
```

To check an extracted package, run the helper from the release tooling checkout:

```bash
python3 scripts/release/smoke-cli.py /path/to/package/bin/trawl \
  scripts/release/fixtures/cli.parquet --expected-version 0.4.0
```

Use the selected artifact's actual version. The helper gives the CLI an empty
config, a fresh home, and a minimal environment without loader settings. It
also rejects extra files in that home, including a downloaded extension cache.
An artifact must pass after relocation as well as in its original extraction
location. The separate library probe verifies built-in extension and IANA
timezone behavior with extension downloads disabled.
