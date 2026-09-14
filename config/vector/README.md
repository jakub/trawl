# Vector collector examples

[`debian/`](debian/) collects Linux host and application logs and forwards them
to Trawl's HTTP ingest endpoint. Its base configuration explains source
selection, certificate trust, and the optional noise filter. Follow the
[Vector integration guide](https://trawl.sh/getting-started/vector-integration/)
for installation and configuration-directory selection.

For a first local dataset, use
[Your first query](https://trawl.sh/getting-started/first-query/). It provisions
an owned server, ingests three known events, and exports Parquet. The CLI reads
Parquet directly; an NDJSON file produced by a collector is not a local-query
dataset.

A macOS client can [connect to a Linux server](https://trawl.sh/start/connect/)
or [query a Parquet export](https://trawl.sh/start/local-parquet/). The client
does not need a local collector or daemon for either path.
