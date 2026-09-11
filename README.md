<p align="center">
  <a href="https://trawl.sh"><img src="docs/public/trawl.png" alt="trawl" height="120"></a>
</p>

# trawl

Self-hosted log collection and search for homelabs and small installations.
Use a pipeline query language to investigate events through the browser, CLI,
or terminal UI. Trawl keeps its log corpus on one node in Parquet and queries
it with DuckDB.

## Start here

- [Connect to an existing server](https://trawl.sh/start/connect/).
- [Query local Parquet files](https://trawl.sh/start/local-parquet/) with the CLI alone.
- [Install Trawl](https://trawl.sh/getting-started/) and [run your first query](https://trawl.sh/getting-started/first-query/) against known sample data.

A server installation uses `trawld` plus two PostgreSQL databases, one for the
Fleet keystore and one for Trawl app state and field types. Add `trawl-web` for
browser access. `fleet-admin` manages keys and roles; `trawl-admin` generates TLS
certificates. See [requirements and components](https://trawl.sh/start/overview/).

## Query your logs

```bash
# Count errors by service on your configured server.
trawl query '_severity>=error last=1h | stats count() as errors by service | sort -errors'

# Read local Parquet without a server.
trawl query --data 'data/**/*.parquet' '* | stats count() by service'
```

Follow the [query tutorial](https://trawl.sh/use/query-tutorial/), or use the
[DSL reference](https://trawl.sh/reference/dsl/) for exact syntax and comparison rules.

## Documentation

[trawl.sh](https://trawl.sh) separates setup, investigation, operations, reference,
architecture, and contributor guides.

- [Browser workflows](https://trawl.sh/reference/web-ui/)
- [Deployment and operations](https://trawl.sh/operate/deployment/)
- [Configuration](https://trawl.sh/reference/configuration/)
- [HTTP API](https://trawl.sh/reference/api/)
- [Architecture](https://trawl.sh/architecture/overview/)

## Develop

Read the [local development guide](docs/src/content/docs/getting-started/development.md)
for prerequisites, profiles, and persistent state before starting the stack:

```bash
bin/fleet-dev plan trawl --format human
bin/fleet-dev doctor trawl
bin/dev
```

Agent tests use disposable infrastructure. The [test guide](docs/src/content/docs/contribute/testing.md)
explains focused Rust and browser checks; the
[experiment runbook](scripts/app-experiment/README.md) exercises the real application.
[AGENTS.md](AGENTS.md) links to project task skills.

The Astro documentation site has its own [development and checking guide](docs/src/content/docs/contribute/documentation.md).

## License

[MPL-2.0](LICENSE)
