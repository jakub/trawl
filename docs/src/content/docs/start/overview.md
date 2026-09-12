---
title: Requirements and components
description: Pick the path that fits the data you have and learn what a server installation needs.
---

Pick a starting point from the data you have.

| You have | Start with | You need |
| --- | --- | --- |
| A running Trawl server | [Connect to a server](/start/connect/) | Its URL, an API key, and trust in its TLS certificate |
| Parquet files on disk | [Query local Parquet](/start/local-parquet/) | The `trawl` CLI and read access to the files |
| Logs to collect from now on | [Install Trawl](/getting-started/), then [your first query](/getting-started/first-query/) | Linux, persistent storage, PostgreSQL, and the server packages |
| A change to make to Trawl | [Local development](/getting-started/development/) | Rust, the build tools, and disposable test infrastructure |

## What a server installation runs

`trawld` accepts events, writes them to a write-ahead log and Parquet files,
runs queries with DuckDB in its own process, and serves the HTTPS API.
PostgreSQL holds two databases: the Fleet keystore for keys, roles, and
permissions, and the Trawl app-state database for history, saved queries,
report runs, and the field catalog. `trawl-web` serves the browser UI and
keeps each session in an encrypted cookie in front of `trawld`. It needs the
database and API connections, a public origin, and a session key.

| Component | Responsibility |
| --- | --- |
| `trawl` | CLI queries, local Parquet queries, and the terminal UI |
| `trawld` | Ingest, storage, queries, and the HTTPS API |
| `trawl-web` | Browser UI and session proxy |
| `fleet-admin` | Fleet migrations, keys, roles, and session keys |
| `trawl-admin` | Self-signed TLS certificate generation |
| PostgreSQL | The Fleet keystore and the Trawl app-state database |

## Size a node

Trawl runs on one Linux node with no clustering, sharding, or multi-tenancy.
There is no published hardware minimum, because storage and memory depend on
your ingest rate, retention window, and queries. Start with a representative
sample of your logs and measure with [health and storage](/operate/health/).

Continuous operation needs persistent volumes for the data directory and
PostgreSQL, a [backup and restore](/operate/backup-restore/) plan, and a TLS
certificate your clients trust. See [deployment choices](/operate/deployment/)
and the [process overview](/architecture/overview/).
