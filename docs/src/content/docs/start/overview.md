---
title: Requirements and components
description: Choose an existing server, local Parquet, or a new single-node installation.
---

Choose a starting point based on the data you have.

| You have | Start with | What you need |
| --- | --- | --- |
| An existing Trawl installation | [Connect to a server](/start/connect/) | Browser URL or API URL, an assigned key, and the server's TLS trust information |
| Parquet files on disk | [Query local Parquet](/start/local-parquet/) | The `trawl` CLI and permission to read the files |
| Logs to collect continuously | [Install Trawl](/getting-started/), then [your first query](/getting-started/first-query/) | Linux, persistent storage, PostgreSQL, and the server and administration binaries |
| A change to the source | [Local development](/getting-started/development/) | Rust, the relevant build tools, and disposable infrastructure for tests |

## A server installation

`trawld` accepts events, writes the log corpus, runs queries, and serves the HTTPS
API. It uses DuckDB in the process. It does not need a separate DuckDB server.
Logs live in a write-ahead log and Parquet files on the data volume.

PostgreSQL holds two distinct databases. The Fleet keystore stores keys, roles,
and permissions. The Trawl app-state database stores history, saved queries,
report state, and the field catalog. Both may share one PostgreSQL server.

`trawl-web` adds browser access. It serves the compiled UI and manages cookie
sessions in front of `trawld`. Browser access needs a correctly configured public
origin and session key, as well as the database and API connections.

| Program | Responsibility |
| --- | --- |
| `trawl` | CLI queries, embedded Parquet queries, and the terminal UI |
| `trawld` | Ingest, storage, queries, and the HTTPS API |
| `trawl-web` | Browser UI and session proxy |
| `fleet-admin` | Fleet migrations, keys, roles, and session-key administration |
| `trawl-admin` | TLS certificate generation |

## Size and boundaries

Trawl targets homelabs and small installations. One node owns the log corpus;
there is no clustering, sharding, or multi-tenancy. Extra query capacity does not
turn the storage design into a distributed system.

Choose storage and memory against your own ingest rate, retention window, and
queries. An event count alone cannot predict storage size or query latency.
Start with a representative sample and measure [health and storage](/operate/health/).
There is no universal hardware minimum established by a supported benchmark.

For continuous operation, provide persistent data and database volumes, a plan
for [backup and restore](/operate/backup-restore/), and trusted TLS for clients.
See [deployment choices](/operate/deployment/) and the
[process overview](/architecture/overview/) for the next level of detail.
