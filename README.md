<p align="center">
  <a href="https://trawl.sh"><img src="docs/public/trawl.png" alt="trawl" height="120"></a>
</p>

# trawl

Self-hosted log collection and search for homelabs and small installations.
Trawl keeps logs on one node as Parquet, queries them with DuckDB, and answers
a pipeline query language from the browser, the CLI, or the terminal UI. No
clustering, no multi-tenancy.

## Start here

- [Connect to a server](https://trawl.sh/start/connect/) someone already runs. About 2 minutes.
- [Query local Parquet files](https://trawl.sh/start/local-parquet/) with the CLI alone. About 5 minutes.
- [Install Trawl](https://trawl.sh/getting-started/) and [run your first query](https://trawl.sh/getting-started/first-query/). About 20 minutes.

## Ask a question

```bash
# Which services logged errors in the last hour?
trawl query '_severity>=error last=1h | stats count() as errors by service | sort -errors'

# The same count over local Parquet, with no server.
trawl query --data 'data/**/*.parquet' '* | stats count() by service'
```

[Build a query](https://trawl.sh/use/query-tutorial/) and the
[DSL reference](https://trawl.sh/reference/dsl/) cover the language.

## Components

| Component | Responsibility |
| --- | --- |
| `trawl` | CLI queries, local Parquet queries, and the terminal UI |
| `trawld` | Ingest, storage, queries, and the HTTPS API |
| `trawl-web` | Browser UI and session proxy |
| `fleet-admin` | Fleet migrations, keys, roles, and session keys |
| `trawl-admin` | Self-signed TLS certificate generation |
| PostgreSQL | The Fleet keystore and the Trawl app-state database |

The manual at [trawl.sh](https://trawl.sh) covers
[requirements](https://trawl.sh/start/overview/),
[deployment](https://trawl.sh/operate/deployment/),
[configuration](https://trawl.sh/reference/configuration/), the
[HTTP API](https://trawl.sh/reference/api/), and
[architecture](https://trawl.sh/architecture/overview/).

## Develop

[Local development](https://trawl.sh/getting-started/development/) covers the
prerequisites and the `bin/dev` stack. [Testing](https://trawl.sh/contribute/testing/)
picks the checks for a change. [AGENTS.md](AGENTS.md) links the task skills.

## License

[MPL-2.0](LICENSE)
